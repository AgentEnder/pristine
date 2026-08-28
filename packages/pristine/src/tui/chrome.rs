//! Everything the terminal shows that is not a cell of the frame.
//!
//! Five decorations, one rule. The rule first, because it is the whole design: **every one of
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
//!   way out, including the error path — see [`Chrome::restore`] — and it is only ever set on a
//!   terminal that can restore it. See [`Title`].
//! - **One notification**, and only when the run was long enough to be worth interrupting
//!   somebody for *and* they are demonstrably looking elsewhere. A notification for a 200 ms
//!   scan is spam.
//!
//! - **The kitty graphics protocol** is the one decoration that is *cells* rather than
//!   chrome: it is what puts [`super::treemap`]'s picture on the screen. It is decided here
//!   anyway, because what decides it is which terminal this is, which is this table's
//!   subject — and two tables reading one environment are two tables that can disagree.
//!
//! # Why four of the five are allowlisted, and one is not
//!
//! **Only the synchronized update goes everywhere**, because it is the only one that leaves
//! nothing behind: an unknown private mode is dropped by every parser that understands `CSI`,
//! and there is no state to give back afterwards. The other four all fail by *persisting* —
//! an image most loudly of all, since a terminal that does not decode `APC G` prints a
//! megabyte of base64 into the reader's scrollback.
//!
//! Two of them fail by being misread, and OSC 9 colliding with itself is why.
//! `OSC 9 ; <text>` is a desktop notification in iTerm2, `WezTerm` and Ghostty;
//! `OSC 9 ; 4 ; <state> ; <percent>` is `ConEmu`'s progress bar, read by `WezTerm`, Ghostty,
//! `ConEmu` and Windows Terminal. A terminal that knows only the first reads a progress report
//! as a notification saying `4;1;41`, which is worse than no bar at all.
//!
//! The third fails by being *unreturnable*, which is subtler and worse. Setting a title is easy
//! everywhere; putting the old one back needs a title stack that not every terminal keeps, and
//! a terminal without one is simply left holding `pristine — freed 41.2 GiB` forever. So the
//! title is not a flag — it is the push/pop pair itself ([`Title`]), present only for terminals
//! documented to keep the stack, which makes "set a title nothing can clear" unrepresentable
//! rather than merely avoided.

use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use ratatui::crossterm::event::{DisableFocusChange, EnableFocusChange};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate, SetTitle};

use super::state::{View, percent};
use crate::size::human;

/// How long a run has to have taken before finishing it is worth a notification.
///
/// The floor exists because the failure mode is spam, not silence: a default scan of a project
/// is over in well under a second and nobody wants to be told. Five seconds is about the
/// shortest run somebody walks away from, and the run this feature is for takes a minute.
const NOTIFY_AFTER: Duration = Duration::from_secs(5);

/// How a terminal's window title is taken, and given back.
///
/// **One value carrying both halves, which is the entire point of the type.** A title is a
/// piece of the reader's terminal that this run borrows, and the rule for everything borrowed
/// here is that it has to be returnable. No terminal will tell an application what its title
/// currently is — there is no query to ask — so the only way to put one back is to have asked
/// the terminal to remember it first, which is what `xterm`'s title stack is for.
///
/// A terminal with no stack ignores the push, ignores the pop, and keeps whatever this run last
/// set: `pristine — freed 41.2 GiB` in a tab bar for the rest of that terminal's life. That is
/// not a decoration degrading to nothing, it is a decoration that never leaves. So the two
/// sequences are held together as one capability, and a terminal that is not known to have it
/// is never sent a title at all — the setting cannot be switched on without the putting back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Title {
    /// Saves the title this run is about to replace.
    push: &'static str,
    /// Puts it back.
    pop: &'static str,
}

/// `xterm`'s title stack, which is the only one there is.
pub const XTERM_STACK: Title = Title {
    push: "\x1b[22;2t",
    pop: "\x1b[23;2t",
};

/// Which decorations a terminal is known to read.
///
/// Data rather than a chain of `if`s at each call site, and computed once: the environment
/// does not change under a running process, and a decision taken per frame is a decision that
/// can differ per frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Decor {
    /// Wrap frames in DEC 2026.
    pub sync: bool,
    /// How to set the window title and put it back, if this terminal can do both.
    pub title: Option<Title>,
    /// Report progress as OSC 9;4.
    pub progress: bool,
    /// How to raise a desktop notification, if this terminal can.
    pub notify: Option<Notify>,
    /// Read the kitty graphics protocol — which is how the treemap pane gets on the screen.
    ///
    /// A column of this table rather than a second one keyed on the same two environment
    /// variables, because two tables read from one environment are two tables that can
    /// disagree about which terminal this is. It is also the only decoration here that is
    /// *cells* rather than chrome, and it is here anyway for that reason: what decides it is
    /// the terminal's identity, which is this table's whole subject.
    ///
    /// Absence is a refusal to guess, as everywhere else. The protocol does define a query
    /// for "do you read this", and it is a round trip with no bound on the silence — the
    /// blocking probe this module exists to avoid. See [`super::treemap`].
    pub graphics: bool,
}

/// The spelling of a desktop notification that a given terminal reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notify {
    /// `OSC 9 ; <text> BEL` — iTerm2, `WezTerm`, Ghostty.
    Osc9,
    /// `OSC 777 ; notify ; <title> ; <body> BEL` — urxvt's, and read by several others.
    Osc777,
}

/// One terminal, and everything it is known to read.
struct Known {
    /// What it calls itself in `TERM_PROGRAM`, or `""` for one that sets no such variable.
    program: &'static str,
    /// A `TERM` that names this terminal and nothing else, or `""`. `xterm-256color` is not
    /// such a name — half the terminals here can be found wearing it — which is why this is a
    /// second key rather than the first one.
    term: &'static str,
    /// What it reads.
    decor: Decor,
}

impl Known {
    /// Whether this row is the terminal the environment describes.
    fn names(&self, program: &str, term: &str) -> bool {
        (!self.program.is_empty() && self.program == program)
            || (!self.term.is_empty() && self.term == term)
    }
}

/// The table. **Absence from it is a refusal to guess, not a claim about a terminal**, and
/// every column is a claim that the terminal *documents* the sequence in question.
///
/// The asymmetry in the cost of being wrong is what sets the default. A decoration wrongly
/// withheld is a feature somebody does not get; a decoration wrongly sent is a pop-up full of
/// punctuation, or a window title nobody can clear. So a row is added when a terminal's own
/// documentation says it reads the sequence, and never because it probably does.
const KNOWN: &[Known] = &[
    Known {
        program: "ghostty",
        term: "xterm-ghostty",
        decor: Decor {
            sync: true,
            title: Some(XTERM_STACK),
            progress: true,
            notify: Some(Notify::Osc9),
            graphics: true,
        },
    },
    Known {
        program: "WezTerm",
        term: "wezterm",
        decor: Decor {
            sync: true,
            title: Some(XTERM_STACK),
            progress: true,
            notify: Some(Notify::Osc9),
            graphics: true,
        },
    },
    Known {
        program: "iTerm.app",
        term: "",
        decor: Decor {
            sync: true,
            title: Some(XTERM_STACK),
            // iTerm2 reads `OSC 9 ; …` as a notification, so a progress report would reach the
            // reader as a pop-up saying `4;1;41`.
            progress: false,
            notify: Some(Notify::Osc9),
            // iTerm2's inline images are its own OSC 1337, not this protocol. That is the
            // obvious next terminal to reach and it is a different encoder, so it is a
            // follow-on rather than a row that can be flipped.
            graphics: false,
        },
    },
    Known {
        program: "",
        term: "xterm-kitty",
        decor: Decor {
            sync: true,
            title: Some(XTERM_STACK),
            progress: false,
            // kitty's notification is OSC 99, which nothing here speaks.
            notify: None,
            graphics: true,
        },
    },
    Known {
        program: "Apple_Terminal",
        term: "",
        // Looked at, and the answer is no to everything but the private mode. Kept as a row
        // rather than left to the default so the next person does not have to look again.
        decor: Decor {
            sync: true,
            title: None,
            progress: false,
            notify: None,
            graphics: false,
        },
    },
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
        let term = env("TERM").unwrap_or_default();
        if term.is_empty() || term == "dumb" {
            return Self::silent();
        }
        // What an unidentified terminal gets: the one decoration that changes nothing it could
        // be left holding. An unknown private mode is dropped by every parser that understands
        // `CSI` at all, and there is no state to give back afterwards.
        let anonymous = Self {
            sync: true,
            ..Self::silent()
        };
        // Inside a multiplexer, `TERM_PROGRAM` names whatever started the *server* — which is
        // not necessarily what is parsing these bytes, and may not still be running. tmux drops
        // the OSC sequences it does not implement, so most of this would go nowhere; the title
        // is the exception, because tmux does set a pane title from OSC 0 and would then be
        // left holding it.
        if term.starts_with("screen") || term.starts_with("tmux") {
            return anonymous;
        }
        let program = env("TERM_PROGRAM").unwrap_or_default();
        if let Some(known) = KNOWN.iter().find(|known| known.names(&program, &term)) {
            return known.decor;
        }
        // Neither of these sets `TERM_PROGRAM`, and both read ConEmu's bar because one of them
        // is ConEmu. Neither is known to keep a title stack.
        if env("WT_SESSION").is_some() || env("ConEmuANSI").is_some() {
            return Self {
                progress: true,
                ..anonymous
            };
        }
        anonymous
    }
}

/// What the tab bar and the taskbar say about a run, at one moment.
///
/// A ladder rather than a set of flags: at any moment exactly one of these is the thing a
/// reader who is elsewhere wants to know, and the order is what makes that true.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// A removal is running, and this much of its batch is behind it.
    ///
    /// The one bar here with a denominator that does not move. The batch's size is fixed when
    /// the reader answers the confirmation, where the pricing bar below counts against a total
    /// the walk is still adding to — so this is the fraction a reader who has left the terminal
    /// can actually plan around, and a running byte total on its own cannot say whether a long
    /// delete is a third of the way through or nearly done.
    Deleting(u8),
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
        if let Some(removing) = view.removing() {
            return Self::Deleting(removing.percent());
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
            Self::Deleting(percent) => format!("pristine — deleting {percent}%"),
            Self::Pricing(percent) => format!("pristine — pricing {percent}%"),
            Self::Scanning(bytes) | Self::Idle(bytes) => format!("pristine — {}", human(*bytes)),
            Self::Freed(bytes) => format!("pristine — freed {}", human(*bytes)),
        }
    }

    /// The taskbar's bar.
    fn bar(&self) -> Bar {
        match self {
            // Walking has no honest fraction attached — nothing knows how many directories
            // are under a path until they have been visited — which is what the indeterminate
            // state is for. Reporting 0% instead would read as stuck.
            Self::Scanning(_) => Bar::Working,
            Self::Deleting(percent) | Self::Pricing(percent) => Bar::At(*percent),
            Self::Freed(_) | Self::Idle(_) => Bar::Off,
        }
    }
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
        if let Some(title) = self.decor.title {
            self.put(title.push)?;
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
        // Gated on the *stack*, not on a separate "may I set a title" flag: the terminals that
        // cannot give one back are exactly the terminals that are never given one to hold.
        if self.decor.title.is_some() {
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
        if let Some(title) = self.decor.title {
            first = first.and(self.put(title.pop));
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
    use super::{Chrome, Decor, Notify, Status, XTERM_STACK, text};
    use crate::fixture::{hit, priced};
    use crate::size::Size;
    use crate::tree::Tree;
    use crate::tui::keymap::{Action, Turn};
    use crate::tui::state::{Planned, View};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Duration;

    /// Everything on, which is what a terminal this can identify gets.
    fn everything() -> Decor {
        Decor {
            sync: true,
            title: Some(XTERM_STACK),
            progress: true,
            notify: Some(Notify::Osc9),
            graphics: true,
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
            progress: false,
            ..everything()
        });
        chrome.enter().unwrap();
        chrome.show(Status::Pricing(41)).unwrap();
        chrome.restore().unwrap();

        let said = written(&chrome);
        assert!(!said.contains("\x1b]9;4"), "{said:?}");
        assert!(said.contains("pristine — pricing 41%"));
    }

    #[test]
    fn a_terminal_that_cannot_hand_a_title_back_is_never_given_one() {
        // The one decoration that fails by *persisting* rather than by being ignored. A title
        // set on a terminal with no stack is `pristine — freed 41.2 GiB` in somebody's tab bar
        // for the rest of that terminal's life, which is the opposite of degrading to nothing.
        let mut chrome = chrome(Decor {
            title: None,
            ..everything()
        });
        chrome.enter().unwrap();
        chrome.show(Status::Freed(2048)).unwrap();
        chrome.restore().unwrap();

        let said = written(&chrome);
        assert!(!said.contains("\x1b]0;"), "a title was set: {said:?}");
        assert!(
            !said.contains("22;2t") && !said.contains("23;2t"),
            "{said:?}"
        );
        // The rest still works: this is a narrowing of one decoration, not of the module.
        assert!(said.contains("\x1b]9;4;0;0\x07"));
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
        // Synced first, exactly as the loop does before it asks: the rolled-up numbers are
        // recomputed once per frame, so reading them without one is reading last frame's.
        view.sync();
        // One of two priced, while the walk is still running.
        assert_eq!(Status::of(&view, 0), Status::Pricing(50));

        view.priced(
            std::path::Path::new("/scan/a/node_modules"),
            Size::Measured(1024),
            0,
        );
        view.sync();
        assert_eq!(Status::of(&view, 0), Status::Scanning(3072));

        view.scanned();
        assert_eq!(Status::of(&view, 0), Status::Idle(3072));
        // A session that removed something says what it got back rather than what is left,
        // because that is the number the reader went away to wait for.
        assert_eq!(Status::of(&view, 4096), Status::Freed(4096));

        // …until a removal starts, which outranks everything: it is the one thing running, and
        // unlike the walk it can say how far through it is.
        view.deleting_for_test();
        assert_eq!(Status::of(&view, 4096), Status::Deleting(0));
    }

    #[test]
    fn a_removal_reports_where_it_has_got_to_rather_than_only_that_it_is_running() {
        let mut view = view();
        view.found(priced("/scan/a/node_modules", 1024));
        view.found(priced("/scan/b/node_modules", 1024));
        view.found(priced("/scan/c/node_modules", 1024));
        view.found(priced("/scan/d/node_modules", 1024));
        view.scanned();
        view.asking(
            &["a", "b", "c", "d"]
                .iter()
                .map(|name| {
                    Planned::at(
                        PathBuf::from(format!("/scan/{name}/node_modules")),
                        Size::Measured(1024),
                    )
                })
                .collect::<Vec<_>>(),
            &[],
        );
        view.apply(Action::Highlight(Turn::Next));
        view.apply(Action::Answer);

        // A bar rather than the indeterminate throbber the walk gets: the denominator was
        // fixed by the confirmation, so every target that comes back moves it by a knowable
        // amount. This is the whole difference between "something is happening" and "you are
        // half way".
        assert_eq!(Status::of(&view, 0), Status::Deleting(0));
        assert_eq!(Status::of(&view, 0).bar().code(), (1, 0));

        view.removed(std::path::Path::new("/scan/a/node_modules"), 1024, true);
        view.swept(std::path::Path::new("/scan/a/node_modules"));
        assert_eq!(Status::of(&view, 0), Status::Deleting(25));
        // A target the sweep could not finish is still one it is no longer working on, and so
        // is a target it could not touch at all: the bar says where the deleter is, not how
        // much of the batch worked.
        view.removed(std::path::Path::new("/scan/b/node_modules"), 512, false);
        view.swept(std::path::Path::new("/scan/b/node_modules"));
        view.swept(std::path::Path::new("/scan/c/node_modules"));
        assert_eq!(Status::of(&view, 0), Status::Deleting(75));
        assert_eq!(Status::of(&view, 0).bar().code(), (1, 75));

        // And the batch reporting takes the bar down.
        view.deleted(crate::tui::state::Notice::standing("freed 1.5 KiB"), 1536);
        assert_eq!(Status::of(&view, 1536), Status::Freed(1536));
        assert_eq!(Status::of(&view, 1536).bar().code(), (0, 0));
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
        view.sync();
        assert_eq!(Status::of(&view, 0), Status::Pricing(0));
        view.scanned();
        assert_eq!(Status::of(&view, 0), Status::Idle(0));
    }

    #[test]
    fn a_dumb_terminal_gets_nothing_and_an_unknown_one_gets_only_what_leaves_nothing_behind() {
        assert_eq!(Decor::read(&env(&[("TERM", "dumb")])), Decor::silent());
        assert_eq!(Decor::read(&env(&[])), Decor::silent());

        // A private mode is the only one of the five an unidentified terminal can be sent
        // safely: anything that parses `CSI` drops it, and it leaves no state behind. A title
        // would be left standing, the two OSCs can be misread as each other, and an image
        // sent to a terminal that cannot decode it is a screenful of base64.
        assert_eq!(
            Decor::read(&env(&[("TERM", "xterm-256color")])),
            Decor {
                sync: true,
                title: None,
                progress: false,
                notify: None,
                graphics: false,
            }
        );
    }

    #[test]
    fn a_multiplexer_is_not_the_terminal_named_in_the_environment() {
        // Inside tmux, `TERM_PROGRAM` names whatever started the server — possibly something
        // that is no longer running, and certainly not what is parsing these bytes. Taking it
        // at its word sets a title tmux is then left holding.
        for term in ["screen-256color", "tmux-256color"] {
            assert_eq!(
                Decor::read(&env(&[("TERM", term), ("TERM_PROGRAM", "ghostty")])),
                Decor {
                    sync: true,
                    ..Decor::silent()
                },
                "{term} was taken for the terminal that started it"
            );
        }
    }

    #[test]
    fn every_terminal_offered_a_title_is_offered_the_way_to_put_it_back() {
        // Over the shipped table rather than a fixture. The type is what makes this hold — a
        // title is the push/pop pair, so "sets a title" cannot be spelled without the restore
        // — and this is the assertion that the table cannot quietly acquire a half of one.
        for known in super::KNOWN {
            if let Some(title) = known.decor.title {
                assert!(
                    !title.push.is_empty() && !title.pop.is_empty(),
                    "{} sets a title it cannot put back",
                    known.program
                );
            }
        }
    }

    #[test]
    fn the_terminals_that_are_known_get_what_they_are_known_to_read() {
        let ghostty = Decor::read(&env(&[
            ("TERM", "xterm-ghostty"),
            ("TERM_PROGRAM", "ghostty"),
        ]));
        assert!(ghostty.progress && ghostty.notify == Some(Notify::Osc9));
        assert_eq!(ghostty.title, Some(XTERM_STACK));

        // A terminal that sets no `TERM_PROGRAM` is found by the `TERM` that names it and
        // nothing else — which `xterm-256color` is not, and is why that is the second key.
        let kitty = Decor::read(&env(&[("TERM", "xterm-kitty")]));
        assert_eq!(kitty.title, Some(XTERM_STACK));
        assert!(!kitty.progress);
        assert!(kitty.graphics, "the terminal the protocol is named after");

        // …and the one that has inline images of a different spelling gets none, because a
        // row here is a claim that the terminal reads *this* sequence.
        assert!(
            !Decor::read(&env(&[
                ("TERM_PROGRAM", "iTerm.app"),
                ("TERM", "xterm-256color")
            ]))
            .graphics
        );

        let iterm = Decor::read(&env(&[
            ("TERM", "xterm-256color"),
            ("TERM_PROGRAM", "iTerm.app"),
        ]));
        assert!(
            !iterm.progress,
            "a progress report would arrive as a pop-up"
        );
        assert_eq!(iterm.notify, Some(Notify::Osc9));

        // Windows Terminal names itself in a variable of its own rather than in TERM_PROGRAM,
        // and is not known to keep a title stack.
        let wt = Decor::read(&env(&[("TERM", "xterm-256color"), ("WT_SESSION", "…")]));
        assert!(wt.progress);
        assert_eq!(wt.notify, None);
        assert_eq!(wt.title, None);

        let apple = Decor::read(&env(&[
            ("TERM", "xterm-256color"),
            ("TERM_PROGRAM", "Apple_Terminal"),
        ]));
        assert!(!apple.progress);
        assert_eq!(apple.notify, None);
        assert_eq!(apple.title, None);
    }

    #[test]
    fn nothing_interpolated_can_end_the_sequence_it_is_inside() {
        assert_eq!(text("node_modules\x07;rm -rf /"), "node_modules;rm -rf /");
        assert_eq!(text("a\x1b]0;b"), "a]0;b");
    }
}
