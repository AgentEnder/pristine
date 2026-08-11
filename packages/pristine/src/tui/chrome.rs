//! Everything the terminal shows that is not a cell of the frame.
//!
//! Four decorations, one rule. The rule first, because it is the whole design: **every one of
//! these degrades to nothing.** Nothing here probes a capability, waits for an answer, or
//! sniffs a version beyond reading two environment variables; a terminal that does not know a
//! sequence ignores it, and a terminal this cannot identify is simply told less. None of it
//! runs at all when stdout is not a terminal, because an escape sequence written into a pipe
//! is corruption of somebody's data.
//!
//! # What each one buys
//!
//! - **Synchronized output (DEC 2026)** wraps every frame. Without it a 10 fps repaint tears,
//!   and it tears worst over ssh, which is where a sweep of a disk that is filling up tends to
//!   run. This is the one decoration with no allowlist: the private mode is defined to be
//!   ignorable and every terminal that parses `CSI` already drops what it does not know.
//! - **OSC 9;4 progress** puts a real bar on the dock or the taskbar. A full price of one real
//!   `~/repos` is 55.8 s, which is long enough that the reader has gone somewhere else, and
//!   the percentage is one the pool already knows: claims priced over claims found.
//! - **OSC 0 title** makes a backgrounded run readable from the tab bar. It is restored on the
//!   way out, including the error path — see [`Chrome::restore`].
//! - **One notification**, and only when the run was long enough to be worth interrupting
//!   somebody for *and* they are demonstrably looking elsewhere. A notification for a 200 ms
//!   scan is spam.
//!
//! # Why there is an allowlist for two of them and not the other two
//!
//! Because OSC 9 collides with itself. `OSC 9 ; <text>` is a desktop notification in iTerm2,
//! `WezTerm` and Ghostty; `OSC 9 ; 4 ; <state> ; <percent>` is `ConEmu`'s progress bar, read by
//! `WezTerm`, Ghostty, `ConEmu` and Windows Terminal. A terminal that knows only the first reads a
//! progress report as a notification saying `4;1;41`, which is worse than no bar at all. The
//! two that collide are therefore sent only where they are known to be understood, and the two
//! that cannot collide — a private mode and a title — go everywhere.

use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use ratatui::crossterm::event::{DisableFocusChange, EnableFocusChange};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate, SetTitle};

use super::state::View;
use crate::size::human;

/// How long a run has to have taken before finishing it is worth a notification.
///
/// The floor exists because the failure mode is spam, not silence: a default scan of a project
/// is over in well under a second and nobody wants to be told. Five seconds is about the
/// shortest run somebody walks away from, and the run this feature is for takes a minute.
const NOTIFY_AFTER: Duration = Duration::from_secs(5);

/// Pushes the window title onto the terminal's title stack, so the way out can pop it.
///
/// `xterm`'s, and there is no other way: no terminal will tell an application what its title
/// currently is, so restoring one means having asked the terminal to remember it. Terminals
/// without a title stack ignore both halves and keep whatever this run last set — the honest
/// limit of the feature, and the reason the title is set to something meaningful rather than
/// something transient.
const PUSH_TITLE: &str = "\x1b[22;2t";

/// Pops it back.
const POP_TITLE: &str = "\x1b[23;2t";

/// Which decorations a terminal is known to read.
///
/// Data rather than a chain of `if`s at each call site, and computed once: the environment
/// does not change under a running process, and a decision taken per frame is a decision that
/// can differ per frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Decor {
    /// Wrap frames in DEC 2026.
    pub sync: bool,
    /// Set and restore the window title.
    pub title: bool,
    /// Report progress as OSC 9;4.
    pub progress: bool,
    /// How to raise a desktop notification, if this terminal can.
    pub notify: Option<Notify>,
}

/// The spelling of a desktop notification that a given terminal reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notify {
    /// `OSC 9 ; <text> BEL` — iTerm2, `WezTerm`, Ghostty.
    Osc9,
    /// `OSC 777 ; notify ; <title> ; <body> BEL` — urxvt's, and read by several others.
    Osc777,
}

/// What each terminal is known to read, keyed by `TERM_PROGRAM`.
///
/// Absence from this table is not a claim that a terminal lacks the feature; it is a refusal
/// to guess. The cost of guessing wrong is a notification full of punctuation, and the cost of
/// not guessing is a missing progress bar.
const KNOWN: &[(&str, bool, Option<Notify>)] = &[
    ("ghostty", true, Some(Notify::Osc9)),
    ("WezTerm", true, Some(Notify::Osc9)),
    // No progress: iTerm2 reads `OSC 9 ; …` as a notification, so a progress report reaches
    // the reader as a pop-up saying `4;1;41`.
    ("iTerm.app", false, Some(Notify::Osc9)),
    ("Apple_Terminal", false, None),
];

impl Decor {
    /// What this process's terminal reads, from the environment and nothing else.
    #[must_use]
    pub fn detect() -> Self {
        if !io::stdout().is_terminal() {
            return Self::default();
        }
        Self::read(&|key| std::env::var(key).ok())
    }

    /// Nothing at all: the front end that is not a terminal, and the tests that are about
    /// something else.
    #[must_use]
    pub fn silent() -> Self {
        Self::default()
    }

    /// The decision, against an environment a test can supply.
    fn read(env: &dyn Fn(&str) -> Option<String>) -> Self {
        // A `TERM` that is absent or `dumb` is the one thing in the environment that is a
        // statement about escape sequences rather than about a product, and it says no.
        match env("TERM").as_deref() {
            None | Some("" | "dumb") => return Self::default(),
            Some(_) => {}
        }
        let program = env("TERM_PROGRAM").unwrap_or_default();
        let (progress, notify) = KNOWN
            .iter()
            .find(|(name, _, _)| *name == program)
            .map_or_else(
                || {
                    // Neither of these sets `TERM_PROGRAM`, and both read ConEmu's bar because
                    // one of them is ConEmu.
                    let bar = env("WT_SESSION").is_some() || env("ConEmuANSI").is_some();
                    (bar, None)
                },
                |(_, progress, notify)| (*progress, *notify),
            );
        Self {
            sync: true,
            title: true,
            progress,
            notify,
        }
    }
}

/// What the tab bar and the taskbar say about a run, at one moment.
///
/// A ladder rather than a set of flags: at any moment exactly one of these is the thing a
/// reader who is elsewhere wants to know, and the order is what makes that true.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// A removal is running. No denominator — the deleter reports targets as it finishes
    /// them, not bytes it still owes.
    Deleting,
    /// The pricing pool is behind the walk, by this percentage.
    Pricing(u8),
    /// Walking, with nothing outstanding to price.
    Scanning(u64),
    /// Nothing running, and a removal has happened this session.
    Freed(u64),
    /// Nothing running.
    Idle(u64),
}

impl Status {
    /// What the view is showing, said in one line.
    ///
    /// The percentage's denominator **grows**, because a claim is published the moment it is
    /// judged and priced later, so the figure can go down as the walk finds faster than the
    /// pool prices. That is honest rather than tidy: the alternative is a denominator that is
    /// only known when the walk finishes, which is 7.5 s into a 63 s run — a bar that
    /// appears once it has stopped being needed.
    #[must_use]
    pub fn of(view: &View, freed: u64) -> Self {
        let total = view.total();
        if view.is_deleting() {
            return Self::Deleting;
        }
        if view.is_scanning() {
            let priced = total.claims - total.unpriced;
            return match (total.unpriced, total.claims) {
                (0, _) | (_, 0) => Self::Scanning(total.bytes),
                (_, claims) => Self::Pricing(percent(priced, claims)),
            };
        }
        if freed > 0 {
            return Self::Freed(freed);
        }
        Self::Idle(total.bytes)
    }

    /// The window title.
    fn title(&self) -> String {
        match self {
            Self::Deleting => "pristine — deleting".to_owned(),
            Self::Pricing(percent) => format!("pristine — pricing {percent}%"),
            Self::Scanning(bytes) | Self::Idle(bytes) => format!("pristine — {}", human(*bytes)),
            Self::Freed(bytes) => format!("pristine — freed {}", human(*bytes)),
        }
    }

    /// The taskbar's bar.
    fn bar(&self) -> Bar {
        match self {
            // Both of these are work with no honest fraction attached, which is what the
            // indeterminate state is for. Reporting 0% instead would read as stuck.
            Self::Deleting | Self::Scanning(_) => Bar::Working,
            Self::Pricing(percent) => Bar::At(*percent),
            Self::Freed(_) | Self::Idle(_) => Bar::Off,
        }
    }
}

/// One of `1 - x/y` as a percentage, saturating rather than wrapping.
fn percent(part: usize, whole: usize) -> u8 {
    if whole == 0 {
        return 0;
    }
    let scaled = part.saturating_mul(100) / whole;
    u8::try_from(scaled.min(100)).unwrap_or(100)
}

/// The state of the taskbar's progress bar, in `ConEmu`'s vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Bar {
    /// No bar (state 0).
    Off,
    /// Something is happening and nobody can say how much of it is left (state 3).
    Working,
    /// This much of it is done (state 1).
    At(u8),
    /// The run ended without doing everything it was asked (state 2).
    Failed,
}

impl Bar {
    /// The two numbers OSC 9;4 carries. Both are always sent: the percentage is optional in
    /// the sequence and not every reader of it agrees what it defaults to.
    fn code(self) -> (u8, u8) {
        match self {
            Self::Off => (0, 0),
            Self::Working => (3, 0),
            Self::At(percent) => (1, percent),
            // Full rather than empty, because a zero-length red bar is one a reader cannot
            // see, and "this run ended badly" is the whole message.
            Self::Failed => (2, 100),
        }
    }
}

/// Whether the reader is looking at this terminal.
///
/// Assumed [`Focus::Here`] until the terminal says otherwise, which is deliberately the
/// conservative end: a terminal that does not report focus never contradicts the assumption, so
/// it never notifies, and a missing notification is the failure this feature is allowed to
/// have. The one it is not allowed to have is interrupting somebody who is already watching
/// the thing finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    /// The reader is here, or has not proved otherwise.
    Here,
    /// The terminal reported losing focus and has not reported getting it back.
    Away,
}

/// The terminal's decorations, and the promise to undo them.
///
/// Generic over its sink so the sequences are assertable. That is not a courtesy to the tests:
/// every one of these writes is invisible to every other kind of test — a title that is never
/// restored, a progress bar left at 41% forever, a frame that begins a synchronized update and
/// never ends it — and the last of those leaves the reader looking at a frozen screen.
#[derive(Debug)]
pub struct Chrome<W: Write> {
    out: W,
    decor: Decor,
    /// The title as last written, so a repaint ten times a second does not rewrite it.
    title: Option<String>,
    /// The bar as last written, for the same reason.
    bar: Option<Bar>,
    /// Whether the states this has to undo were actually entered.
    entered: bool,
    /// Whether a frame is open. A synchronized update that is begun and not ended is a
    /// terminal showing the frame before last, indefinitely.
    framing: bool,
    focus: Focus,
    /// Whether the run is ending badly, which the bar says on the way out.
    failed: bool,
}

impl<W: Write> Chrome<W> {
    /// A chrome that writes what `decor` allows, and nothing else, to `out`.
    pub fn new(out: W, decor: Decor) -> Self {
        Self {
            out,
            decor,
            title: None,
            bar: None,
            entered: false,
            framing: false,
            focus: Focus::Here,
            failed: false,
        }
    }

    /// Takes the states that have to be given back: the title, and focus reporting.
    ///
    /// Focus reporting is only asked for when a notification could actually be sent, because
    /// it is the answer to exactly one question — is anybody looking — and a terminal that
    /// cannot show a notification is not being asked it.
    ///
    /// The flag is set **before** the writes rather than after each one, which is the opposite
    /// of [`super::Restore`]'s rule and deliberate: a half-written `enter` leaves the terminal
    /// in a state this cannot know, and of the two ways to be wrong, undoing something that
    /// never happened costs five bytes at a terminal already being restored while skipping the
    /// undo leaves a shell answering every click with escape gibberish.
    ///
    /// # Errors
    ///
    /// Anything the terminal refuses.
    pub fn enter(&mut self) -> io::Result<()> {
        self.entered = true;
        if self.decor.title {
            self.put(PUSH_TITLE)?;
        }
        if self.decor.notify.is_some() {
            execute!(self.out, EnableFocusChange)?;
        }
        Ok(())
    }

    /// Opens a synchronized update. Everything drawn until [`Chrome::end_frame`] lands at once.
    ///
    /// # Errors
    ///
    /// Anything the terminal refuses.
    pub fn begin_frame(&mut self) -> io::Result<()> {
        if !self.decor.sync {
            return Ok(());
        }
        self.framing = true;
        execute!(self.out, BeginSynchronizedUpdate)
    }

    /// Closes it. Must run even when the draw between the two failed.
    ///
    /// # Errors
    ///
    /// Anything the terminal refuses.
    pub fn end_frame(&mut self) -> io::Result<()> {
        if !std::mem::take(&mut self.framing) {
            return Ok(());
        }
        execute!(self.out, EndSynchronizedUpdate)
    }

    /// Says where the run has got to, writing only what changed.
    ///
    /// # Errors
    ///
    /// Anything the terminal refuses.
    pub fn show(&mut self, status: Status) -> io::Result<()> {
        if self.decor.title {
            let title = status.title();
            if self.title.as_ref() != Some(&title) {
                execute!(self.out, SetTitle(text(&title)))?;
                self.title = Some(title);
            }
        }
        self.bar(status.bar())
    }

    /// Writes a bar, if it is not the one already showing.
    fn bar(&mut self, bar: Bar) -> io::Result<()> {
        if !self.decor.progress || self.bar == Some(bar) {
            return Ok(());
        }
        let (state, percent) = bar.code();
        self.put(&format!("\x1b]9;4;{state};{percent}\x07"))?;
        self.bar = Some(bar);
        Ok(())
    }

    /// What the terminal reported about the reader's attention.
    pub fn focused(&mut self, here: bool) {
        self.focus = if here { Focus::Here } else { Focus::Away };
    }

    /// Tells the reader something finished, if they are not here to see it and it took long
    /// enough to be worth saying.
    ///
    /// # Errors
    ///
    /// Anything the terminal refuses.
    pub fn announce(&mut self, body: &str, took: Duration) -> io::Result<()> {
        if self.focus == Focus::Here || took < NOTIFY_AFTER {
            return Ok(());
        }
        match self.decor.notify {
            None => Ok(()),
            Some(Notify::Osc9) => self.put(&format!("\x1b]9;pristine: {}\x07", text(body))),
            Some(Notify::Osc777) => {
                self.put(&format!("\x1b]777;notify;pristine;{}\x07", text(body)))
            }
        }
    }

    /// Records that the run is ending without having done everything it was asked.
    ///
    /// The bar says so rather than simply going out, because the reader who wanted a bar is by
    /// definition the reader who is not looking at the exit status.
    pub fn failed(&mut self) {
        self.failed = true;
    }

    /// Puts back everything this took, and can be called twice.
    ///
    /// Idempotent because it runs from two places by design: the ordinary way out, and the
    /// guard that owns it being dropped by a `?` or a panic. Every step is attempted and the
    /// first refusal reported, for [`super::Restore`]'s reason — a terminal half restored is
    /// no better than one not restored at all, and a failing call says nothing about whether
    /// the next would.
    ///
    /// # Errors
    ///
    /// The first thing the terminal refused.
    pub fn restore(&mut self) -> io::Result<()> {
        let mut first = self.end_frame();
        if !std::mem::take(&mut self.entered) {
            return first;
        }
        let bar = if self.failed { Bar::Failed } else { Bar::Off };
        first = first.and(self.bar(bar));
        if self.decor.notify.is_some() {
            first = first.and(execute!(self.out, DisableFocusChange));
        }
        if self.decor.title {
            first = first.and(self.put(POP_TITLE));
        }
        first
    }

    /// Writes a sequence and flushes it, because a frame that is waiting in a buffer is a
    /// frame that has not happened.
    fn put(&mut self, sequence: &str) -> io::Result<()> {
        self.out.write_all(sequence.as_bytes())?;
        self.out.flush()
    }

    /// What has been written, for the tests that are about exactly that.
    #[cfg(test)]
    pub(crate) fn sink(&self) -> &W {
        &self.out
    }
}

/// Strips the control characters out of anything going inside a title or a notification.
///
/// Nothing in this file interpolates a path today, and this is here for the day something
/// does: a `BEL` or an `ESC` in a directory name would end the sequence early and leave the
/// rest of the name being read as commands. A cleaner is pointed at exactly the directories
/// whose names it did not choose.
fn text(said: &str) -> String {
    said.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::{Chrome, Decor, Notify, Status, text};
    use crate::fixture::{hit, priced};
    use crate::size::Size;
    use crate::tree::Tree;
    use crate::tui::state::View;
    use std::collections::HashMap;
    use std::time::Duration;

    /// Everything on, which is what a terminal this can identify gets.
    fn everything() -> Decor {
        Decor {
            sync: true,
            title: true,
            progress: true,
            notify: Some(Notify::Osc9),
        }
    }

    fn chrome(decor: Decor) -> Chrome<Vec<u8>> {
        Chrome::new(Vec::new(), decor)
    }

    fn written(chrome: &Chrome<Vec<u8>>) -> String {
        String::from_utf8(chrome.sink().clone()).unwrap()
    }

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    fn view() -> View {
        View::new(Tree::new("/scan"))
    }

    #[test]
    fn a_terminal_that_reads_nothing_is_written_nothing() {
        // The property the whole module rests on. Not one escape byte reaches a stdout that
        // is a pipe, whatever the run does.
        let mut chrome = chrome(Decor::silent());
        chrome.enter().unwrap();
        chrome.begin_frame().unwrap();
        chrome.show(Status::Pricing(41)).unwrap();
        chrome.end_frame().unwrap();
        chrome.focused(false);
        chrome.announce("done", Duration::from_secs(60)).unwrap();
        chrome.failed();
        chrome.restore().unwrap();

        assert_eq!(written(&chrome), "", "an escape reached a pipe");
    }

    #[test]
    fn every_frame_is_wrapped_in_a_synchronized_update() {
        let mut chrome = chrome(everything());
        chrome.begin_frame().unwrap();
        chrome.end_frame().unwrap();

        assert_eq!(written(&chrome), "\x1b[?2026h\x1b[?2026l");
    }

    #[test]
    fn a_frame_left_open_is_closed_by_the_restore() {
        // The failure this prevents is the worst one here: a terminal inside a synchronized
        // update shows the frame before last and keeps showing it, so a draw that fails
        // between the two halves freezes the screen rather than reporting anything.
        let mut chrome = chrome(everything());
        chrome.enter().unwrap();
        chrome.begin_frame().unwrap();
        chrome.restore().unwrap();

        let said = written(&chrome);
        assert!(
            said.contains("\x1b[?2026l"),
            "the frame was left open: {said:?}"
        );
        assert_eq!(said.matches("\x1b[?2026l").count(), 1);
    }

    #[test]
    fn the_title_is_written_once_per_change() {
        // Ten frames a second times a title nothing has changed is a terminal being asked to
        // redraw its own tab bar for no reason.
        let mut chrome = chrome(everything());
        chrome.show(Status::Idle(1024)).unwrap();
        chrome.show(Status::Idle(1024)).unwrap();
        assert_eq!(written(&chrome).matches("\x1b]0;").count(), 1);

        chrome.show(Status::Freed(2048)).unwrap();
        let said = written(&chrome);
        assert_eq!(said.matches("\x1b]0;").count(), 2);
        assert!(said.contains("pristine — freed 2.0 KiB"), "{said:?}");
    }

    #[test]
    fn the_title_is_put_back_on_the_way_out_and_only_once() {
        let mut chrome = chrome(everything());
        chrome.enter().unwrap();
        chrome.show(Status::Scanning(0)).unwrap();
        chrome.restore().unwrap();
        // The second call is the guard's `Drop` after an ordinary `finish`, which is the
        // path every early return takes.
        chrome.restore().unwrap();

        let said = written(&chrome);
        assert_eq!(said.matches("\x1b[22;2t").count(), 1, "{said:?}");
        assert_eq!(said.matches("\x1b[23;2t").count(), 1, "{said:?}");
    }

    #[test]
    fn pricing_reports_a_percentage_and_the_end_of_a_run_takes_the_bar_away() {
        let mut chrome = chrome(everything());
        chrome.enter().unwrap();
        chrome.show(Status::Scanning(10)).unwrap();
        assert!(
            written(&chrome).contains("\x1b]9;4;3;0\x07"),
            "indeterminate"
        );

        chrome.show(Status::Pricing(41)).unwrap();
        assert!(written(&chrome).contains("\x1b]9;4;1;41\x07"));

        chrome.restore().unwrap();
        assert!(written(&chrome).ends_with("\x1b[23;2t"));
        assert!(
            written(&chrome).contains("\x1b]9;4;0;0\x07"),
            "the bar was left up"
        );
    }

    #[test]
    fn a_run_that_ends_with_failures_leaves_the_bar_saying_so() {
        let mut chrome = chrome(everything());
        chrome.enter().unwrap();
        chrome.failed();
        chrome.restore().unwrap();

        assert!(written(&chrome).contains("\x1b]9;4;2;100\x07"));
    }

    #[test]
    fn a_terminal_that_does_not_read_the_bar_is_not_sent_one() {
        // iTerm2's shape: it would read `OSC 9 ; 4 ; …` as a notification and pop up a box
        // saying `4;1;41`.
        let mut chrome = chrome(Decor {
            sync: true,
            title: true,
            progress: false,
            notify: Some(Notify::Osc9),
        });
        chrome.enter().unwrap();
        chrome.show(Status::Pricing(41)).unwrap();
        chrome.restore().unwrap();

        let said = written(&chrome);
        assert!(!said.contains("\x1b]9;4"), "{said:?}");
        assert!(said.contains("pristine — pricing 41%"));
    }

    #[test]
    fn a_notification_waits_for_a_run_worth_interrupting_somebody_for() {
        let mut chrome = chrome(everything());
        chrome.focused(false);
        chrome
            .announce("scanned", Duration::from_millis(200))
            .unwrap();
        assert_eq!(written(&chrome), "", "a 200 ms scan raised a notification");

        chrome.announce("scanned", Duration::from_secs(60)).unwrap();
        assert_eq!(written(&chrome), "\x1b]9;pristine: scanned\x07");
    }

    #[test]
    fn a_reader_who_is_watching_is_not_notified() {
        let mut chrome = chrome(everything());
        // Never told otherwise, which is also what a terminal that cannot report focus leaves
        // behind — and that silence is the direction this is allowed to fail in.
        chrome.announce("scanned", Duration::from_secs(60)).unwrap();
        assert_eq!(written(&chrome), "");

        chrome.focused(false);
        chrome.announce("scanned", Duration::from_secs(60)).unwrap();
        assert!(written(&chrome).contains("\x1b]9;pristine: scanned\x07"));

        chrome.focused(true);
        let before = written(&chrome).len();
        chrome.announce("more", Duration::from_secs(60)).unwrap();
        assert_eq!(written(&chrome).len(), before, "notified after coming back");
    }

    #[test]
    fn the_other_spelling_of_a_notification() {
        let mut chrome = chrome(Decor {
            notify: Some(Notify::Osc777),
            ..everything()
        });
        chrome.focused(false);
        chrome
            .announce("freed 2.0 KiB", Duration::from_secs(60))
            .unwrap();

        assert_eq!(
            written(&chrome),
            "\x1b]777;notify;pristine;freed 2.0 KiB\x07"
        );
    }

    #[test]
    fn focus_reporting_is_only_asked_for_when_it_would_answer_something() {
        let mut asked = chrome(everything());
        asked.enter().unwrap();
        asked.restore().unwrap();
        assert!(written(&asked).contains("\x1b[?1004h"));
        assert!(
            written(&asked).contains("\x1b[?1004l"),
            "left reporting focus"
        );

        let mut quiet = chrome(Decor {
            notify: None,
            ..everything()
        });
        quiet.enter().unwrap();
        quiet.restore().unwrap();
        assert!(!written(&quiet).contains("1004"));
    }

    #[test]
    fn what_the_view_is_doing_decides_what_the_tab_says() {
        let mut view = view();
        assert_eq!(Status::of(&view, 0), Status::Scanning(0));

        view.found(hit("/scan/a/node_modules", Size::Unmeasured, 0));
        view.found(priced("/scan/b/target", 2048));
        // One of two priced, while the walk is still running.
        assert_eq!(Status::of(&view, 0), Status::Pricing(50));

        view.priced(
            std::path::Path::new("/scan/a/node_modules"),
            Size::Measured(1024),
        );
        assert_eq!(Status::of(&view, 0), Status::Scanning(3072));

        view.scanned();
        assert_eq!(Status::of(&view, 0), Status::Idle(3072));
        // A session that removed something says what it got back rather than what is left,
        // because that is the number the reader went away to wait for.
        assert_eq!(Status::of(&view, 4096), Status::Freed(4096));

        view.deleting_for_test();
        assert_eq!(Status::of(&view, 4096), Status::Deleting);
    }

    #[test]
    fn an_unpriced_scan_is_indeterminate_rather_than_stuck_at_zero() {
        // `--breakdown-under` leaves most claims unpriced forever, so the percentage is not a
        // fraction of anything that will complete. It still describes what has been priced.
        let mut view = view();
        for n in 0..4 {
            view.found(hit(
                &format!("/scan/p{n}/node_modules"),
                Size::Unmeasured,
                0,
            ));
        }
        assert_eq!(Status::of(&view, 0), Status::Pricing(0));
        view.scanned();
        assert_eq!(Status::of(&view, 0), Status::Idle(0));
    }

    #[test]
    fn a_dumb_terminal_gets_nothing_and_an_unknown_one_gets_what_cannot_collide() {
        assert_eq!(Decor::read(&env(&[("TERM", "dumb")])), Decor::silent());
        assert_eq!(Decor::read(&env(&[])), Decor::silent());

        // Neither a private mode nor a title can be misread as something else, so an
        // unrecognised terminal still gets both.
        assert_eq!(
            Decor::read(&env(&[("TERM", "xterm-256color")])),
            Decor {
                sync: true,
                title: true,
                progress: false,
                notify: None,
            }
        );
    }

    #[test]
    fn the_terminals_that_are_known_get_what_they_are_known_to_read() {
        let ghostty = Decor::read(&env(&[
            ("TERM", "xterm-ghostty"),
            ("TERM_PROGRAM", "ghostty"),
        ]));
        assert!(ghostty.progress && ghostty.notify == Some(Notify::Osc9));

        let iterm = Decor::read(&env(&[
            ("TERM", "xterm-256color"),
            ("TERM_PROGRAM", "iTerm.app"),
        ]));
        assert!(
            !iterm.progress,
            "a progress report would arrive as a pop-up"
        );
        assert_eq!(iterm.notify, Some(Notify::Osc9));

        // Windows Terminal names itself in a variable of its own rather than in TERM_PROGRAM.
        let wt = Decor::read(&env(&[("TERM", "xterm-256color"), ("WT_SESSION", "…")]));
        assert!(wt.progress);
        assert_eq!(wt.notify, None);

        let apple = Decor::read(&env(&[
            ("TERM", "xterm-256color"),
            ("TERM_PROGRAM", "Apple_Terminal"),
        ]));
        assert!(!apple.progress);
        assert_eq!(apple.notify, None);
    }

    #[test]
    fn nothing_interpolated_can_end_the_sequence_it_is_inside() {
        assert_eq!(text("node_modules\x07;rm -rf /"), "node_modules;rm -rf /");
        assert_eq!(text("a\x1b]0;b"), "a]0;b");
    }
}
