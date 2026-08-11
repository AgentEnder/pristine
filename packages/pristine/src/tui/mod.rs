//! The rollup tree TUI: the front end the whole tool is for.
//!
//! # What it is
//!
//! The filesystem tree, pruned to paths that lead to something reclaimable, with every
//! ancestor carrying the bytes recoverable *beneath* it — collapsed by default and drilled
//! into on demand. A row's number is not "how big is this directory" but "how much do I get
//! back by emptying this subtree", so `~/repos/archived` is one row worth 118 GB rather than
//! forty rows a reader has to recognise as related.
//!
//! That is the thing neither reference implementation has. kondo's own README calls it
//! "essentially `rm -rf` with a prompt", which is a decision per hit, and hits are what scale:
//! one real home directory here holds 16,013 of them. npkill is better and still flat, so the
//! row a reader actually wants does not exist and has to be assembled from forty selections;
//! its range-select is npkill approximating a tree without having one.
//!
//! # Three moving parts, and the channel between them
//!
//! - The **walker** runs on its own thread and reports [`Found`] events. Rows appear as it
//!   finds them, which is npkill's good idea and is *easier* on a tree: a new claim updates
//!   ancestor totals in place instead of reordering a flat list.
//! - The **view** ([`state::View`]) holds everything a keystroke can change. It never touches
//!   the terminal or the filesystem, so every rule it has is a unit test.
//! - The **deleter** runs a marked batch on a pool and reports each target as it finishes,
//!   which is why the cursor is anchored to a path: rows vanish under it.
//!
//! All three meet on one channel, drained once per frame. The event loop blocks on the
//! terminal with a short timeout rather than on the channel, so a scan that finds nothing for
//! a second still repaints and a keystroke is never waiting behind a walk.
//!
//! # The TUI prices what it shows, and the CLI does not
//!
//! A default scan leaves claims [`crate::Size::Unmeasured`], because pricing one means enumerating
//! the subtree the walk deliberately pruned at — 4.6 s against 55.8 s over one real `~/repos`.
//! That is right for a listing you read once and wrong for a tree you steer by: unpriced, the
//! rollup has nothing to roll up, and the headline question has no answer at any depth.
//!
//! So the TUI turns the breakdown on unless the command line has scoped it. It can afford to,
//! and #618 is why: prices are computed on a pool and arrive as separate events, so the rows
//! are on screen at 7.5 s while the numbers fill in behind them for the following minute. The
//! reader marks and deletes throughout. `--breakdown-under <PATH>` still means what it says,
//! for a reader who wants one subtree priced and the rest left alone.

pub mod keymap;
pub mod render;
pub mod state;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::Duration;

use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::{Terminal, TerminalOptions, Viewport, backend::CrosstermBackend};

use crate::delete::{Deleter, Planner, Removal, Target};
use crate::size::{SizeMode, human};
use crate::tree::Tree;
use crate::walk::{Found, WalkOutcome, Walker};
use crate::{Ruleset, WalkError};
use keymap::action_for;
use state::{Effect, Pending, View, plural};

/// How long the loop waits on the terminal before repainting anyway.
///
/// The frame rate while something is arriving, and nothing at all while the reader is
/// thinking: `event::poll` returns the moment a key is pressed, so this only bounds how stale
/// a *number* can be, not how long a keystroke waits.
const TICK: Duration = Duration::from_millis(100);

/// Everything the front end needs from the command line.
#[derive(Debug, Clone)]
pub struct Options {
    /// The directory to scan.
    pub root: PathBuf,
    /// The size floor for the fallback tier.
    pub min_size: u64,
    /// How hard to work for each claim's size. See the module docs for why the default here
    /// is not the default for the listing.
    pub size_mode: SizeMode,
    /// Whether to stay on one filesystem — for the walk *and* for the planner, which is a
    /// safety property rather than a tidiness one.
    pub one_file_system: bool,
    /// Keep anything touched more recently than this.
    pub older_than: Option<Duration>,
}

/// What one arriving event tells the view.
enum Message {
    Found(Found),
    Scanned(WalkOutcome),
    Removed { path: PathBuf, complete: bool },
    Deleted(Box<Removal>),
}

/// Runs the view until the reader quits, then puts the terminal back.
///
/// # Errors
///
/// Anything the terminal refuses. A failure to *restore* it is reported even when the run
/// itself succeeded, because a terminal left in raw mode is the one outcome a user cannot
/// ignore and cannot easily undo.
pub fn run(options: &Options, ruleset: Arc<Ruleset>) -> io::Result<()> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let terminal = Terminal::with_options(
        CrosstermBackend::new(out),
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    );

    let outcome = terminal.and_then(|mut terminal| {
        let result = drive(&mut terminal, options, ruleset);
        // Cursor position is the terminal's business and the alternate screen swallowed it.
        terminal.show_cursor().and(result)
    });

    // Restoring runs whatever happened above, because the alternative is handing back a
    // terminal with no echo and no line editing.
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    outcome
}

/// The event loop, with the terminal already set up.
fn drive<B: ratatui::backend::Backend<Error = io::Error>>(
    terminal: &mut Terminal<B>,
    options: &Options,
    ruleset: Arc<Ruleset>,
) -> io::Result<()> {
    let (post, inbox) = channel();
    let mut view = View::new(Tree::new(&options.root));

    let walker = spawn_walk(options, ruleset, post.clone());
    let mut errors: Vec<WalkError> = Vec::new();

    loop {
        drain(&mut view, &inbox, &mut errors);
        view.sync();
        terminal.draw(|frame| render::draw(frame, &mut view, &errors))?;

        if !event::poll(TICK)? {
            continue;
        }
        let event = event::read()?;
        // A resize is not a keystroke and produces no action; the redraw above is the whole
        // of what it needs.
        if matches!(event, Event::Resize(..)) {
            continue;
        }
        match view.apply(action_for(&event, view.overlay())) {
            Effect::None => {}
            Effect::Quit => break,
            Effect::Plan(batch) => {
                let plan = Planner::new(&options.root)
                    .one_file_system(options.one_file_system)
                    .older_than(options.older_than)
                    .plan(batch);
                view.ask(Pending::of(&plan));
            }
            Effect::Delete(targets) => spawn_delete(options, targets, post.clone()),
        }
    }

    // The walk holds a `Sender`; dropping ours and letting the thread finish is what stops a
    // half-written frame from being the last thing on the screen. The thread is not joined:
    // a walk of a home directory can be seconds from finishing, and a reader who pressed `q`
    // has said they are done waiting.
    drop(walker);
    Ok(())
}

/// Empties the channel into the view.
///
/// Everything on it, not one message: a breakdown reports 16,013 claims and as many prices,
/// and taking one per frame would render a tree that fills up over four minutes.
fn drain(view: &mut View, inbox: &Receiver<Message>, errors: &mut Vec<WalkError>) {
    loop {
        match inbox.try_recv() {
            Ok(Message::Found(Found::Claim(hit))) => view.found(hit),
            Ok(Message::Found(Found::Priced(priced))) => view.priced(&priced.path, priced.size),
            Ok(Message::Scanned(outcome)) => {
                errors.extend(outcome.errors);
                view.scanned();
            }
            Ok(Message::Removed { path, complete }) => view.removed(&path, complete),
            Ok(Message::Deleted(removal)) => view.deleted(summarise(&removal)),
            // Disconnected as well as empty: the walk finishing drops its sender, and there
            // is nothing left to say either way.
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
        }
    }
}

/// Starts the walk. The returned handle is only held so its `Sender` outlives the loop.
fn spawn_walk(
    options: &Options,
    ruleset: Arc<Ruleset>,
    post: Sender<Message>,
) -> std::thread::JoinHandle<()> {
    let walker = Walker::new(&options.root, ruleset)
        .size_mode(options.size_mode.clone())
        .same_file_system(options.one_file_system)
        .min_size(options.min_size);
    std::thread::spawn(move || {
        let reporting = post.clone();
        let outcome = walker.run(move |found| {
            // A closed channel means the reader has quit. Nothing to do about it here, and
            // the walk stops on its own when it finishes.
            let _ = reporting.send(Message::Found(found));
        });
        let _ = post.send(Message::Scanned(outcome));
    })
}

/// Starts a removal, reporting each target as it finishes and the whole thing at the end.
fn spawn_delete(options: &Options, targets: Vec<PathBuf>, post: Sender<Message>) {
    let planner = Planner::new(&options.root)
        .one_file_system(options.one_file_system)
        .older_than(options.older_than);
    std::thread::spawn(move || {
        // Re-planned rather than the plan the question was asked about, and the reason is
        // #595's: a plan is a set of `stat`s taken at a moment, and the moment has passed.
        // The targets are the ones the reader confirmed, so nothing new can enter the batch —
        // this can only refuse more, never less.
        let plan = planner.plan(targets.iter().map(Target::at));
        let reporting = post.clone();
        let removal = Deleter::new()
            .watching(move |removed| {
                let _ = reporting.send(Message::Removed {
                    path: removed.path.clone(),
                    complete: removed.complete,
                });
            })
            .remove(&plan);
        let _ = post.send(Message::Deleted(Box::new(removal)));
    });
}

/// One line about what a removal did, for the footer.
///
/// Failures and refusals are named as counts rather than swallowed: a batch where half the
/// targets were left standing has to say so, and the rows are still there to be looked at.
fn summarise(removal: &Removal) -> String {
    let mut said = vec![format!(
        "removed {} from {}",
        human(removal.bytes_freed()),
        plural(removal.removed.len(), "directory", "directories")
    )];
    if !removal.kept.is_empty() {
        said.push(format!(
            "{} left alone",
            plural(removal.kept.len(), "directory", "directories")
        ));
    }
    if !removal.failures.is_empty() {
        said.push(format!(
            "{} failed",
            plural(removal.failures.len(), "directory", "directories")
        ));
    }
    said.join(", ")
}

/// The size mode a live view runs under.
///
/// A scoped breakdown is honoured as given — that flag is a request to price one subtree and
/// nothing else. Everything else becomes a full breakdown, including the `Skip` that is the
/// listing's default: see the module docs for why a tree of dashes is not a front end.
#[must_use]
pub fn size_mode(asked: SizeMode) -> SizeMode {
    match asked {
        SizeMode::BreakdownUnder(scope) => SizeMode::BreakdownUnder(scope),
        SizeMode::Skip | SizeMode::Breakdown => SizeMode::Breakdown,
    }
}
