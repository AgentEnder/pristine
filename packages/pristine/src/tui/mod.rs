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
use std::thread::JoinHandle;
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

/// What the run turns out to have been, for the exit status.
///
/// A live view says everything it knows on the screen — an unreadable path in the header, a
/// failed removal in the footer — and none of that reaches a script. The listing's rule is
/// that a run which could not do everything it was asked exits non-zero, and there is no
/// reason for the tree to be the exception: the same person pipes the same tool into the same
/// `&&` the next day.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Paths the walk could not read, so every total is a lower bound. The header says so.
    pub errors: Vec<WalkError>,
    /// Targets a removal could not finish. **Not** the ones it refused: a refusal is the
    /// safety model working, which is exactly the distinction [`Removal::is_clean`] draws.
    pub failures: usize,
}

impl Outcome {
    /// Whether everything the run was asked to do actually happened.
    #[must_use]
    pub fn whole(&self) -> bool {
        self.errors.is_empty() && self.failures == 0
    }
}

/// The terminal states this took, and the undoing of each.
///
/// A guard rather than a pair of calls at the end, because the two things that go wrong are
/// both invisible until somebody is left with a broken terminal: an *early* failure between
/// the two setup steps skips a cleanup that had not been written yet, and a `?` on the first
/// cleanup step skips the second. Each flag is set the moment its state is genuinely entered,
/// so [`Restore::finish`] and [`Drop`] both undo exactly what happened and nothing else.
#[derive(Debug, Default)]
struct Restore {
    raw: bool,
    alternate: bool,
}

impl Restore {
    /// Puts back what was taken, attempting **every** step and reporting the first refusal.
    ///
    /// `?` between the steps is the bug this replaces: a terminal that is out of raw mode and
    /// still on the alternate screen, or the other way round, is no better than one that was
    /// never restored, and the failing call says nothing about whether the next one would.
    fn finish(&mut self) -> io::Result<()> {
        let mut first = Ok(());
        if std::mem::take(&mut self.raw) {
            first = first.and(disable_raw_mode());
        }
        if std::mem::take(&mut self.alternate) {
            first = first.and(execute!(io::stdout(), LeaveAlternateScreen));
        }
        first
    }
}

impl Drop for Restore {
    /// The path a `?` takes, and the one a panic takes. Nothing here can report a failure, so
    /// nothing here tries — [`Restore::finish`] is what the ordinary return calls.
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// Runs the view until the reader quits, then puts the terminal back.
///
/// # Errors
///
/// Anything the terminal refuses. A failure to *restore* it is reported even when the run
/// itself succeeded, because a terminal left in raw mode is the one outcome a user cannot
/// ignore and cannot easily undo.
pub fn run(options: &Options, ruleset: Arc<Ruleset>) -> io::Result<Outcome> {
    let mut restore = Restore::default();
    enable_raw_mode()?;
    restore.raw = true;
    execute!(io::stdout(), EnterAlternateScreen)?;
    restore.alternate = true;

    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )?;
    let outcome = drive(&mut terminal, options, ruleset);
    // Cursor position is the terminal's business and the alternate screen swallowed it.
    let shown = terminal.show_cursor();
    let restored = restore.finish();
    outcome.and_then(|outcome| shown.and(restored).map(|()| outcome))
}

/// The event loop, with the terminal already set up.
fn drive<B: ratatui::backend::Backend<Error = io::Error>>(
    terminal: &mut Terminal<B>,
    options: &Options,
    ruleset: Arc<Ruleset>,
) -> io::Result<Outcome> {
    let (post, inbox) = channel();
    let mut view = View::new(Tree::new(&options.root));

    let walker = spawn_walk(options, ruleset, post.clone());
    let mut outcome = Outcome::default();
    let mut deleter: Option<JoinHandle<()>> = None;

    loop {
        drain(&mut view, &inbox, &mut outcome);
        reap(&mut view, &inbox, &mut outcome, deleter.as_ref());
        view.sync();
        terminal.draw(|frame| render::draw(frame, &mut view, &outcome.errors))?;

        // A quit that was held back while a removal ran, now that it is over. Checked here
        // rather than where the key was pressed, because what the key produced was a promise
        // to leave and this is the first frame on which it can be kept.
        if view.wants_to_quit() {
            break;
        }
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
            // Only ever reached with nothing in flight: the view holds a quit back while it
            // is deleting, and hands it over through `wants_to_quit` instead.
            Effect::Quit => break,
            Effect::Plan(batch) => {
                let plan = Planner::new(&options.root)
                    .one_file_system(options.one_file_system)
                    .older_than(options.older_than)
                    .plan(batch);
                view.ask(Pending::of(&plan));
            }
            Effect::Delete(targets) => deleter = Some(spawn_delete(options, targets, post.clone())),
        }
    }

    // The removal is over by the time the loop breaks — that is what the view guarantees —
    // but the thread that ran it has not necessarily been reaped, and leaving `main` would
    // take it with us either way. Joining is what makes "the batch finished" true rather than
    // merely likely.
    if let Some(deleter) = deleter {
        let _ = deleter.join();
    }
    // The walk holds a `Sender`; dropping ours and letting the thread finish is what stops a
    // half-written frame from being the last thing on the screen. It is deliberately NOT
    // joined, where the removal is: a walk reads, so abandoning one costs nothing, and a
    // reader who pressed `q` during a scan of a home directory has said they are done
    // waiting.
    drop(walker);
    Ok(outcome)
}

/// Notices a removal that ended without saying what it did.
///
/// Nothing in the deleter should panic, and if one ever does the cost is not the panic: the
/// view would sit `is_deleting()` forever, and a view that will not quit is worse than the
/// thing that made it. The thread posts its report *before* it finishes, so a finished thread
/// with nothing on the channel really has gone without reporting.
fn reap(
    view: &mut View,
    inbox: &Receiver<Message>,
    outcome: &mut Outcome,
    deleter: Option<&JoinHandle<()>>,
) {
    if !view.is_deleting() || !deleter.is_some_and(JoinHandle::is_finished) {
        return;
    }
    drain(view, inbox, outcome);
    if view.is_deleting() {
        outcome.failures += 1;
        view.deleted("the removal ended without reporting what it did".to_owned());
    }
}

/// Empties the channel into the view.
///
/// Everything on it, not one message: a breakdown reports 16,013 claims and as many prices,
/// and taking one per frame would render a tree that fills up over four minutes.
fn drain(view: &mut View, inbox: &Receiver<Message>, outcome: &mut Outcome) {
    loop {
        match inbox.try_recv() {
            Ok(Message::Found(Found::Claim(hit))) => view.found(hit),
            Ok(Message::Found(Found::Priced(priced))) => view.priced(&priced.path, priced.size),
            Ok(Message::Scanned(walk)) => {
                outcome.errors.extend(walk.errors);
                view.scanned();
            }
            Ok(Message::Removed { path, complete }) => view.removed(&path, complete),
            Ok(Message::Deleted(removal)) => {
                outcome.failures += removal.failures.len();
                view.deleted(summarise(&removal));
            }
            // Disconnected as well as empty: the walk finishing drops its sender, and there
            // is nothing left to say either way.
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
        }
    }
}

/// Starts the walk. The returned handle is only held so its `Sender` outlives the loop.
fn spawn_walk(options: &Options, ruleset: Arc<Ruleset>, post: Sender<Message>) -> JoinHandle<()> {
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
///
/// The handle is kept by the loop, which will not leave until this thread is done: see
/// [`View::wants_to_quit`].
fn spawn_delete(options: &Options, targets: Vec<PathBuf>, post: Sender<Message>) -> JoinHandle<()> {
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
        // Posted before the thread ends, which is what lets `reap` read a finished thread
        // with an empty channel as "this one died without saying anything".
        let _ = post.send(Message::Deleted(Box::new(removal)));
    })
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

#[cfg(test)]
mod tests {
    use super::{Message, Outcome, Restore, drain, reap, summarise};
    use crate::delete::{Failure, Refusal, Refused, Removal, Removed};
    use crate::fixture::priced;
    use crate::tree::Tree;
    use crate::tui::state::View;
    use crate::walk::{Found, WalkError, WalkOutcome};
    use std::sync::mpsc::channel;

    fn view() -> View {
        View::new(Tree::new("/scan"))
    }

    fn removed(path: &str) -> Removed {
        Removed {
            path: path.into(),
            bytes: 1024,
            entries: 3,
            complete: true,
        }
    }

    #[test]
    fn a_scan_that_could_not_read_a_path_is_not_a_whole_run() {
        let (post, inbox) = channel();
        let mut view = view();
        let mut outcome = Outcome::default();
        assert!(outcome.whole());

        post.send(Message::Found(Found::Claim(priced("/scan/a/target", 8))))
            .unwrap();
        post.send(Message::Scanned(WalkOutcome {
            errors: vec![WalkError {
                path: Some("/scan/locked".into()),
                message: "Permission denied".to_owned(),
            }],
            ..WalkOutcome::default()
        }))
        .unwrap();
        drain(&mut view, &inbox, &mut outcome);

        // The header says so on screen, and this is the same fact reaching a script. Tested
        // through the real drain rather than through a terminal, because the terminal is the
        // one part of this that a test cannot have.
        assert_eq!(outcome.errors.len(), 1);
        assert!(!outcome.whole());
        assert!(!view.is_scanning());
    }

    #[test]
    fn a_removal_that_failed_is_not_a_whole_run_and_a_removal_that_was_refused_is() {
        let (post, inbox) = channel();
        let mut view = view();
        let mut outcome = Outcome::default();

        // A refusal is the safety model working, which is exactly the distinction
        // `Removal::is_clean` draws — and the listing exits zero on one.
        post.send(Message::Deleted(Box::new(Removal {
            removed: vec![removed("/scan/a/target")],
            kept: vec![Refused {
                path: "/scan/b/node_modules".into(),
                reason: Refusal::HoldsCheckout,
            }],
            failures: Vec::new(),
        })))
        .unwrap();
        drain(&mut view, &inbox, &mut outcome);
        assert!(outcome.whole());
        assert!(view.notice().unwrap().contains("left alone"));

        post.send(Message::Deleted(Box::new(Removal {
            removed: Vec::new(),
            kept: Vec::new(),
            failures: vec![Failure {
                path: "/scan/c/target".into(),
                message: "Device or resource busy".to_owned(),
            }],
        })))
        .unwrap();
        drain(&mut view, &inbox, &mut outcome);
        assert_eq!(outcome.failures, 1);
        assert!(!outcome.whole());
    }

    #[test]
    fn a_removal_that_ended_without_reporting_does_not_leave_the_view_unable_to_quit() {
        let (post, inbox) = channel();
        let mut view = view();
        let mut outcome = Outcome::default();
        // A thread that has already ended, having said nothing — which is what a panic on
        // the pool would look like from here.
        let dead = std::thread::spawn(|| {});
        while !dead.is_finished() {
            std::thread::yield_now();
        }
        view.deleting_for_test();

        reap(&mut view, &inbox, &mut outcome, Some(&dead));

        assert!(!view.is_deleting(), "the view would never quit again");
        assert_eq!(outcome.failures, 1);
        drop(post);
    }

    #[test]
    fn a_removal_that_reported_on_its_way_out_is_read_rather_than_called_a_failure() {
        let (post, inbox) = channel();
        let mut view = view();
        let mut outcome = Outcome::default();
        let dead = std::thread::spawn(|| {});
        while !dead.is_finished() {
            std::thread::yield_now();
        }
        view.deleting_for_test();
        // The report is posted before the thread ends, so a finished thread can still have
        // something on the channel. Calling that a failure would fail every clean removal.
        post.send(Message::Deleted(Box::default())).unwrap();

        reap(&mut view, &inbox, &mut outcome, Some(&dead));

        assert!(!view.is_deleting());
        assert_eq!(outcome.failures, 0);
        assert!(outcome.whole());
    }

    #[test]
    fn a_summary_names_what_was_left_behind_as_well_as_what_went() {
        let removal = Removal {
            removed: vec![removed("/scan/a/target")],
            kept: vec![Refused {
                path: "/scan/b".into(),
                reason: Refusal::HoldsCheckout,
            }],
            failures: vec![Failure {
                path: "/scan/c".into(),
                message: "busy".to_owned(),
            }],
        };
        let said = summarise(&removal);
        assert!(said.contains("removed 1.0 KiB from 1 directory"), "{said}");
        assert!(said.contains("1 directory left alone"), "{said}");
        assert!(said.contains("1 directory failed"), "{said}");
    }

    #[test]
    fn restoring_the_terminal_attempts_every_step_that_was_reached() {
        // The states are global to the process, so what is asserted here is the bookkeeping:
        // `finish` undoes exactly what was entered, and having run it, `Drop` has nothing
        // left to do. That is the whole of what the early-failure path depends on — a flag
        // still set is a step Drop would take a second time, and a flag never set is a step
        // neither of them would take at all.
        //
        // It really does call `disable_raw_mode` and leave the alternate screen, on a process
        // that is in neither state. Both are no-ops there, and the escape sequence goes to
        // the harness's captured stdout.
        let mut restore = Restore::default();
        assert!(restore.finish().is_ok(), "nothing was taken");

        restore.raw = true;
        restore.alternate = true;
        assert!(restore.finish().is_ok());
        assert!(!restore.raw, "raw mode would be disabled twice");
        assert!(
            !restore.alternate,
            "the alternate screen would be left twice"
        );
    }
}
