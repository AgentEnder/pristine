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

pub mod chrome;
pub mod keymap;
pub mod moving;
pub mod render;
pub mod state;

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::{Terminal, TerminalOptions, Viewport, backend::CrosstermBackend};

use crate::delete::{Deleter, Planner, Removal, Step, Target};
use crate::size::{SizeMode, human};
use crate::tree::Tree;
use crate::walk::{Found, WalkOutcome, Walker};
use crate::{Ruleset, WalkError};
use chrome::{Chrome, Decor, Status};
use keymap::action_for;
use state::{Effect, Pending, View, plural};

/// How long the loop waits on the terminal before repainting anyway.
///
/// The frame rate while something is arriving, and nothing at all while the reader is
/// thinking: `event::poll` returns the moment a key is pressed, so this only bounds how stale
/// a *number* can be, not how long a keystroke waits.
const TICK: Duration = Duration::from_millis(100);

/// The same, while something on screen is actually moving.
///
/// Ten frames a second is enough to keep a number from going stale and not enough to make one
/// climb smoothly, so the loop draws faster exactly when there is something to see and drops
/// straight back to [`TICK`] when there is not — which is most of the time a reader spends in
/// here, deciding. A frame is 1.5 ms in release against this budget, so the difference is a
/// few percent of one core during a scan and nothing at all while one is being read.
const FRAME: Duration = Duration::from_millis(33);

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
    /// A removal reporting on itself as it happens: see [`crate::delete::Step`]. Carried
    /// through rather than flattened, because the two halves land on the view differently —
    /// bytes as they leave, the row when the sweep is finished with it.
    Removing(Step),
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
    /// What the session actually got back, across every batch it ran.
    ///
    /// Not part of the exit status — a run that freed nothing is a perfectly whole run — but
    /// it is the number the reader who walked away came back for, so it is what the window
    /// title says once there is one. See [`Status::Freed`].
    pub freed: u64,
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
///
/// The [`Chrome`] is a **field** rather than a second guard beside this one, and that is the
/// point of it being here: a window title, a taskbar bar and an open synchronized update are
/// three more things a `?` can walk away from, and three guards that each undo a third of the
/// terminal are three chances to miss one. One owner, one order — innermost first.
#[derive(Debug)]
struct Restore<W: Write> {
    raw: bool,
    alternate: bool,
    chrome: Chrome<W>,
}

impl<W: Write> Restore<W> {
    /// A guard that has taken nothing yet, holding the decorations it will hand back.
    fn new(chrome: Chrome<W>) -> Self {
        Self {
            raw: false,
            alternate: false,
            chrome,
        }
    }

    /// Puts back what was taken, attempting **every** step and reporting the first refusal.
    ///
    /// `?` between the steps is the bug this replaces: a terminal that is out of raw mode and
    /// still on the alternate screen, or the other way round, is no better than one that was
    /// never restored, and the failing call says nothing about whether the next one would.
    fn finish(&mut self) -> io::Result<()> {
        // The decorations first: they were taken last, and the one that matters most —
        // closing a synchronized update — is the difference between a terminal that repaints
        // and one that shows a frozen frame forever.
        let mut first = self.chrome.restore();
        if std::mem::take(&mut self.raw) {
            first = first.and(disable_raw_mode());
        }
        if std::mem::take(&mut self.alternate) {
            first = first.and(execute!(io::stdout(), LeaveAlternateScreen));
        }
        first
    }
}

impl<W: Write> Drop for Restore<W> {
    /// The path a `?` takes, and the one a panic takes. Nothing here can report a failure, so
    /// nothing here tries — [`Restore::finish`] is what the ordinary return calls.
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// The removal thread, joined however the loop ends.
///
/// A guard rather than a `join` at the bottom of the loop, and the difference is every exit
/// that is not the bottom of the loop: a draw that fails, a terminal that stops answering, a
/// panic. Each of those returns through a `?` that no amount of care at the end can catch, and
/// what it abandons is a thread half way through unlinking a directory tree the reader
/// confirmed. Leaving `main` then kills it mid-batch — the same hazard `q` had, reached by a
/// path nobody presses.
///
/// So there is no bare [`JoinHandle`] anywhere in the loop. Owning one *is* the guarantee.
#[derive(Default)]
struct Batch(Option<JoinHandle<()>>);

impl Batch {
    /// Takes over from whatever ran before, which the view guarantees has finished — joined
    /// rather than dropped anyway, because dropping a handle detaches it and detaching is the
    /// whole bug.
    fn takes_over(&mut self, removal: JoinHandle<()>) {
        self.join();
        self.0 = Some(removal);
    }

    /// Waits for the removal, if there is one. A failure to join is a thread that panicked,
    /// which the loop already learns about from `reap` — there is nothing to do here but stop
    /// waiting.
    fn join(&mut self) {
        if let Some(removal) = self.0.take() {
            let _ = removal.join();
        }
    }
}

impl Drop for Batch {
    fn drop(&mut self) {
        self.join();
    }
}

/// Where keystrokes come from.
///
/// A seam with exactly one implementation in the shipped binary, and it exists so the event
/// loop can be tested at all: the guarantees worth having here are about what happens when a
/// *terminal* fails half way through a removal, and a test cannot press a key on a real one.
trait Events {
    /// Whether an event is waiting, within `timeout`.
    fn poll(&mut self, timeout: Duration) -> io::Result<bool>;

    /// The next event. Only called once [`Events::poll`] has said there is one.
    fn read(&mut self) -> io::Result<Event>;
}

/// The real terminal.
struct Keyboard;

impl Events for Keyboard {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        event::poll(timeout)
    }

    fn read(&mut self) -> io::Result<Event> {
        event::read()
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
    // Detected once, from the environment and a `is_terminal`, and never probed for: see
    // [`chrome`] for why nothing here asks the terminal a question it has to wait for.
    let mut restore = Restore::new(Chrome::new(io::stdout(), Decor::detect()));
    enable_raw_mode()?;
    restore.raw = true;
    execute!(io::stdout(), EnterAlternateScreen)?;
    restore.alternate = true;
    restore.chrome.enter()?;

    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )?;
    let outcome = drive(
        &mut terminal,
        &mut Keyboard,
        &mut restore.chrome,
        options,
        ruleset,
    );
    // The taskbar says how the run ended as well as how far it got. The reader who wanted a
    // bar is by definition the one who is not watching the exit status go past.
    if !outcome.as_ref().is_ok_and(Outcome::whole) {
        restore.chrome.failed();
    }
    // Cursor position is the terminal's business and the alternate screen swallowed it.
    let shown = terminal.show_cursor();
    let restored = restore.finish();
    outcome.and_then(|outcome| shown.and(restored).map(|()| outcome))
}

/// The event loop, with the terminal already set up.
fn drive<B: ratatui::backend::Backend<Error = io::Error>, W: Write>(
    terminal: &mut Terminal<B>,
    events: &mut dyn Events,
    chrome: &mut Chrome<W>,
    options: &Options,
    ruleset: Arc<Ruleset>,
) -> io::Result<Outcome> {
    let (post, inbox) = channel();
    let mut view = View::new(Tree::new(&options.root));

    let walker = spawn_walk(options, ruleset, post.clone());
    let mut outcome = Outcome::default();
    // Dropped at the end of this function however it ends — the bottom of the loop, a `?`, or
    // a panic — and dropping it waits for the removal. See [`Batch`].
    let mut batch = Batch::default();
    // Two clocks rather than one, because either phase can finish while the other is running:
    // a reader can mark and delete a subtree the moment it appears, long before the walk is
    // over, and a scan timed from the start of the removal it overlapped would be a scan
    // reported as having taken seconds.
    let scan_since = Instant::now();
    let mut batch_since = Instant::now();
    let mut scanning = view.is_scanning();
    let mut deleting = view.is_deleting();

    loop {
        drain(&mut view, &inbox, &mut outcome);
        reap(&mut view, &inbox, &mut outcome, batch.0.as_ref());
        // The one place a clock enters the view, and it syncs on the way through. Every
        // interpolated number on screen moves exactly once per frame, here.
        view.animate(Instant::now());

        // A phase that has just ended is the only thing worth interrupting anybody for, and
        // the chrome decides whether it is: long enough to matter, and nobody watching. Both
        // of these mirror the view rather than being told separately when a phase begins —
        // two records of one fact is how a notification ends up describing a removal that is
        // still running.
        if scanning && !view.is_scanning() {
            chrome.announce(&scanned(&view), scan_since.elapsed())?;
        }
        if deleting && !view.is_deleting() {
            // The footer's own sentence, so a notification can never describe a removal
            // differently from the screen behind it.
            let said = view.notice().unwrap_or("the removal finished").to_owned();
            chrome.announce(&said, batch_since.elapsed())?;
        }
        scanning = view.is_scanning();
        deleting = view.is_deleting();
        chrome.show(Status::of(&view, outcome.freed))?;

        chrome.begin_frame()?;
        let drawn = terminal.draw(|frame| render::draw(frame, &mut view, &outcome.errors));
        // Ended before the draw's failure is reported, never after: a terminal left inside a
        // synchronized update keeps showing the frame before last, so a `?` here would trade
        // a reported error for a screen that is silently frozen.
        let ended = chrome.end_frame();
        drawn.and(ended)?;

        // A quit that was held back while a removal ran, now that it is over. Checked here
        // rather than where the key was pressed, because what the key produced was a promise
        // to leave and this is the first frame on which it can be kept.
        if view.wants_to_quit() {
            break;
        }
        if !events.poll(if view.is_moving() { FRAME } else { TICK })? {
            continue;
        }
        let event = events.read()?;
        match &event {
            // A resize is not a keystroke and produces no action; the redraw above is the
            // whole of what it needs.
            Event::Resize(..) => continue,
            // Focus reporting was asked for by the chrome and is answered to it: it is the
            // one thing that can tell a notification whether anybody is there to read it.
            Event::FocusGained => {
                chrome.focused(true);
                continue;
            }
            Event::FocusLost => {
                chrome.focused(false);
                continue;
            }
            _ => {}
        }
        match view.apply(action_for(&event, view.overlay())) {
            Effect::None => {}
            // Only ever reached with nothing in flight: the view holds a quit back while it
            // is deleting, and hands it over through `wants_to_quit` instead.
            Effect::Quit => break,
            Effect::Plan(targets) => {
                let plan = Planner::new(&options.root)
                    .one_file_system(options.one_file_system)
                    .older_than(options.older_than)
                    .plan(targets);
                view.ask(Pending::of(&plan));
            }
            Effect::Delete(targets) => {
                batch_since = Instant::now();
                batch.takes_over(spawn_delete(options, targets, post.clone()));
            }
        }
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
        // The freed total is left where it stands: a removal that said nothing is a removal
        // that gave no figure, and inventing one is the opposite of what this branch is for.
        view.deleted(
            "the removal ended without reporting what it did".to_owned(),
            outcome.freed,
        );
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
            Ok(Message::Found(Found::Pricing(path))) => view.pricing(&path),
            Ok(Message::Found(Found::Priced(priced))) => view.priced(&priced.path, priced.size),
            Ok(Message::Scanned(walk)) => {
                outcome.errors.extend(walk.errors);
                view.scanned();
            }
            // Both halves reach the view, and both carry the deleter's own running byte
            // total — which is what lets a row's number fall on bytes that have genuinely
            // left the disk rather than on a timer started once they already had.
            Ok(Message::Removing(Step::Freeing(freeing))) => {
                view.freeing(&freeing.path, freeing.bytes);
            }
            Ok(Message::Removing(Step::Finished(removed))) => {
                view.removed(&removed.path, removed.bytes, removed.complete);
            }
            // Where the batch has got to, which is a different question from what happened to
            // any row — a target that failed before unlinking anything still counts here.
            Ok(Message::Removing(Step::Swept(_))) => view.swept(),
            Ok(Message::Deleted(removal)) => {
                outcome.failures += removal.failures.len();
                outcome.freed += removal.bytes_freed();
                // The rows the safety model left standing, so each one can say so where it
                // is. The footer's count says how many; only the row can say which.
                view.refused(&removal.kept);
                // The batch's own arithmetic replaces the per-target figures the counter has
                // been climbing on, rather than being added to them: they are the same bytes
                // counted by the same code, so adding would double every one of them.
                view.deleted(summarise(&removal), outcome.freed);
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
            .watching(move |step| {
                let _ = reporting.send(Message::Removing(step.clone()));
            })
            .remove(&plan);
        // Posted before the thread ends, which is what lets `reap` read a finished thread
        // with an empty channel as "this one died without saying anything".
        let _ = post.send(Message::Deleted(Box::new(removal)));
    })
}

/// One line about what the scan found, for the notification that says it is over.
///
/// The header's own numbers rather than a second phrasing of them: somebody who comes back to
/// a notification and then looks at the screen has to be able to see the same run described.
fn scanned(view: &View) -> String {
    let total = view.total();
    format!(
        "{} reclaimable in {}",
        human(total.bytes),
        plural(total.claims, "directory", "directories")
    )
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
    use super::{Batch, Chrome, Decor, Message, Outcome, Restore, drain, reap, summarise};
    use crate::delete::{Failure, Refusal, Refused, Removal, Removed};
    use crate::fixture::priced;
    use crate::tree::Tree;
    use crate::tui::chrome::XTERM_STACK;
    use crate::tui::state::View;
    use crate::walk::{Found, WalkError, WalkOutcome};
    use std::path::Path;
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
    fn a_removals_progress_reaches_the_rows_and_the_freed_counter_from_one_event() {
        use crate::delete::{Freeing, Step};
        use crate::size::Size;
        use std::time::Instant;

        let (post, inbox) = channel();
        let mut tree = Tree::new("/scan");
        tree.insert(crate::fixture::hit(
            "/scan/app/node_modules",
            Size::Measured(1000),
            0,
        ));
        let mut view = View::new(tree);
        view.viewport(20);
        let mut outcome = Outcome::default();
        let start = Instant::now();
        view.animate(start);
        assert_eq!(view.drawn_total().bytes, 1000);
        assert!(!view.has_freed());

        // What the deleter says while it is working. The wiring is the point of this test:
        // the same event has to reach the row's number and the freed counter, because the
        // two moving in opposite directions is only true if they are the same bytes.
        post.send(Message::Removing(Step::Freeing(Freeing {
            path: "/scan/app/node_modules".into(),
            bytes: 600,
            entries: 12,
        })))
        .unwrap();
        drain(&mut view, &inbox, &mut outcome);
        view.animate(start);

        assert_eq!(view.drawn_total().bytes, 400);
        assert_eq!(view.drawn_freed(), 600);
        // Still on disk, still on screen, and not yet dimmed — it is emptying, not emptied.
        let row = view
            .tree()
            .find(Path::new("/scan/app/node_modules"))
            .unwrap();
        assert!(view.is_freeing(row));
        assert!(!view.is_spent(row));

        post.send(Message::Removing(Step::Finished(Removed {
            path: "/scan/app/node_modules".into(),
            bytes: 1000,
            entries: 20,
            complete: true,
        })))
        .unwrap();
        drain(&mut view, &inbox, &mut outcome);
        view.animate(start);

        assert_eq!(view.drawn_total().bytes, 0);
        assert_eq!(view.drawn_freed(), 1000);
        assert!(view.is_spent(row));
    }

    #[test]
    fn the_batchs_position_advances_on_a_target_the_deleter_could_not_touch() {
        use crate::delete::Step;

        let (post, inbox) = channel();
        let mut view = view();
        let mut outcome = Outcome::default();
        // One target, and the deleter fails on it before unlinking anything — so there is no
        // `Finished` and no row to move, only the pool saying it has moved on.
        view.deleting_for_test();
        assert_eq!(view.removing().unwrap().counted(), (0, 1));

        post.send(Message::Removing(Step::Swept("/scan/a/target".into())))
            .unwrap();
        drain(&mut view, &inbox, &mut outcome);

        // The wiring is the point: a missing arm here would leave the bar at zero for the
        // whole run and say nothing about it.
        assert_eq!(view.removing().unwrap().counted(), (1, 1));
        assert_eq!(view.removing().unwrap().percent(), 100);
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
    fn a_batch_waits_for_its_removal_however_the_scope_ends() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let finished = Arc::new(AtomicBool::new(false));
        let worker = Arc::clone(&finished);
        {
            let mut batch = Batch::default();
            batch.takes_over(std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(50));
                worker.store(true, Ordering::SeqCst);
            }));
            // …and here the scope ends, which is what a `?`, a `break` and a panic all do.
        }
        assert!(
            finished.load(Ordering::SeqCst),
            "the removal was abandoned rather than waited for"
        );
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
        let mut restore = Restore::new(Chrome::new(Vec::new(), Decor::silent()));
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

    #[test]
    fn the_decorations_are_handed_back_by_the_same_guard_and_not_a_second_one() {
        // The reason the chrome is a field of `Restore` rather than a guard beside it. What
        // is asserted is that the ordinary way out and the drop path are one path: `finish`
        // does the whole undoing, and `Drop` afterwards has nothing left to write.
        let mut restore = Restore::new(Chrome::new(
            Vec::new(),
            Decor {
                sync: true,
                title: Some(XTERM_STACK),
                progress: true,
                notify: None,
            },
        ));
        restore.chrome.enter().unwrap();
        restore.chrome.begin_frame().unwrap();
        restore.finish().unwrap();

        let said = String::from_utf8(restore.chrome.sink().clone()).unwrap();
        assert!(said.contains("\x1b[?2026l"), "a frozen screen: {said:?}");
        assert!(said.contains("\x1b]9;4;0;0\x07"), "the bar was left up");
        assert!(said.ends_with("\x1b[23;2t"), "the title was not put back");

        // And this is exactly what `Drop` runs a moment later, on every path that returns
        // through a `?`. Undoing it twice would pop a title this run never pushed.
        restore.finish().unwrap();
        assert_eq!(
            restore.chrome.sink().len(),
            said.len(),
            "the undoing was written a second time"
        );
    }
}

/// Driving the event loop itself, which needs a terminal that can be made to fail and a
/// keyboard that can be made to type.
///
/// The findings this exists for were all in the loop, and all of the same shape: an exit the
/// happy path does not take. There is no way to reach one from outside without these two
/// seams, which is exactly why they are here.
#[cfg(test)]
mod loop_tests {
    use super::{Chrome, Decor, Events, Options, drive};
    use crate::Ruleset;
    use crate::size::SizeMode;
    use crate::tui::chrome::XTERM_STACK;
    use ratatui::Terminal;
    use ratatui::backend::{Backend, TestBackend, WindowSize};
    use ratatui::buffer::Cell;
    use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::{Position, Size};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tempfile::TempDir;

    /// A result from a backend that cannot fail, in the shape of one that can.
    fn never<T>(result: Result<T, std::convert::Infallible>) -> io::Result<T> {
        result.map_err(|never| match never {})
    }

    /// A terminal that stops working the moment something else says so.
    ///
    /// Every method delegates; only `draw` reads the flag. That is the narrowest injection
    /// that reaches the `?` under test, and it is a real `io::Error` on the real code path
    /// rather than a branch added for the test.
    struct Flaky {
        inner: TestBackend,
        broken: Arc<AtomicBool>,
    }

    impl Backend for Flaky {
        type Error = io::Error;

        fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            if self.broken.load(Ordering::SeqCst) {
                return Err(io::Error::other("the terminal went away"));
            }
            never(self.inner.draw(content))
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            never(self.inner.hide_cursor())
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            never(self.inner.show_cursor())
        }

        fn get_cursor_position(&mut self) -> io::Result<Position> {
            never(self.inner.get_cursor_position())
        }

        fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
            never(self.inner.set_cursor_position(position))
        }

        fn clear(&mut self) -> io::Result<()> {
            never(self.inner.clear())
        }

        fn clear_region(&mut self, clear_type: ratatui::backend::ClearType) -> io::Result<()> {
            never(self.inner.clear_region(clear_type))
        }

        fn size(&self) -> io::Result<Size> {
            never(self.inner.size())
        }

        fn window_size(&mut self) -> io::Result<WindowSize> {
            never(self.inner.window_size())
        }

        fn flush(&mut self) -> io::Result<()> {
            never(self.inner.flush())
        }
    }

    /// How many keys a script will press before it gives up and quits anyway.
    ///
    /// Only reached by a run that never satisfies its `until`, which is a real failure — the
    /// point of the ceiling is that such a run *ends*, so the test reports what it found
    /// instead of hanging with no output.
    const PATIENCE: usize = 10_000;

    /// A keyboard that types the same short phrase over and over.
    ///
    /// Repeated rather than sequenced because the loop and the walk are concurrent: the row
    /// to mark does not exist until the walker finds it, and a script that fired once would
    /// be racing that. Every key in this cycle is harmless before it is useful — `x` with an
    /// empty batch says "nothing is marked", and `→`/`Enter` on a childless row do nothing.
    struct Script {
        keys: Vec<KeyCode>,
        at: usize,
        /// Presses `q` instead of the next key, once this says the run has done its job.
        ///
        /// A `q` *inside* the cycle would undo the repetition the cycle exists for: the first
        /// pass would end the run whether or not the walker had published anything yet, so on
        /// a machine under load the `x` lands on an empty batch, nothing is removed, and the
        /// assertion fires on scheduling rather than on a fault. Measured, not guessed — the
        /// test failed that way under three spinning cores, on this commit and on the one
        /// before any of this work.
        ///
        /// So the quit is a condition, which is the discipline the sibling test already
        /// applies to breaking the terminal: wait for the thing to have actually happened.
        until: Option<Box<dyn Fn() -> bool + Send>>,
    }

    impl Events for Script {
        fn poll(&mut self, _timeout: Duration) -> io::Result<bool> {
            Ok(true)
        }

        fn read(&mut self) -> io::Result<Event> {
            let leaving =
                self.at >= PATIENCE || self.until.as_ref().is_some_and(|finished| finished());
            let code = if leaving {
                KeyCode::Char('q')
            } else {
                self.keys[self.at % self.keys.len()]
            };
            self.at += 1;
            Ok(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        }
    }

    fn files_under(dir: &Path) -> usize {
        let mut count = 0;
        let mut stack = vec![dir.to_path_buf()];
        while let Some(at) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&at) else {
                continue;
            };
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    stack.push(entry.path());
                } else {
                    count += 1;
                }
            }
        }
        count
    }

    /// A project whose `node_modules` takes long enough to remove that a failure can land in
    /// the middle of it.
    fn fixture() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("app/node_modules");
        std::fs::create_dir_all(tmp.path().join("app")).unwrap();
        std::fs::write(tmp.path().join("app/package.json"), "{}").unwrap();
        for n in 0..40 {
            let dir = target.join(format!("p{n}"));
            std::fs::create_dir_all(&dir).unwrap();
            for f in 0..50 {
                std::fs::write(dir.join(format!("f{f}.js")), "x").unwrap();
            }
        }
        (tmp, target)
    }

    #[test]
    fn a_terminal_that_fails_mid_removal_still_waits_for_the_batch() {
        let (tmp, target) = fixture();
        let whole = files_under(&target);
        assert_eq!(whole, 2000);

        let broken = Arc::new(AtomicBool::new(false));
        // Breaks the terminal as soon as the removal is *demonstrably* under way — a real
        // condition rather than a sleep, and the exact moment the finding is about.
        let arming = Arc::clone(&broken);
        let counting = target.clone();
        let watcher = std::thread::spawn(move || {
            while files_under(&counting) == whole {
                std::thread::yield_now();
            }
            arming.store(true, Ordering::SeqCst);
        });

        let mut terminal = Terminal::new(Flaky {
            inner: TestBackend::new(100, 24),
            broken: Arc::clone(&broken),
        })
        .unwrap();
        // Mark the scan root, then ask-highlight-confirm, on repeat. The root row exists from
        // the first frame, and a mark on it covers whatever the walk finds underneath —
        // which is the streaming property doing the test's synchronisation for it.
        let mut keys = Script {
            keys: vec![
                KeyCode::Char(' '),
                KeyCode::Char('x'),
                KeyCode::Right,
                KeyCode::Enter,
            ],
            at: 0,
            // Nothing to wait for: this run is ended by the terminal failing, which is the
            // whole subject of the test.
            until: None,
        };

        let outcome = drive(
            &mut terminal,
            &mut keys,
            &mut Chrome::new(Vec::new(), Decor::silent()),
            &Options {
                root: tmp.path().to_path_buf(),
                min_size: crate::DEFAULT_MIN_SIZE,
                size_mode: SizeMode::Skip,
                one_file_system: true,
                older_than: None,
            },
            Arc::new(Ruleset::builtin().unwrap()),
        );
        watcher.join().unwrap();

        // The loop left through a `?`, which is the path that used to abandon the thread…
        assert!(outcome.is_err(), "the terminal was supposed to fail");
        // …and the batch the reader confirmed is finished rather than half done. Without the
        // join this is 2,000 files minus however many fitted into a few microseconds.
        assert!(
            !target.exists(),
            "{} survived a removal that was abandoned",
            target.display()
        );
    }

    #[test]
    fn a_marked_batch_is_removed_through_the_real_loop() {
        // The same machinery, ending the ordinary way: it is worth one test that the keys, the
        // planner and the deleter meet correctly, because everything else about the loop is
        // asserted through the pieces.
        let (tmp, target) = fixture();
        // The same wrapper with the flag never set — a terminal that works, whose `Error` is
        // the `io::Error` the loop is written against.
        let mut terminal = Terminal::new(Flaky {
            inner: TestBackend::new(100, 24),
            broken: Arc::new(AtomicBool::new(false)),
        })
        .unwrap();
        let gone = target.clone();
        let mut keys = Script {
            keys: vec![
                KeyCode::Char(' '),
                KeyCode::Char('x'),
                KeyCode::Right,
                KeyCode::Enter,
            ],
            at: 0,
            // The target is `rmdir`ed only once every child under it is gone, so this becomes
            // true exactly when the batch the test is about has finished. A `q` before then
            // would be the script racing the walker rather than the loop doing its job — and
            // a `q` after it is held by the view anyway until the removal reports.
            until: Some(Box::new(move || !gone.exists())),
        };

        let mut chrome = Chrome::new(
            Vec::new(),
            Decor {
                sync: true,
                title: Some(XTERM_STACK),
                progress: true,
                notify: None,
            },
        );
        let outcome = drive(
            &mut terminal,
            &mut keys,
            &mut chrome,
            &Options {
                root: tmp.path().to_path_buf(),
                min_size: crate::DEFAULT_MIN_SIZE,
                size_mode: SizeMode::Skip,
                one_file_system: true,
                older_than: None,
            },
            Arc::new(Ruleset::builtin().unwrap()),
        )
        .unwrap();

        assert!(!target.exists(), "nothing was removed");
        assert!(outcome.whole(), "{outcome:?}");
        assert!(
            tmp.path().join("app/package.json").exists(),
            "too much went"
        );

        // The decorations, through the real loop rather than through the chrome on its own.
        // Balance is the property that matters: an update begun once more than it is ended is
        // a terminal that stops repainting, and it is a *run*'s worth of frames that proves
        // that rather than one call and its pair.
        let said = String::from_utf8(chrome.sink().clone()).unwrap();
        assert!(said.contains("\x1b[?2026h"), "nothing was ever wrapped");
        // Counting the two halves would pass on a run that opened every frame and then closed
        // them all at the end. What has to hold is that they alternate: one open, one close,
        // never two of either in a row.
        let mut open = 0i32;
        for frame in said.split("\x1b[?2026").skip(1) {
            match frame.as_bytes().first() {
                Some(b'h') => open += 1,
                Some(b'l') => open -= 1,
                other => panic!("a private mode nobody wrote: {other:?}"),
            }
            assert!((0..=1).contains(&open), "the frames did not alternate");
        }
        assert_eq!(open, 0, "a frame was left open");
        assert!(said.contains("\x1b]0;pristine — "), "{said:?}");
        // The bar reaches 0 only when the guard restores it, which this test does not use, so
        // what the run itself must not do is leave it at a percentage it never got to.
        assert!(said.contains("\x1b]9;4;"), "no progress was ever reported");
    }
}
