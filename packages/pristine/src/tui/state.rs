//! The live view's whole state: what is open, what is marked, and where the cursor is.
//!
//! Nothing here touches the terminal or the filesystem. Every rule the front end has is a
//! function of a [`Tree`] and some keystrokes, which is what makes them assertable one at a
//! time — "marking a collapsed row marks everything under it" is a test rather than a
//! screenshot.
//!
//! # The cursor is anchored to a path, never to a row
//!
//! pua anchors its cursor to `(pid, start_time)` because processes vanish under a live cursor.
//! Here the equivalent event is **deletion**: rows disappear as the deleter finishes each
//! target, and a cursor holding a row *index* would slide onto whatever fell into that
//! position — which, under a held key, deletes something nobody chose.
//!
//! So a keystroke or an arrival re-walks the tree and puts the cursor back on the **path** it
//! was on. If that path is gone the chain of its ancestors is tried in turn, which lands on the
//! nearest surviving directory rather than on the other side of the screen — usually the
//! project whose `node_modules` was just deleted, and at worst the scan root, which is the last
//! rung of the chain and still somewhere the reader was. If none of them is on screen at all —
//! which a filter can do, where a deletion cannot — the cursor is **deselected**, visibly, so
//! the next arrow key picks a row deliberately.
//!
//! There is no index fallback, and that is the point rather than an omission: clamping the old
//! index is the same mis-selection one step removed. The two outcomes above are both statements
//! about a *directory*; row 0 is a statement about a position, and after a deletion the two
//! name different things.
//!
//! # Marks are subtree roots, not a set of rows
//!
//! Marking a collapsed row has to mark everything beneath it — that is the whole reason the
//! rollup is worth having — and the tree it covers can still be *growing* while the scan runs.
//! Storing the covered claims would mean a set of 8,660 ids for one keystroke and a set that
//! silently missed whatever arrived afterwards. So a mark is the **root of a marked subtree**,
//! a row is marked when it or any ancestor is one, and a row is partial when a mark sits
//! somewhere below it. A claim that streams in under a marked directory is marked on arrival,
//! which is what a reader who marked that directory asked for.
//!
//! Unmarking one row out of a marked subtree is the interesting case, and it is why the marks
//! are not just a set: the ancestor's mark is **pushed down**, replaced by marks on everything
//! beside the path to the row being spared. "Mark all, then keep this one" is a real workflow —
//! npkill's select-all exists for the first half of it — and the alternative is telling a
//! reader to clear forty marks and start again.
//!
//! # What the footer says is transient, and has to be able to stop being said
//!
//! A [`Notice`] is a report about something that has already happened, drawn in permanent
//! furniture: the footer, which otherwise carries the keys. So a report with no way out is a
//! stale claim sitting on the one line that tells a reader what they can do — and the older it
//! gets the less of the tree it still describes. See [`Notice`] for how long one lasts and why.
//!
//! # The clock is handed in, like everything else
//!
//! This file animates ([`super::moving`]) and still has no terminal and no filesystem in it,
//! because time arrives the same way a keystroke does: [`View::animate`] is given the instant
//! and everything else reads it off [`View`]. So "a removed row empties for a third of a
//! second and then collapses away" is an assertion with three `advance`s in it rather than a
//! test that sleeps, and the drain's *consequences* — a row that can no longer be marked,
//! deleted a second time, or counted into a batch — are assertions too.
//!
//! The one thing on screen that is deliberately **not** on that clock is the notice. Everything
//! the clock drives is a number moving towards a fact the reader can still go and check; a
//! report of what was destroyed is the one thing they cannot, so its lifetime is a reader's
//! action instead. See [`Notice`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use regex::Regex;

use super::keymap::{Action, Motion, Overlay, Turn};
use super::lens::{Lens, Preset};
use super::moving::Moving;
use super::treemap::Maps;
use crate::delete::{Plan, Refused, Target};
use crate::rules::Kind;
use crate::size::{Size, human};
use crate::tree::{NodeId, Order, Sort, Tree};
use crate::walk::Hit;

/// Rows one turn of the wheel moves the viewport.
///
/// Three rather than one because a wheel notch that moved a single row reads as a tool that
/// is not answering, and rather than a page because a page is what `Ctrl-d` is for: the wheel
/// is how a reader looks around without losing their place.
const WHEEL: usize = 3;

/// One line of the tree, once the collapsed subtrees have been left out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    /// Which directory.
    pub id: NodeId,
    /// How deep, for the indent. The root is 0.
    pub depth: usize,
}

/// How much of a row's subtree is marked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// Nothing under here.
    None,
    /// Some of it — the state an ancestor shows when only part of it is spoken for.
    Partial,
    /// This row and everything beneath it.
    All,
}

/// What a row's subtree is worth: the three numbers every total on the screen is made of.
///
/// Carried together because a filter has to be able to answer all three about *what it shows*
/// rather than about what is there, and answering two of them consistently is not enough — a
/// selection stated in bytes that came from one set and directories that came from another is
/// the arithmetic a reader would catch first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Roll {
    /// Measured reclaimable bytes.
    pub bytes: u64,
    /// Claims, priced or not.
    pub claims: usize,
    /// How many of those claims nobody has put a number on.
    pub unpriced: usize,
}

impl Roll {
    /// What the size column shows: a number, or a dash when there is nothing to add up yet.
    ///
    /// The distinction the performance thesis forces onto the front end. A default scan
    /// prices 8% of what it finds, so `0` and "not looked" are wildly different facts about a
    /// row and rendering both as `0 B` would report a 40 GiB `node_modules` as empty.
    ///
    /// There is a **third** state between those two, and it is the ordinary one for an
    /// ancestor while the pool works: some of what is under here is priced and some is not.
    /// `4.2 GiB` for a row that is really worth 4.9 GiB is wrong in the direction a cleaner
    /// must not be wrong in, so it is spelled `> 4.2 GiB`. The `>` is true the whole time it
    /// is up, it costs nothing, and it is what turns the number climbing underneath it into
    /// information rather than a number that cannot make its mind up.
    #[must_use]
    pub fn label(&self) -> String {
        match (self.bytes, self.unpriced) {
            (0, 1..) => Size::Unmeasured.label(),
            (bytes, 1..) => format!("> {}", human(bytes)),
            (bytes, 0) => human(bytes),
        }
    }
}

/// How far through its batch a running removal is.
///
/// A running byte total is the wrong thing on its own, and real use is what showed it: bytes
/// say how much has gone but not how much is left to go, so a reader watching a long delete
/// cannot tell a third of the way through from nearly finished. A **count of targets against
/// the batch's own total** can, and this is the one phase of the run where the denominator is
/// honest without qualification — it is fixed the instant the reader answers the confirmation,
/// where the pricing bar's denominator grows as the walk finds claims faster than the pool can
/// price them.
///
/// It is a lower bound, and deliberately so. The deleter speaks only for a target something
/// actually *happened* to, so a batch where one turned out to be gone already ends at eleven of
/// twelve rather than counting a directory nobody touched. The state ends when the batch
/// reports, not when the count reaches its total.
///
/// # …and a count on its own is not enough either, which took a real batch to learn
///
/// The paragraph above is right that bytes cannot say how much is left. What it missed is that
/// a count cannot say how much is left *either*, because targets are not the same size — and
/// they are not close. A real `pristine ~` batch of 2,188 directories sat at **2,162 of 2,188,
/// 98%** for over an hour, because the small ones drain first and the twenty-six still going
/// were most of the bytes. Every figure on the screen was true and the reader still could not
/// tell it from a hang.
///
/// So there are two, and they answer the two different questions a reader has: [`percent`] is
/// how far through the *list*, [`weighed`] is how much of the *weight*, and [`busiest`] names
/// the one target that decides when it ends. Neither number is the other's approximation.
///
/// [`percent`]: Removing::percent
/// [`weighed`]: Removing::weighed
/// [`busiest`]: Removing::busiest
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removing {
    /// Every target the confirmed plan handed to the deleter, and where each has got to.
    targets: HashMap<PathBuf, Live>,
    /// Targets the confirmed plan handed to the deleter. Held rather than counted off the map
    /// above, which collapses a plan that named one target twice — and a denominator that
    /// quietly shrank would make the batch smaller than the dialog promised.
    total: usize,
    /// Targets the deleter has reported finishing with, whole or in part.
    done: usize,
    /// What the plan said the whole batch was worth, which is only the part anybody had priced.
    /// Zero when none of it was, which is the state a default scan leaves most batches in.
    planned: u64,
}

/// Where one target of a batch has got to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Live {
    /// What the plan thought this target was worth, or zero when nothing had priced it.
    planned: u64,
    /// The latest cumulative figure it has reported. Assigned rather than added to, because
    /// [`crate::Freeing::bytes`] is a running total and never a delta — which is what makes a
    /// consumer that misses a report or coalesces two of them exact anyway.
    freed: u64,
    /// Whether the pool has moved off it.
    swept: bool,
}

impl Removing {
    /// The start of a batch, given what the plan thought each of its targets was worth.
    fn new(targets: &[(PathBuf, u64)]) -> Self {
        Self {
            total: targets.len(),
            done: 0,
            planned: targets.iter().map(|(_, planned)| planned).sum(),
            targets: targets
                .iter()
                .map(|(path, planned)| {
                    let live = Live {
                        planned: *planned,
                        ..Live::default()
                    };
                    (path.clone(), live)
                })
                .collect(),
        }
    }

    /// Notes one more target the deleter has come back out of.
    ///
    /// Capped at the total rather than allowed past it: the count is a position in a batch of
    /// known size, and a `13 of 12` would say the batch was not what the confirmation said it
    /// was — which is the one thing the dialog promises.
    ///
    /// The count moves whether or not `path` is one this batch knows about. That is deliberate
    /// and it is load-bearing: the position is the one figure here that needs no path to be
    /// right, so it stays right even if every path-keyed thing beside it stops matching.
    fn finished(&mut self, path: &Path) {
        self.done = self.done.saturating_add(1).min(self.total);
        if let Some(live) = self.targets.get_mut(path) {
            live.swept = true;
        }
    }

    /// Bytes one target has given back so far, as a running total.
    fn freeing(&mut self, path: &Path, bytes: u64) {
        if let Some(live) = self.targets.get_mut(path) {
            live.freed = bytes;
        }
    }

    /// Targets done, and how many there are.
    #[must_use]
    pub fn counted(&self) -> (usize, usize) {
        (self.done, self.total)
    }

    /// How far through, for the footer and for the dock.
    ///
    /// **Targets rather than bytes, and that is not an oversight.** A batch that failed on every
    /// one of its targets has still been worked through, and a bar weighted by bytes would read
    /// 0% for the whole of it — which reports the *outcome* under the guise of the position.
    /// What bytes are good for is saying how much is left, and [`Removing::weighed`] says that
    /// beside this rather than instead of it.
    #[must_use]
    pub fn percent(&self) -> u8 {
        percent(self.done, self.total)
    }

    /// Bytes given back so far against what the plan expected of the whole batch, or `None`
    /// when nothing in it was priced and there is no denominator to give.
    ///
    /// This is the half of the answer a count cannot give. Targets vary in size by four orders
    /// of magnitude, so "2162 of 2188" says nothing about whether the remainder is a second or
    /// an hour — and the last few targets of a real batch are routinely most of its bytes.
    #[must_use]
    pub fn weighed(&self) -> Option<(u64, u64)> {
        (self.planned > 0).then(|| (self.freed(), self.planned))
    }

    /// Bytes the batch has given back so far, across every target in it.
    #[must_use]
    pub fn freed(&self) -> u64 {
        self.targets.values().map(|live| live.freed).sum()
    }

    /// The target the batch is most likely to be waiting on: the largest one the pool has
    /// started and not yet moved off.
    ///
    /// A removal runs its targets concurrently, so there is no single current one — but there
    /// is one that decides when the batch ends. A target is swept by a single thread, so once
    /// the pool has more threads than targets left the finish time is the largest survivor's,
    /// and that is the name worth drawing. It changes only when that target is done, where
    /// naming the most recent report would flicker between unrelated paths several times a
    /// second — motion that is not information.
    ///
    /// Weighed by what the plan thought each was worth, falling back to what each has already
    /// given back when nothing priced them: on an unpriced batch the target that has freed the
    /// most is the best available guess at the biggest. The path breaks the remaining ties, so
    /// that two equal targets do not swap the name between frames.
    #[must_use]
    pub fn busiest(&self) -> Option<&Path> {
        self.targets
            .iter()
            .filter(|(_, live)| !live.swept && live.freed > 0)
            .max_by_key(|(path, live)| (live.planned, live.freed, *path))
            .map(|(path, _)| path.as_path())
    }

    /// What the footer says: where the deleter is, and how much of the batch's weight that
    /// leaves. The name of what it is working on is drawn beside this rather than folded in,
    /// because only the renderer knows how much room is left for a path.
    #[must_use]
    pub fn label(&self) -> String {
        let weight = match self.weighed() {
            Some((freed, planned)) => format!(" · {} of {}", human(freed), human(planned)),
            None => String::new(),
        };
        format!(
            "removing {} of {} · {}%{weight}",
            self.done,
            plural(self.total, "directory", "directories"),
            self.percent()
        )
    }
}

/// `part` of `whole` as a percentage, saturating rather than wrapping.
///
/// Lives here rather than beside its other caller in [`super::chrome`] so that the dependency
/// runs the way the layering does: the chrome reads the view, and the view knows nothing about
/// a terminal. One implementation, because a footer and a taskbar bar that rounded differently
/// would be two claims about the same run.
pub(super) fn percent(part: usize, whole: usize) -> u8 {
    if whole == 0 {
        return 0;
    }
    let scaled = part.saturating_mul(100) / whole;
    u8::try_from(scaled.min(100)).unwrap_or(100)
}

/// One directory a resolved plan is going to remove, as the confirmation needs it.
///
/// The half of a [`crate::delete::PlanTarget`] this screen reads, restated so the screen can
/// be driven without a filesystem: both spellings of the path, and what the scan priced it at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Planned {
    /// The path as the scan spelled it, which is what the tree holds.
    pub requested: PathBuf,
    /// The path the deleter will unlink.
    pub resolved: PathBuf,
    /// What the scan knew about its size.
    pub size: Size,
}

impl Planned {
    /// A target whose two spellings are the same, which is every target outside a symlinked
    /// ancestor.
    #[must_use]
    pub fn at(path: impl Into<PathBuf>, size: Size) -> Self {
        let path = path.into();
        Self {
            requested: path.clone(),
            resolved: path,
            size,
        }
    }
}

/// One line of the batch a confirmation lists.
///
/// Everything a reader needs in order to recognise a directory they marked several views ago:
/// where it is, what it is, what it is worth, whether they can currently *see* it, and whether
/// the safety model is going to refuse it anyway.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Which directory, by identity. What `space` on this line acts on, for the reason every
    /// other action in this file names a [`NodeId`]: the tree moves while a dialog is up.
    ///
    /// `None` for a path the tree no longer holds, which is a line that can be read and not
    /// unmarked — the honest state rather than a line that quietly does nothing.
    pub id: Option<NodeId>,
    /// Where it is, spelled as the scan spelled it — which is what the tree holds and what a
    /// reader recognises.
    pub path: PathBuf,
    /// The **resolved** path the deleter would unlink, when this line is going to be removed
    /// at all. `None` on a line the safety model refused.
    ///
    /// Two paths rather than one because the planner resolves `..` and symlinked ancestors,
    /// and on macOS that is not exotic: a scan of `/var/…` plans against `/private/var/…`.
    /// Taking a line out of the batch has to name the same path the deed does, or the deed
    /// would keep a directory the listing had stopped showing.
    pub target: Option<PathBuf>,
    /// What it is, which is also what the listing groups by. `None` is the tier-two claim's
    /// own content rather than a gap: nothing named it.
    pub kind: Option<Kind>,
    /// The ecosystem and the kind, as a row of the tree would say it.
    pub label: String,
    /// What the scan priced it at, which on a default scan is nothing.
    pub size: Size,
    /// Whether the **current** view is hiding it. The point of the screen: a reader marks
    /// broadly under one view, narrows, forgets, and is about to confirm a deletion whose
    /// contents they cannot see.
    pub hidden: bool,
    /// Why the safety model is going to leave it standing, if it is.
    ///
    /// Said *here*, before the reader commits, rather than in the post-run report — which is
    /// the same refusal reporting, moved to the moment it can still change a decision.
    pub kept: Option<String>,
}

/// The question the delete key asks, and everything it is holding while it asks.
///
/// It carries the **targets the plan resolved**, not "whatever is marked when the answer is
/// taken". The tree moves while a dialog is up — claims arrive, prices land, an earlier
/// deletion finishes — and a deed that re-read the marks at the moment of the answer would
/// remove a different set from the one the box described.
///
/// # It lists the batch, and that is the safety half of orthogonal selection
///
/// A selection that is independent of what is visible creates a hazard that did not exist when
/// the two were the same thing, and the mitigation is that the box **shows what it is holding**
/// — grouped by kind, with the hidden entries named as hidden and every one of them
/// unmarkable from here. The answer to a surprise has to be better than "cancel and start
/// again".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    /// Exactly what will be removed.
    pub targets: Vec<PathBuf>,
    /// Every directory the batch touched, refused ones included — what the listing draws.
    pub entries: Vec<Entry>,
    /// What the plan says that is worth, which is only the part anybody has priced.
    pub bytes: u64,
    /// How many of the targets carry no price.
    pub unpriced: usize,
    /// How the view that is hiding some of this spells itself, so the warning can name it.
    pub view: String,
    /// Which line the reader is on.
    at: usize,
    /// The first line drawn. Held here rather than derived, so a listing does not jump under
    /// a reader moving back up it.
    scroll: usize,
    /// How many lines the box has room for. The renderer owns the number and tells the view,
    /// exactly as it does for the tree's own viewport.
    page: usize,
    /// Which answer is highlighted. Starts on cancel — the key a reader presses to get rid of
    /// what is in front of them has to be the safe one.
    pub answer: Answer,
}

impl Pending {
    /// The lines, in the order they are drawn: grouped by kind, and by path inside a group.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Which line the cursor is on.
    #[must_use]
    pub fn at(&self) -> usize {
        self.at
    }

    /// The first line drawn.
    #[must_use]
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// How many lines the box has room for.
    #[must_use]
    pub fn page(&self) -> usize {
        self.page
    }

    /// How many of the entries the current view is hiding.
    #[must_use]
    pub fn hidden(&self) -> usize {
        self.entries.iter().filter(|entry| entry.hidden).count()
    }

    /// How many lines of this batch are things nothing brings back.
    ///
    /// Counted over the lines that are actually going to be removed, refusals excluded: a
    /// directory the safety model is leaving standing is not one this warning is about, and
    /// counting it would put a red line over a batch that takes nothing precious.
    #[must_use]
    pub fn unrecoverable(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.kept.is_none() && entry.kind == Some(Kind::Unrecoverable))
            .count()
    }

    /// How many the safety model will leave standing.
    #[must_use]
    pub fn kept(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.kept.is_some())
            .count()
    }

    /// The line under the cursor.
    fn current(&self) -> Option<&Entry> {
        self.entries.get(self.at)
    }

    /// Moves the cursor, and the listing under it.
    fn walk(&mut self, motion: Motion) {
        let Some(last) = self.entries.len().checked_sub(1) else {
            return;
        };
        let page = self.page.max(1);
        self.at = match motion {
            Motion::Up => self.at.saturating_sub(1),
            Motion::Down => (self.at + 1).min(last),
            Motion::PageUp => self.at.saturating_sub(page),
            Motion::PageDown => (self.at + page).min(last),
            Motion::Top => 0,
            Motion::Bottom => last,
        };
        self.follow();
    }

    /// Keeps the drawn window over the cursor, and inside the entries either way.
    fn follow(&mut self) {
        let page = self.page.max(1);
        if self.at < self.scroll {
            self.scroll = self.at;
        } else if self.at >= self.scroll + page {
            self.scroll = self.at + 1 - page;
        }
        self.scroll = self.scroll.min(self.entries.len().saturating_sub(1));
    }

    /// Takes one line out of the batch: the deed shrinks with the listing, because a dialog
    /// that showed one thing and removed another would be the failure this type exists to
    /// prevent.
    fn drop_at(&mut self, at: usize) -> Option<Entry> {
        if at >= self.entries.len() {
            return None;
        }
        let entry = self.entries.remove(at);
        // Matched on the requested path because that is what the deed carries — see
        // [`Pending::targets`]. `target` is still what says whether this line is a target at
        // all, which a refusal is not.
        if entry.target.is_some() {
            self.targets.retain(|path| path != &entry.path);
        }
        self.bytes = self.bytes.saturating_sub(entry.size.bytes().unwrap_or(0));
        if entry.kept.is_none() && entry.size.bytes().is_none() {
            self.unpriced = self.unpriced.saturating_sub(1);
        }
        self.at = self.at.min(self.entries.len().saturating_sub(1));
        self.follow();
        Some(entry)
    }
}

/// The two answers a confirmation has.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Answer {
    /// Leave everything where it is. Where the highlight starts, always.
    #[default]
    Cancel,
    /// Go ahead.
    Delete,
}

impl Answer {
    /// Both answers, in the order the box draws them — safe one first, which is also the
    /// order the arrow keys move through and the order a click is resolved against.
    pub const ALL: [Self; 2] = [Self::Cancel, Self::Delete];

    /// The word on the button.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Cancel => "cancel",
            Self::Delete => "delete",
        }
    }

    /// Which way the highlight has to move to land on this answer.
    ///
    /// The pointer aims by naming an answer and the keyboard aims by turning; this is the one
    /// place the two are reconciled, so a hover and a `→` cannot end up meaning different
    /// things.
    #[must_use]
    pub fn turn(self) -> Turn {
        match self {
            Self::Cancel => Turn::Prev,
            Self::Delete => Turn::Next,
        }
    }
}

/// The filter prompt, while it is up.
///
/// Separate from the applied filter because they are different facts: what is being typed and
/// what is being shown. A prompt that wrote straight through would re-walk the tree on every
/// keystroke of a pattern that is not finished, and `Esc` would have nothing to restore.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prompt {
    /// Characters rather than bytes, because the caret moves by character and a path is
    /// arbitrary Unicode.
    chars: Vec<char>,
    caret: usize,
    /// What the regex engine said about the last thing submitted, if it said no.
    error: Option<String>,
}

impl Prompt {
    /// A prompt holding `seed`, with the caret at the end.
    fn seeded(seed: &str) -> Self {
        let chars: Vec<char> = seed.chars().collect();
        Self {
            caret: chars.len(),
            chars,
            error: None,
        }
    }

    /// What has been typed.
    #[must_use]
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    /// Which character the caret is before.
    #[must_use]
    pub fn caret(&self) -> usize {
        self.caret
    }

    /// Why the last pattern was refused, if it was.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// One mark: a directory, and the view the reader was looking through when they made it.
///
/// **A mark cannot be stored as "the subtree under N", and that is the load-bearing
/// constraint.** Toggling what is visible must never change what is selected, so if a mark
/// were only a node, re-deriving what it covers under a different view would silently change
/// the batch — which is precisely the behaviour being ruled out. Baking the lens into the mark
/// is what makes switching views *inert*.
///
/// It is a pair resolved on demand rather than a frozen list of ids for a reason specific to
/// this tool: **results stream in**. A subtree marked at seven seconds would otherwise never
/// include the claims that arrive at forty, and "mark this directory" plainly means the
/// directory rather than the eleven things anybody had found under it so far.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Marked {
    /// Which directory. A [`NodeId`], never a row index — rows re-sort as prices land and
    /// vanish as removals complete, so an index taken now and acted on later names a
    /// different directory.
    root: NodeId,
    /// What "everything under it" meant when the reader said it.
    lens: Lens,
}

/// What one node is worth, three ways.
///
/// Carried together because they are three answers to one traversal and because two of them
/// disagreeing is the arithmetic a reader would catch first. The pair that has to be allowed
/// to differ is [`visible`](Self::visible) against [`all`](Self::all): a selection made
/// through one view and read under another is exactly the hazard the confirmation exists to
/// mitigate, and hiding it by making the batch filter-relative would contradict "retained".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counts {
    /// Claims under this node that survive the current lens — what its row draws.
    visible: Roll,
    /// Of those, the ones the marks select. What the mark glyph is computed from, so the
    /// glyph describes **what the reader can see** rather than a global fact that would
    /// contradict it.
    chosen: Roll,
    /// Everything the marks select under this node, visible or not: the batch, and the number
    /// the footer states.
    all: Roll,
}

impl Counts {
    /// Folds a child in.
    fn absorb(&mut self, child: Self) {
        add(&mut self.visible, child.visible);
        add(&mut self.chosen, child.chosen);
        add(&mut self.all, child.all);
    }
}

/// Adds one roll into another.
fn add(into: &mut Roll, roll: Roll) {
    into.bytes += roll.bytes;
    into.claims += roll.claims;
    into.unpriced += roll.unpriced;
}

/// A node [`tally`] is holding a mark for.
const MARKED: u8 = 1;
/// A node the reader has individually spared.
const SPARED: u8 = 2;

/// A step of the one traversal that answers everything the marks and the lens decide.
enum Step {
    /// On the way down, carrying how deep this node is.
    Enter(NodeId, usize),
    /// On the way back up, where the children's numbers are already in.
    Leave(NodeId, usize),
}

/// Walks the whole tree once, saying what each node is worth and which claims are selected.
///
/// # Why one pass rather than three
///
/// The rolled-up numbers are recomputed rather than read off the tree, and that is a **safety**
/// property before it is a cosmetic one: a row showing 312 GiB while the view hides all but 2
/// GiB of it is a row whose glyph would claim to describe something it does not. Whole, on
/// every change, rather than incrementally — the scan streams claims into arbitrary places in
/// the tree, so an incremental update would have to be right about every arrival, every price
/// and every deletion, which is three chances to leave a stale number on a row that a mark then
/// acts on.
///
/// The selection is folded into the same pass rather than derived afterwards, because the
/// counter and the batch **must** be the same set. Two traversals that could disagree is the
/// bug this file already refuses everywhere else.
///
/// # Which marks cover a claim, and which spare it
///
/// Both are carried down the path rather than looked up per claim, so this stays O(the tree)
/// with a handful of live entries rather than O(claims × marks × depth). They are kept with
/// their **depths** because the two interleave: a reader can mark `~/repos`, spare
/// `~/repos/a`, and then mark `~/repos/a/b` again — and the deepest thing on the path is the
/// one that speaks. A shallower mark cannot reach back through a spare below it.
fn tally(
    tree: &Tree,
    lens: &Lens,
    marks: &[Marked],
    spared: &HashSet<NodeId>,
    moving: &Moving,
    out: &mut Tallied,
) {
    let Tallied {
        counts,
        selection,
        map_stamps: stamps,
    } = out;
    // Indexed by [`NodeId`] rather than hashed on it, and that is the difference between a
    // fifth of a frame and a whole one: this runs over 32,634 nodes and the ids are dense —
    // the tree only ever pushes a slot and never recycles a detached one. A `HashMap` here
    // costs four `SipHash`es per node for what an index answers for nothing.
    counts.clear();
    counts.resize(tree.minted(), Counts::default());
    selection.clear();
    // One byte per node saying whether anything at all happens here, so the O(marks) scan that
    // finds *which* mark only runs on the handful of nodes that carry one.
    let mut flags = vec![0u8; tree.minted()];
    for mark in marks {
        flags[mark.root] |= MARKED;
    }
    for &id in spared {
        flags[id] |= SPARED;
    }
    let mut covering: Vec<(usize, &Lens)> = Vec::new();
    let mut sparing: Vec<usize> = Vec::new();
    // An explicit stack rather than recursion: the depth here is the filesystem's, and nothing
    // stops a checkout from being nested far deeper than the ten levels a real home directory
    // reaches.
    let mut stack = vec![Step::Enter(tree.root(), 0)];
    while let Some(step) = stack.pop() {
        match step {
            Step::Enter(id, depth) => {
                if flags[id] & MARKED != 0 {
                    covering.extend(
                        marks
                            .iter()
                            .filter(|mark| mark.root == id)
                            .map(|mark| (depth, &mark.lens)),
                    );
                }
                if flags[id] & SPARED != 0 {
                    sparing.push(depth);
                }
                stack.push(Step::Leave(id, depth));
                for &child in &tree.node(id).children {
                    stack.push(Step::Enter(child, depth + 1));
                }
            }
            Step::Leave(id, depth) => {
                let node = tree.node(id);
                let mut here = Counts::default();
                // The stamps of the children this node's rectangles are divided among, added
                // rather than chained: [`Tree::sort_by`] moves children about and the map
                // orders its own rectangles by weight, so a fold that could see sibling order
                // would redraw a megabyte on `s` to show the same picture.
                let mut beneath = 0u64;
                if let Some(hit) = &node.hit {
                    let roll = Roll {
                        bytes: node.reclaimable,
                        claims: 1,
                        unpriced: node.unmeasured,
                    };
                    let seen = lens.matches(hit);
                    if seen {
                        here.visible = roll;
                    }
                    let deepest = sparing.last().copied();
                    // **A mark is a statement about a subtree, and it has no exceptions.**
                    // Every claim under it that the mark's own lens accepts is covered,
                    // whatever kind it is. An earlier pass excepted [`Kind::Unrecoverable`]
                    // unless the mark sat at the claim's exact depth, and that was wrong twice:
                    // the fractional glyph on an ancestor reads as the share of the subtree
                    // that is spoken for, so a mark quietly skipping descendants makes it
                    // describe a set nobody can see — and the exception had no spelling
                    // anywhere a reader could find it.
                    //
                    // What keeps something precious out of a bulk mark is upstream of here and
                    // needs nothing added: a mark carries the lens it was made through, and no
                    // lens shows gitignored files until `i` says so. Seeing one at all is the
                    // deliberate act; after that it is a row like any other.
                    let chosen = covering.iter().any(|&(at, mark)| {
                        deepest.is_none_or(|spared| at > spared) && mark.matches(hit)
                    });
                    // The deleter's two phases come off the selection at different moments,
                    // and both timings are load-bearing. A target part way through is a
                    // directory that **still exists**: its bytes have gone, so the counter
                    // says so, but "how many directories" must not drop until the sweep says
                    // it has finished — otherwise the footer reports a directory deleted
                    // while it is being deleted. It is out of the *batch* from the first byte,
                    // though: offering a directory that is already going to a second removal
                    // would report a failure for the one thing that worked.
                    if chosen && !moving.is_spent(id) {
                        let counted = Roll {
                            bytes: roll.bytes.saturating_sub(moving.freed_from(id)),
                            ..roll
                        };
                        here.all = counted;
                        if seen {
                            here.chosen = counted;
                        }
                        if !moving.is_leaving(id) {
                            selection.push(id);
                        }
                    }
                } else {
                    for &child in &node.children {
                        here.absorb(counts[child]);
                        // Only the children the map can see. A claim the lens hides is not a
                        // rectangle, so a claim of that kind arriving must not read as the
                        // picture having changed — which is the whole reason this is folded
                        // here, on the lens-aware pass, rather than read off the tree.
                        if !stamps.is_empty() && counts[child].visible.claims > 0 {
                            beneath = beneath.wrapping_add(stamps[child]);
                        }
                    }
                }
                counts[id] = here;
                if !stamps.is_empty() {
                    stamps[id] = stamp_of(id, here, beneath);
                }
                if flags[id] & MARKED != 0 {
                    covering.retain(|&(at, _)| at != depth);
                }
                if flags[id] & SPARED != 0 {
                    sparing.pop();
                }
            }
        }
    }
}

/// What one pass of [`tally`] writes: three answers to one traversal of the tree.
///
/// Carried together because they are read together and because two of them from different
/// passes would describe two different trees — the same reason [`Counts`] is one struct rather
/// than three parallel numbers.
#[derive(Debug, Default)]
struct Tallied {
    /// What every node is worth under the current view, and what of that the marks select.
    counts: Vec<Counts>,
    /// Every claim the marks select, whichever view each was marked through.
    selection: Vec<NodeId>,
    /// See [`View::map_stamp`]. Left empty when nothing is drawing a map, which is how the
    /// pass is told not to fold one.
    map_stamps: Vec<u64>,
}

/// One node's contribution to [`View::map_stamp`]: everything [`super::treemap`] reads about
/// it, and the stamps of the children it divides its rectangle among.
///
/// Exactly what the map reads and nothing else. [`Counts::visible`] is what `roll` answers, so
/// it is every rectangle's area and every label; [`Counts::chosen`] is what `mark_of` compares,
/// so it is the colour. [`Counts::all`] is deliberately absent — it is the batch, which the map
/// never draws, and folding it in would redraw the picture when a claim the lens hides was
/// selected under a mark.
///
/// The `id` goes in because a rollup can be identical across a change that swapped which
/// directory it came from: a claim arriving as another is deleted puts bytes, claims and
/// unpriced back exactly where they were, and the map is then of two different directories.
fn stamp_of(id: NodeId, counts: Counts, beneath: u64) -> u64 {
    // FNV-1a, which is two instructions a value against `DefaultHasher`'s SipHash — this runs
    // once per node per frame over 32,634 of them, so the hash has to cost less than the
    // redraw it exists to avoid. What it buys over a plain sum is diffusion: `beneath` adds
    // its children commutatively, and a sum of poorly spread values collides easily.
    const SEED: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut stamp = SEED;
    for value in [
        id as u64,
        counts.visible.bytes,
        counts.visible.claims as u64,
        counts.visible.unpriced as u64,
        counts.chosen.claims as u64,
        beneath,
    ] {
        stamp = (stamp ^ value).wrapping_mul(PRIME);
    }
    stamp
}

/// What the event loop has to do about a keystroke, once the view has done its part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Nothing outside the view.
    None,
    /// Put the terminal back.
    Quit,
    /// Resolve these into a [`Plan`] and hand it back with [`View::ask`].
    ///
    /// The view never plans, because planning is filesystem work — every path resolved, every
    /// check in the safety model applied — and this file has none.
    ///
    /// [`Target`]s rather than paths, and the difference is the whole answer to "how much do
    /// I get back": a target carries what the scan priced, and one built from a bare path
    /// carries [`Size::Unmeasured`]. Handing the planner paths made the confirmation offer to
    /// delete 196 KiB of `node_modules` "giving back 0 B", with the tree saying otherwise two
    /// lines above it.
    Plan(Vec<Target>),
    /// The question was answered yes. Remove exactly these.
    Delete(Vec<PathBuf>),
    /// Put a size on these claims, which nobody has priced.
    ///
    /// Filesystem work, so the view asks for it rather than doing it — the same split
    /// [`Plan`](Self::Plan) draws. Paths and not [`Target`]s, because the whole point is that
    /// these carry no size yet: the answer comes back as the prices the walk would have sent.
    Price(Vec<PathBuf>),
}

/// One sentence for the footer, and how long it stays there.
///
/// # Why not a timer
///
/// A timer is the wrong answer for a report of what was **destroyed**. A reader who looked away
/// while it counted down has no way to get it back, and the thing it described is not on disk
/// any more — so the one state a countdown leaves them in is "something happened and nothing
/// will say what". Every lifetime here is therefore a *reader's* action rather than a clock.
///
/// # Two lifetimes, because the reports differ in what it costs to miss one
///
/// [`passing`](Self::passing) is an ordinary report — what was removed, what was priced, why a
/// key did nothing. The next thing the reader does takes it away, because by then it describes
/// the frame before rather than the one in front of them.
///
/// [`standing`](Self::standing) names something **refused or failed**. The safety model collects
/// those and the run exits non-zero on them, so a sentence that says a directory was left alone
/// or could not be removed is the only place a reader learns that from — and an arrow key
/// pressed while reading it must not be what takes it away. Nothing incidental clears one: it
/// goes when it is dismissed, or when a newer report answers a keystroke the reader has just
/// made.
///
/// Both are dismissed by `Esc` and by a press on the footer, which is the rung
/// [`View::step_back`] takes first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    said: String,
    /// Whether this one outlasts the reader's next action. See the type's docs.
    stands: bool,
}

impl Notice {
    /// An ordinary report, gone by the reader's next action.
    #[must_use]
    pub fn passing(said: impl Into<String>) -> Self {
        Self {
            said: said.into(),
            stands: false,
        }
    }

    /// One that names something refused or failed, and so waits to be dismissed.
    #[must_use]
    pub fn standing(said: impl Into<String>) -> Self {
        Self {
            said: said.into(),
            stands: true,
        }
    }

    /// The sentence itself.
    #[must_use]
    pub fn said(&self) -> &str {
        &self.said
    }

    /// Whether it waits to be dismissed rather than going with the reader's next action.
    #[must_use]
    pub fn stands(&self) -> bool {
        self.stands
    }
}

/// The live view.
#[derive(Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "five independent facts about one view — is the walk running, is a removal \
              running, do the rows still describe the tree, are the levels in order, was the \
              cursor taken away. The lint's advice is a state machine, and these do not form \
              one: every combination of them happens."
)]
pub struct View {
    tree: Tree,
    sort: Sort,
    /// Which rows are open. The root starts open and everything else closed, which is the
    /// whole premise: a machine's worth of reclaimable directories, shown as a handful of
    /// rows you drill into.
    expanded: HashSet<NodeId>,
    /// The marked subtrees: a directory each, and the view each was marked through. See
    /// [`Marked`].
    marks: Vec<Marked>,
    /// How many times the selection — the marks or the exclusions — has changed. See
    /// [`View::mark_stamp`].
    mark_stamp: u64,
    /// Directories the reader unmarked individually out of a marked subtree.
    ///
    /// The other half of the model, and the reason a push-down is not needed: "mark the lot,
    /// then keep this one" used to mean marking every sibling along the path, which left a
    /// mark per sibling on a level 8,660 wide. An exclusion says the same thing in one entry
    /// and — unlike the push-down — keeps saying it as claims stream in underneath.
    spared: HashSet<NodeId>,
    /// What each node is worth, what of it is selected, and what of that can be seen, by
    /// [`NodeId`]. Rebuilt whole once per sync by [`tally`]; empty when there is nothing to
    /// compute, which is the view a run opens on.
    counts: Vec<Counts>,
    /// Every claim the marks select, whether or not the current view shows it. The batch, and
    /// the set the counter describes — one list, so the two can never disagree.
    selection: Vec<NodeId>,
    /// What the map under each node is drawn from, as one number. See [`View::map_stamp`];
    /// empty when nothing is drawing a map.
    map_stamps: Vec<u64>,
    rows: Vec<Row>,
    cursor: Option<usize>,
    /// Whether the cursor was deselected by something vanishing under it.
    ///
    /// Without this the "no index fallback" rule would last exactly one frame: the next sync
    /// would see an empty anchor, decide this was a view that had never been touched, and put
    /// the cursor back on row 0 — the scan root, whose subtree is everything.
    deselected: bool,
    scroll: usize,
    /// How many rows the tree pane can draw. The renderer owns the number and tells the view.
    page: usize,
    /// What is on screen: the two visibility axes and the `/` pattern, together.
    ///
    /// One value rather than a filter beside a mode, because a mark stores the whole of it —
    /// a mark made under `named · dependencies · /nx` has to keep meaning that when any part
    /// of it changes.
    lens: Lens,
    prompt: Option<Prompt>,
    help: Option<usize>,
    pending: Option<Pending>,
    /// Claims a pricing pass has been asked for and has not answered yet.
    ///
    /// The one piece of state that stops a gesture from being repeatable into unbounded
    /// work: a claim in here is one somebody is already traversing, so a second double click
    /// on the row above it asks for nothing. Emptied by [`View::repriced`] on every way a
    /// pass can end — see there for why that matters more than it looks.
    pricing: HashSet<PathBuf>,
    /// Whether the walk is still running, for the header.
    scanning: bool,
    /// The removal in flight, and where it has got to. A second one would race the first over
    /// the same tree, so its presence is also what refuses one.
    removing: Option<Removing>,
    /// Whether the reader has asked to leave and is waiting on a removal to finish.
    ///
    /// `q` is reserved everywhere, and it stays reserved — but it cannot *end* a run that is
    /// half way through unlinking a directory tree. The process leaving takes the pool with
    /// it, so what would be left on disk is neither the tree the reader had nor the one they
    /// asked for, and nothing would ever report which. So the keystroke is remembered instead
    /// of obeyed, and the loop leaves the moment the removal is over.
    quitting: bool,
    /// What just happened, for the footer to say — until something takes it away. See
    /// [`Notice`].
    notice: Option<Notice>,
    /// Whether the rows on hand still describe the tree.
    stale: bool,
    /// Whether the tree's levels are in the current sort order.
    sorted: bool,
    /// What is in flight on screen: see [`Moving`].
    moving: Moving,
    /// What has already left the disk from at or below each node, as of this frame. Rebuilt in
    /// [`View::animate`] from what the deleter has reported freeing.
    ///
    /// The bytes here are the deleter's own running total, so a row emptying is the disk
    /// emptying and not a timer dressed up as one. The **claims** move separately and later:
    /// a target part way through is a directory that still exists, and it stops being counted
    /// only when the sweep says it has finished with it.
    ///
    /// Bounded by the targets one removal has in flight, times the depth of the tree, and
    /// built once per frame rather than asked per row.
    drained: HashMap<NodeId, Roll>,
    /// This frame's instant. Handed in by [`View::animate`] and read by everything else, so
    /// nothing in this file calls a clock.
    now: Instant,
    /// When the view opened, which is what the shimmer's phase is measured from.
    opened: Instant,
    /// How many [`NodeId`]s the tree had handed out last time this looked, so the ones it has
    /// handed out since can be lit as new arrivals.
    seen: usize,
    /// Directories a removal left standing, and why.
    ///
    /// The safety model refusing a subtree is the tool **working**, so this is kept apart
    /// from the walk's errors and drawn calmly. A reader who marked forty directories and got
    /// thirty-eight has to be able to see which two, on the rows themselves, after the footer
    /// has moved on.
    kept: HashMap<NodeId, String>,
    /// What this session has given back, across every batch.
    freed: u64,
    /// The treemap pane: whether this terminal could draw one, and whether the reader wants
    /// it. Told to the view the way [`View::viewport`] is — the renderer owns the fact and
    /// the view owns the decision, so `m` has one place to act on and the layout has one
    /// place to read.
    map: Map,
}

/// Whether the map pane is possible, and whether it is on.
///
/// The first is [`Maps`] rather than a boolean, because the answer to `m` differs by *why*: a
/// reader on a terminal that cannot draw one has to be told which of the two reasons it is,
/// where a silent no-op on a documented key is the same failure shape as a mark box that
/// cannot be pressed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Map {
    possible: Maps,
    on: bool,
}

impl View {
    /// A view of a scan that has not found anything yet.
    #[must_use]
    pub fn new(tree: Tree) -> Self {
        let opened = Instant::now();
        let mut view = Self {
            expanded: HashSet::from([tree.root()]),
            // The root is not an arrival: it is the directory the reader typed, and lighting
            // it on the first frame would say something was found before anything was.
            seen: tree.minted(),
            tree,
            sort: Sort::default(),
            marks: Vec::new(),
            mark_stamp: 0,
            spared: HashSet::new(),
            counts: Vec::new(),
            selection: Vec::new(),
            map_stamps: Vec::new(),
            rows: Vec::new(),
            cursor: None,
            deselected: false,
            scroll: 0,
            page: 20,
            lens: Lens::default(),
            prompt: None,
            help: None,
            pending: None,
            pricing: HashSet::new(),
            scanning: true,
            removing: None,
            quitting: false,
            notice: None,
            stale: true,
            sorted: false,
            moving: Moving::new(opened),
            drained: HashMap::new(),
            now: opened,
            opened,
            kept: HashMap::new(),
            freed: 0,
            // On wherever it is possible, which is the spike's own bet: a feature nobody
            // turns on is a feature nobody judges.
            map: Map {
                possible: Maps::Unread,
                on: true,
            },
        };
        view.sync();
        view
    }

    /// Opens the view with gitignored files on screen, as `--ignored-files` asks for.
    ///
    /// The walk claims them either way — see [`super::spawn_walk`] — so this decides only where
    /// the lens starts. It exists because a flag that reads "claim gitignored files" and then
    /// does nothing a reader can see is a flag that has lied about itself: the two front ends
    /// have to mean the same thing by it, and in the tree "show me these" is what it means.
    #[must_use]
    pub fn showing_files(mut self) -> Self {
        self.lens = self.lens.with_files(true);
        self.stale = true;
        self.sync();
        self
    }

    // ---- what the workers report ------------------------------------------------------

    /// A claim the walk has just found.
    pub fn found(&mut self, hit: Hit) {
        self.tree.insert(hit);
        self.stale = true;
        self.sorted = false;
    }

    /// A pricing thread has gone into this claim.
    ///
    /// What the shimmer draws, and the reason it is worth drawing: the pool is bounded, so the
    /// rows lit at any instant are exactly the ones being worked on. A dash that is being
    /// measured right now and a dash that will be measured in four minutes are different facts
    /// about a row, and before this event the front end had no way to tell them apart.
    pub fn pricing(&mut self, path: &Path) {
        if let Some(id) = self.tree.find(path) {
            self.moving.heats(id);
        }
    }

    /// A price for a claim that was published without one.
    pub fn priced(&mut self, path: &Path, size: Size) {
        if let Some(id) = self.tree.find(path) {
            self.moving.cools(id);
        }
        self.tree.price(path, size);
        self.stale = true;
        self.sorted = false;
    }

    /// Bytes the deleter has just given back from a target it is still working through.
    ///
    /// This is what makes a row **empty** rather than merely disappear, and the reason it is
    /// an event rather than a timer: the number falling is the number of bytes that have
    /// actually left the disk. A fixed animation started after the fact would look the same
    /// for a target that took ten seconds and one that took ten milliseconds, which is the
    /// definition of motion that is not information.
    pub fn freeing(&mut self, path: &Path, bytes: u64) {
        // Before the tree lookup and outside it, because the two answer to different things:
        // the footer's arithmetic is about the batch, which is known in full, while the row is
        // about a node that may legitimately not be drawn. Folding the first into the second
        // makes a missing row silently cost the whole counter.
        if let Some(removing) = &mut self.removing {
            removing.freeing(path, bytes);
        }
        if let Some(id) = self.tree.find(path) {
            self.moving.frees(id, bytes);
            self.stale = true;
        }
    }

    /// A target the deleter has finished with.
    ///
    /// Only a *complete* removal takes the row away. A target the sweep entered and did not
    /// finish — a checkout inside it, an unreadable subtree — is still on disk, and dropping
    /// its row would tell a reader something was deleted that was not.
    ///
    /// A complete one does not take it away on this frame either. Its number is already at
    /// zero, having got there on the bytes reported by [`View::freeing`]; what it spends now
    /// is [`super::moving::DIM`] dimmed, so the row is seen to have emptied instead of
    /// vanishing on the same frame as its last byte. That beat is presentational and its
    /// consequences are not: the row is out of the batch and out of the marks from the moment
    /// the sweep first touched it.
    pub fn removed(&mut self, path: &Path, bytes: u64, complete: bool) {
        // The target's last word on itself, and for most targets its only one: a sweep reports
        // progress every 64 entries, so anything smaller than that finishes without ever having
        // said anything. A counter fed only by [`View::freeing`] would leave every small target
        // in a batch worth nothing — which on a real batch is most of them.
        //
        // Assignment rather than addition, and that is what makes this safe to do beside the
        // progress reports: both are the same running total read at different moments.
        if let Some(removing) = &mut self.removing {
            removing.freeing(path, bytes);
        }
        let Some(id) = self.tree.find(path) else {
            return;
        };
        if complete {
            self.moving.spends(id, bytes, self.now);
        } else {
            // Still on disk, and the bytes that did go are still gone. The row keeps its
            // place showing what is left of it, which is the honest reading of a sweep that
            // went in and came out again.
            self.moving.frees(id, bytes);
        }
        self.stale = true;
    }

    /// The deleter has moved off a target, whatever it managed to do to it.
    ///
    /// **This and [`View::removed`] answer different questions, which is why the progress
    /// counts here and the rows move there.** A target that failed before unlinking a single
    /// entry, or that had already vanished, is one the deleter is no longer working on — it
    /// belongs to where the batch has got to, and to nothing else. Counting the position on
    /// removals instead would leave a batch that failed on every target reading 0% for its
    /// whole life and then vanishing, which reports the *outcome* under the guise of the
    /// position; and even one such target leaves the bar permanently short of where the
    /// deleter actually is.
    ///
    /// It deliberately does not touch the tree. Nothing happened to that directory, so there
    /// is nothing for its row to say.
    pub fn swept(&mut self, path: &Path) {
        if let Some(removing) = &mut self.removing {
            removing.finished(path);
        }
    }

    /// Directories a removal left standing, with the reason each one was left.
    pub fn refused(&mut self, kept: &[Refused]) {
        for refused in kept {
            if let Some(id) = self.tree.find(&refused.path) {
                self.kept.insert(id, refused.reason.to_string());
            }
        }
    }

    /// The walk is over.
    pub fn scanned(&mut self) {
        self.scanning = false;
        // A pool that has stopped leaves nothing hot behind it.
        self.moving.cooled();
    }

    /// A pricing pass is over, and these are the claims it was holding.
    ///
    /// The prices themselves arrived one at a time through [`View::priced`], exactly as the
    /// walk's do; this hands the claims back, which is what lets the next double click on
    /// them mean something again. It is called on **every** way a pass can end, the worker
    /// dying included — an in-flight set that leaked would be a subtree the reader can never
    /// ask about again for the rest of the run, which is a quiet permanent no-op on a gesture
    /// they keep making.
    pub fn repriced(&mut self, claims: &[PathBuf], notice: Notice) {
        for claim in claims {
            self.pricing.remove(claim);
        }
        self.notice = Some(notice);
    }

    /// The removal is over, `notice` is what it did, and `freed` is what the session has given
    /// back across every batch it has run.
    ///
    /// The two are the same event told from two ends and they hand over here, which is why they
    /// are set in one call. While the batch runs the rows and the counter carry it — bytes
    /// falling as they leave the disk, a position against the batch's own size — and none of
    /// that survives the last target. `notice` is what is left saying anything at all, so it is
    /// the *only* place the counts a live row cannot show land: what the safety model refused,
    /// and what failed. How long it stays is picked from those same counts, by
    /// [`summarise`](crate::tui::summarise). See [`Notice`].
    ///
    /// The freed figure is **set** rather than added to, and it is the batch report's own
    /// arithmetic rather than a second tally kept here: the per-target totals the counter has
    /// been climbing on and [`crate::delete::Removal::bytes_freed`] are the same bytes counted
    /// by the same code, so keeping both would count each one twice. The running figures are
    /// dropped in the same breath that this one lands, which is why the counter hands over
    /// without so much as a flicker.
    ///
    /// **A target the sweep could not finish has to be reconciled into the tree first.** Its
    /// row is staying — the directory is still on disk — and what it is worth is what survived,
    /// which until this point has only ever been said by the deleter's progress. Progress is
    /// the thing being dropped here, so the reduction is made durable before it goes: a claim
    /// whose bytes went but whose row remains springs straight back to its original size
    /// otherwise, and the headline reclaimable figure *rises* after a partial delete while
    /// `freed` says those same bytes are gone. A complete removal needs none of this, because
    /// its claim leaves the tree outright when its dimmed beat is over.
    pub fn deleted(&mut self, notice: Notice, freed: u64) {
        for (id, bytes) in self.moving.leaving().collect::<Vec<_>>() {
            if self.moving.is_spent(id) {
                continue;
            }
            let path = self.tree.node(id).path.clone();
            self.tree.shrink(&path, bytes);
        }
        self.removing = None;
        self.notice = Some(notice);
        self.freed = freed;
        self.moving.banked();
        self.stale = true;
    }

    /// Opens the confirmation on a resolved plan.
    ///
    /// The listing is built here rather than by the planner, because half of what a line says
    /// is a fact about the *view* — what kind of artefact this is, and whether the reader can
    /// currently see it — and the planner knows only paths and the safety model's answers.
    /// The two halves meet exactly once, here.
    ///
    /// An empty plan is not a question. It happens for a real reason — every marked directory
    /// was refused by the safety model — so it says so rather than putting up a box with
    /// nothing in it.
    pub fn ask(&mut self, plan: &Plan) {
        let targets: Vec<Planned> = plan
            .targets()
            .iter()
            .map(|target| Planned {
                requested: target.requested.clone(),
                resolved: target.path.clone(),
                size: target.size,
            })
            .collect();
        self.asking(&targets, plan.kept());
    }

    /// The same question from the two lists a [`Plan`] *is*.
    ///
    /// Taken apart because a plan can only be built against a real filesystem, and the rule
    /// this screen has to keep — that the batch is stated whole, hidden entries and refusals
    /// included — is a rule about the view rather than about the disk.
    pub fn asking(&mut self, targets: &[Planned], kept: &[Refused]) {
        if targets.is_empty() {
            // A refusal, so it waits to be dismissed: this sentence is the *only* place a
            // reader learns that the safety model took their whole batch away. It is also the
            // one path on which the confirmation does not appear, so there is nothing else
            // left saying anything about the batch at all.
            if kept.is_empty() {
                self.says("nothing to delete");
            } else {
                self.warns(format!(
                    "nothing to delete: {} left alone by the safety model",
                    kept.len()
                ));
            }
            return;
        }
        let mut entries: Vec<Entry> = targets
            .iter()
            .map(|target| {
                self.entry(
                    &target.requested,
                    Some(target.resolved.clone()),
                    target.size,
                    None,
                )
            })
            .chain(kept.iter().map(|refused| {
                self.entry(
                    &refused.path,
                    None,
                    Size::Unmeasured,
                    Some(refused.reason.to_string()),
                )
            }))
            .collect();
        // By kind and then by path, which is what "groups by kind" means once the lines are
        // one list: the renderer names the kind wherever it changes rather than keeping a
        // second structure that could disagree about the order.
        entries.sort_by(|a, b| {
            kind_order(a.kind)
                .cmp(&kind_order(b.kind))
                .then_with(|| a.path.cmp(&b.path))
        });
        // Added up over the **entries** rather than over the plan, so the headline and the
        // lines under it are one arithmetic. They can differ: a target the scan priced and the
        // plan did not is a dash in the plan and a number on the row, and a box saying "giving
        // back 0 B" over a line saying 2.0 MiB is the disagreement a reader catches first.
        let priced = |entry: &&Entry| entry.kept.is_none();
        let bytes = entries
            .iter()
            .filter(priced)
            .filter_map(|entry| entry.size.bytes())
            .sum();
        let unpriced = entries
            .iter()
            .filter(priced)
            .filter(|entry| entry.size.bytes().is_none())
            .count();
        self.pending = Some(Pending {
            // **The requested spelling, not the resolved one**, and the difference is the whole
            // of #656's sibling bug. The deed is re-planned before it runs, so either spelling
            // reaches the same directory — but whichever goes in is what the deleter calls the
            // target when it reports back, and the view can only find a row by the name the
            // walk gave it. Hand the resolved path to the deed and every report comes back in
            // a spelling the tree has never heard of: no row empties, no row leaves, the
            // headline total never falls, and the only thing that still moves is the position,
            // because it is the one figure that needs no path.
            targets: targets
                .iter()
                .map(|target| target.requested.clone())
                .collect(),
            bytes,
            unpriced,
            entries,
            view: self.lens.describe(),
            at: 0,
            scroll: 0,
            page: 8,
            answer: Answer::Cancel,
        });
    }

    /// One line of the listing, with everything only the tree and the lens can say.
    fn entry(
        &self,
        path: &Path,
        target: Option<PathBuf>,
        size: Size,
        kept: Option<String>,
    ) -> Entry {
        let id = self.tree.find(path);
        let hit = id.and_then(|id| self.tree.node(id).hit.as_ref());
        Entry {
            id,
            path: path.to_path_buf(),
            target,
            kind: hit.and_then(Hit::kind),
            label: hit.map_or_else(
                || crate::walk::UNLABELLED.to_owned(),
                |hit| hit.label().into_owned(),
            ),
            // The plan's own figure where it has one, and the tree's otherwise: a refusal
            // carries no size, and a line saying nothing about what it is worth is a line a
            // reader cannot weigh.
            size: match size.bytes() {
                Some(_) => size,
                None => hit.map_or(Size::Unmeasured, |hit| hit.size),
            },
            // "Hidden" is a claim about the **view**, so a path the tree does not hold is not
            // one: a view that hides nothing must never be able to report that it is hiding
            // something, or the warning at the top of the box stops meaning anything.
            hidden: hit.is_some_and(|hit| !self.lens.matches(hit)),
            kept,
        }
    }

    // ---- what the renderer asks -------------------------------------------------------

    /// Brings the rows back in line with the tree, if anything has changed under them.
    ///
    /// Idempotent and cheap when nothing moved, so both the renderer and every keystroke can
    /// call it without either having to know whether the other did.
    pub fn sync(&mut self) {
        if !self.stale {
            return;
        }
        let anchor = self.anchor();
        if !self.sorted {
            self.tree.sort_by(self.sort);
            self.sorted = true;
        }
        // Everything the tree has minted since the last look is a directory the walk found
        // since the last look, so it is exactly the set of rows to light. The tree does not
        // have to report arrivals for this to be exact: ids are handed out in order and never
        // recycled.
        for id in self.seen..self.tree.minted() {
            self.moving.arrived(id, self.now);
        }
        self.seen = self.tree.minted();
        // A mark or an open row can outlive the directory it names, because the deleter takes
        // rows away while the reader is looking at them. Dropped here, once per frame, rather
        // than per removal: a batch of ten thousand deletions would otherwise re-scan the
        // whole mark set ten thousand times. A draining row goes with them — it is a row for a
        // directory that is no longer on the disk, and a mark on one would put it in the next
        // batch.
        let (tree, moving) = (&self.tree, &self.moving);
        let held = (self.marks.len(), self.spared.len());
        self.marks
            .retain(|mark| tree.is_attached(mark.root) && !moving.is_leaving(mark.root));
        self.spared.retain(|&id| tree.is_attached(id));
        if (self.marks.len(), self.spared.len()) != held {
            self.mark_stamp += 1;
        }
        self.expanded.retain(|&id| self.tree.is_attached(id));
        self.kept.retain(|&id, _| self.tree.is_attached(id));
        // A claim the reader deleted while a pricing thread was inside it never gets its
        // price, because [`View::priced`] resolves the path and the path is gone — so nothing
        // would ever cool it. It would shimmer for a thread that had finished, forever, and
        // hold the whole view at the animating frame rate to do it. Bounded by the pool, so
        // this is a handful of ids per frame.
        for id in self
            .moving
            .hot()
            .filter(|&id| !self.tree.is_attached(id))
            .collect::<Vec<_>>()
        {
            self.moving.cools(id);
        }
        self.recount();
        self.reflatten();
        self.settle(&anchor);
        self.follow_cursor();
        self.stale = false;
    }

    /// Moves the frame on to `now`: the one place time enters this file.
    ///
    /// Three things, in an order that matters. A drain that has run its course takes its row
    /// out of the tree *first*, so the rows this frame draws are the rows that exist. Then the
    /// view is brought back in line. Then every drawn row's number is advanced toward what it
    /// is really worth — **drawn** rows, which is what keeps this O(the pane) rather than
    /// O(the tree), and the whole reason interpolation is affordable on a view holding 22,765
    /// directories.
    pub fn animate(&mut self, now: Instant) {
        self.now = now;
        self.moving.tick(now);
        for id in self.moving.collapsed(now) {
            let path = self.tree.node(id).path.clone();
            self.tree.remove(&path);
            self.stale = true;
        }
        self.sync();
        self.recount_drains();
        let targets: Vec<(NodeId, u64, bool)> = std::iter::once(self.tree.root())
            .chain(
                self.rows
                    .iter()
                    .skip(self.scroll)
                    .take(self.page)
                    .map(|row| row.id),
            )
            // Never eased: these numbers come from the deleter and they are already the
            // truth, so smoothing them would be putting a guess in front of a measurement.
            // The chase is for values that jump — an arrival, a price — and a row emptying
            // does not jump, it is reported as it happens.
            .map(|id| (id, self.live(id).bytes, self.drained.contains_key(&id)))
            .collect();
        self.moving.advance(now, &targets, self.freed_total());
    }

    /// What the session has given back, counting the batch that is running.
    ///
    /// One source at a time and no overlap between them: while a removal is in flight the
    /// figure climbs on the per-target totals the deleter is reporting, and the moment the
    /// batch reports its own the running figures are dropped for it. See [`View::deleted`].
    fn freed_total(&self) -> u64 {
        self.freed + self.moving.freed_so_far()
    }

    /// Rebuilds [`View::drained`] from what the deleter has reported freeing.
    ///
    /// One walk up the ancestors per target in flight, which is the same shape as
    /// [`View::recount_marks`] and for the same reason: a subtraction applied when a target
    /// started would have to be un-applied correctly when it finished, and being wrong leaves
    /// a total that never comes back.
    ///
    /// The two halves move at different times on purpose. **Bytes** come off as they are
    /// freed, because they really have gone. **Claims** come off only when the sweep says it
    /// has finished with the target, because until then the directory is still there — and
    /// "how many directories" is what the selection counter and the batch are stated in, so
    /// dropping it early would say a row had been deleted while it was still being deleted.
    fn recount_drains(&mut self) {
        self.drained.clear();
        for (id, freed) in self.moving.leaving().collect::<Vec<_>>() {
            let roll = self.roll(id);
            let gone = if self.moving.is_spent(id) {
                // Finished with. Whatever the sweep managed to count, the directory is gone,
                // so the row is worth nothing and its claim stops counting.
                roll
            } else {
                Roll {
                    bytes: freed.min(roll.bytes),
                    claims: 0,
                    unpriced: 0,
                }
            };
            let mut at = Some(id);
            while let Some(current) = at {
                let drained = self.drained.entry(current).or_default();
                drained.bytes += gone.bytes;
                drained.claims += gone.claims;
                drained.unpriced += gone.unpriced;
                at = self.tree.node(current).parent;
            }
        }
    }

    /// What a row is worth once what has already left the disk is taken off.
    ///
    /// The truth the tree itself cannot state, because the tree learns about a removal only
    /// when the deleter has finished with the whole target: this is the same number a second
    /// or two earlier, falling as the bytes do. It is what the screen draws *and* what a
    /// decision is made on — the batch, the selection counter — and there is deliberately no
    /// second, laggier version of it for display.
    fn live(&self, id: NodeId) -> Roll {
        let roll = self.roll(id);
        let Some(gone) = self.drained.get(&id) else {
            return roll;
        };
        Roll {
            bytes: roll.bytes.saturating_sub(gone.bytes),
            claims: roll.claims.saturating_sub(gone.claims),
            unpriced: roll.unpriced.saturating_sub(gone.unpriced),
        }
    }

    /// Whether anything on screen is still in motion, which is what the event loop reads to
    /// decide how often to repaint.
    #[must_use]
    pub fn is_moving(&self) -> bool {
        self.moving.is_moving()
    }

    /// Whether this terminal can draw a map, and if not, why. Told **every frame**.
    ///
    /// Not once at start-up, which is what #656 was: half the answer is the pixel size in the
    /// window, and a window can gain or lose that without the terminal changing — a tmux
    /// client attaching, a pane moving to a display the terminal measures differently. So the
    /// layout reads a fact that is re-taken as often as it is used.
    ///
    /// Which makes the early return load-bearing rather than tidy: this runs ten times a
    /// second, and marking the view stale each time would re-fold every stamp in the tree to
    /// learn that nothing had changed.
    pub fn allow_maps(&mut self, possible: Maps) {
        if self.map.possible == possible {
            return;
        }
        // A map that was on the screen and cannot be now is worth one line. The reader did not
        // ask for the columns back and nothing else on the frame explains where the picture
        // went — which is the same courtesy `m` gives, in the one other place the answer can
        // change out from under somebody.
        if let (true, Some(why)) = (self.maps(), possible.why()) {
            self.says(why);
        }
        self.map.possible = possible;
        // Stale because the answer decides whether [`View::map_stamp`] has a table behind it,
        // and this is told to the view *after* it opened: without it the first frames of a run
        // would answer the map's "has anything changed" from the tree's lens-blind stamp, and
        // then swap to the folded one mid-scan for a picture that had not moved.
        self.stale = true;
    }

    /// Whether the map pane is on the screen.
    #[must_use]
    pub fn maps(&self) -> bool {
        self.map.possible.can() && self.map.on
    }

    /// How many rows the tree pane can draw. Set by the renderer, used by the page keys.
    pub fn viewport(&mut self, page: usize) {
        self.page = page.max(1);
        self.follow_cursor();
    }

    /// The visible rows, outermost first.
    #[must_use]
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// Which row the cursor is on, if any.
    #[must_use]
    pub fn cursor(&self) -> Option<usize> {
        self.cursor
    }

    /// The first row drawn.
    #[must_use]
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// The tree behind the rows, for the renderer to read names and hits off.
    #[must_use]
    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// Whether this row is open.
    #[must_use]
    pub fn is_expanded(&self, id: NodeId) -> bool {
        self.expanded.contains(&id)
    }

    /// What this row's subtree is worth — under the current view, which is the only number a
    /// row is allowed to state.
    #[must_use]
    pub fn roll(&self, id: NodeId) -> Roll {
        if self.is_sifted() {
            return self
                .counts
                .get(id)
                .map_or_else(Roll::default, |counts| counts.visible);
        }
        let node = self.tree.node(id);
        Roll {
            bytes: node.reclaimable,
            claims: node.claims,
            unpriced: node.unmeasured,
        }
    }

    /// What this row is drawing at this instant, which is the truth once it has caught up.
    ///
    /// A rolled-up total climbing toward its real value is the one thing a count of
    /// directories cannot say: how fast the scan is finding them. It is also the same
    /// mechanism, running the other way, that empties a row the deleter has just finished
    /// with. Only the *bytes* move — the claim counts do not, because a count is a thing a
    /// reader reads off rather than watches.
    #[must_use]
    pub fn drawn(&self, id: NodeId) -> Roll {
        Roll {
            bytes: self.moving.shown(id, self.live(id).bytes),
            ..self.live(id)
        }
    }

    /// The header's number, climbing.
    #[must_use]
    pub fn drawn_total(&self) -> Roll {
        self.drawn(self.tree.root())
    }

    /// What this session has given back, climbing — the other of the two counters a removal
    /// moves, and the one that goes up.
    #[must_use]
    pub fn drawn_freed(&self) -> u64 {
        self.moving.freed()
    }

    /// Whether anything has been freed this session at all.
    ///
    /// True as soon as the first bytes leave rather than when the batch reports, so the
    /// counter that climbs is on screen for the whole of the fall it is the counterpart to.
    #[must_use]
    pub fn has_freed(&self) -> bool {
        self.freed_total() > 0
    }

    /// How lit a newly found row is, from 1.0 down to 0.0 over about a second.
    #[must_use]
    pub fn freshness(&self, id: NodeId) -> f64 {
        self.moving.freshness(id)
    }

    /// Whether a pricing thread is inside this claim at this instant.
    #[must_use]
    pub fn is_pricing(&self, id: NodeId) -> bool {
        self.moving.is_hot(id)
    }

    /// Whether bytes are leaving this row right now.
    #[must_use]
    pub fn is_freeing(&self, id: NodeId) -> bool {
        self.moving.is_freeing(id)
    }

    /// Whether this row has emptied and is spending its last moment on screen.
    #[must_use]
    pub fn is_spent(&self, id: NodeId) -> bool {
        self.moving.is_spent(id)
    }

    /// Whether the deleter has touched this row at all — either phase.
    ///
    /// The one predicate the batch, the marks and `space` all read, so "a directory the
    /// deleter is part way through is not a directory to delete again" is stated once rather
    /// than in three places that could drift.
    fn is_leaving(&self, id: NodeId) -> bool {
        self.moving.is_freeing(id) || self.moving.is_spent(id)
    }

    /// Whether the mark cascade is passing through this row right now.
    #[must_use]
    pub fn is_cascading(&self, id: NodeId) -> bool {
        self.moving.is_cascading(id)
    }

    /// Which cell of a `width`-wide pricing shimmer is lit this frame.
    #[must_use]
    pub fn shimmer(&self, width: usize) -> usize {
        self.moving.shimmer(width, self.opened)
    }

    /// Why a removal left this directory standing, if it did.
    #[must_use]
    pub fn kept_reason(&self, id: NodeId) -> Option<&str> {
        self.kept.get(&id).map(String::as_str)
    }

    /// How much of this row's subtree is marked, **as the current view shows it**.
    ///
    /// Filter-relative, and that is a correctness property rather than a nicety. A directory
    /// can be entirely marked under `dependencies` and only partly marked under `all`, so a
    /// glyph computed against the whole tree would contradict the rows the reader can see
    /// directly underneath it. What the box says is a statement about this screen.
    ///
    /// A row with nothing visible under it is [`Mark::None`] rather than [`Mark::All`]: an
    /// empty set is not something to draw as fully marked, and the row is not on screen
    /// anyway.
    #[must_use]
    pub fn mark_of(&self, id: NodeId) -> Mark {
        let Some(counts) = self.counts.get(id) else {
            return Mark::None;
        };
        let whole = self.roll(id).claims;
        if counts.chosen.claims == 0 || whole == 0 {
            Mark::None
        } else if counts.chosen.claims >= whole {
            Mark::All
        } else {
            Mark::Partial
        }
    }

    /// What the map of `id` is drawn from, as one number.
    ///
    /// Everything [`super::treemap`] reads under `id` and nothing else, so it changes when the
    /// picture would and does not when it would not. The distinction that matters is against
    /// [`Tree::stamp`](crate::tree::Tree::stamp), which the tree keeps for free but which is
    /// **lens-blind**: a run opens on `default`, which hides the gitignored tier, so a tier-two
    /// claim arriving under the mapped directory moves the tree's stamp while changing no
    /// rectangle at all. Answering that with a redraw is a megabyte down the pty to show the
    /// picture that was already there.
    ///
    /// So it is folded on the one pass that is already lens-aware — [`tally`], which computes
    /// what every row is worth *under the current view* — rather than maintained beside it. An
    /// incremental version would have to be right about every arrival, every price and every
    /// deletion, which is the same argument this file already makes about the counts
    /// themselves.
    ///
    /// **Falls back to the tree's stamp when there is no map**, which over-reports rather than
    /// under-reports: [`tally`] does not fold what nobody is going to ask for, and a view that
    /// was never told a map is possible is a view with no pane to spend the redraw on.
    #[must_use]
    pub fn map_stamp(&self, id: NodeId) -> u64 {
        self.map_stamps
            .get(id)
            .copied()
            .unwrap_or_else(|| self.tree.stamp(id))
    }

    /// How many times the selection has changed since the view opened — the marks or the
    /// exclusions, either way round.
    ///
    /// For a reader of the view that has to answer "is this the same picture as last frame"
    /// without rebuilding the picture — [`super::treemap`], whose rectangles change colour on
    /// a mark. Nothing else says so: a mark moves no bytes and no claims, so the tree's own
    /// [`Tree::stamp`](crate::tree::Tree::stamp) is silent about it.
    ///
    /// A count and not a hash of what is marked, because the two states such a hash would
    /// most easily call equal — unmarking one directory and marking its equally sized
    /// neighbour — are the ones a reader is most likely to produce. It counts keystrokes
    /// rather than differences, so it can say a selection changed when it did not: that costs
    /// one redraw on a key the reader pressed, where the other way round is a picture that
    /// disagrees with the tree beside it.
    #[must_use]
    pub fn mark_stamp(&self) -> u64 {
        self.mark_stamp
    }

    /// What share of this row's subtree is marked, between 0.0 and 1.0.
    ///
    /// By **bytes**, which is what a reader deciding whether a partial ancestor is worth
    /// opening actually wants: forty marked directories out of fifty means nothing if the
    /// other ten hold all the space. Claims are the fallback for a subtree nobody has priced
    /// yet, where bytes cannot answer and the count is the only thing that is true.
    #[must_use]
    pub fn share(&self, id: NodeId) -> f64 {
        if self.mark_of(id) == Mark::All {
            return 1.0;
        }
        let whole = self.roll(id);
        let marked = self
            .counts
            .get(id)
            .map_or_else(Roll::default, |counts| counts.chosen);
        #[expect(
            clippy::cast_precision_loss,
            reason = "a ratio bound for one of seven block glyphs has no precision to lose"
        )]
        // Bytes only once everything under here has a number on it. Part way through a
        // breakdown a byte share would be a share of what happens to be priced — which reads
        // as "nearly all of this is marked" for a subtree whose one marked claim is the only
        // one anybody has measured. Claims are always known, so they are what the glyph
        // reports until bytes can be trusted, and it converges as the pool catches up.
        let share = match (whole.unpriced, whole.bytes, whole.claims) {
            (0, bytes @ 1.., _) => marked.bytes as f64 / bytes as f64,
            (_, _, claims @ 1..) => marked.claims as f64 / claims as f64,
            _ => 0.0,
        };
        share.clamp(0.0, 1.0)
    }

    /// What is marked, all together — the selection counter.
    ///
    /// **The whole selection, not the visible part of it.** Deleting acts on everything that
    /// is marked, so a counter stating only what is on screen would be the one number a reader
    /// checks disagreeing with the one thing the tool then does. What the view being narrowed
    /// changes is [`View::hidden`] beside it, which says how much of this the reader cannot
    /// currently see.
    ///
    /// Net of the rows the deleter has already finished with, which are in the tree for
    /// another third of a second while they empty. They are out of [`View::batch`], so they
    /// have to be out of the number that describes it.
    #[must_use]
    pub fn marked(&self) -> Roll {
        self.counts
            .get(self.tree.root())
            .map_or_else(Roll::default, |counts| counts.all)
    }

    /// How many marked directories the current view is hiding.
    ///
    /// Zero on a view that hides nothing, which is where a run starts. Above zero it is the
    /// hazard orthogonal selection creates, stated on the footer before the reader ever
    /// reaches the confirmation that spells it out.
    #[must_use]
    pub fn hidden(&self) -> usize {
        self.counts
            .get(self.tree.root())
            .map_or(0, |counts| counts.all.claims - counts.chosen.claims)
    }

    /// The whole scan, as the header states it — under the current view.
    #[must_use]
    pub fn total(&self) -> Roll {
        self.roll(self.tree.root())
    }

    /// How many claims the scan found that the current view is not showing.
    ///
    /// **The header says this, and that is what keeps a narrowed view honest.** The run opens
    /// on `default`, which hides the gitignored tier — a real tier worth real bytes that no
    /// other tool finds at all — so the headline count is not the whole answer to "how much do
    /// I get back". A number that is narrowed without saying so is the "silently keeps" failure
    /// the age floor was resolved against; saying so, on the line the number is on, is the
    /// difference between a filter and a lie.
    #[must_use]
    pub fn out_of_view(&self) -> usize {
        self.tree
            .node(self.tree.root())
            .claims
            .saturating_sub(self.total().claims)
    }

    /// The applied filter's pattern, if there is one.
    #[must_use]
    pub fn filter(&self) -> Option<&str> {
        self.lens.pattern()
    }

    /// Which named view the view is on, or `None` once the axis keys have taken it off all
    /// four.
    ///
    /// Derived from the axes rather than remembered, which the four presets occupying four
    /// distinct points is what buys: a reader who toggles their way onto `dependencies` is on
    /// `dependencies`, and nothing has to keep a record of how they got there.
    #[must_use]
    pub fn preset(&self) -> Option<Preset> {
        self.lens.preset()
    }

    /// What the footer calls the view: a preset's name, or the two axes spelled out.
    ///
    /// A lens the axis keys built has no name, and inventing one — or rounding it to the
    /// nearest preset — would tell the reader they are somewhere they are not.
    #[must_use]
    pub fn view_label(&self) -> String {
        self.preset().map_or_else(
            || self.lens.axes_label(),
            |preset| preset.label().to_owned(),
        )
    }

    /// The whole of what decides visibility, for anything that has to say what is hiding
    /// something.
    #[must_use]
    pub fn lens(&self) -> &Lens {
        &self.lens
    }

    /// The prompt, while it is up.
    #[must_use]
    pub fn prompt(&self) -> Option<&Prompt> {
        self.prompt.as_ref()
    }

    /// How far the help overlay is scrolled, while it is up.
    #[must_use]
    pub fn help(&self) -> Option<usize> {
        self.help
    }

    /// Holds the help overlay's scroll inside the page that was actually drawn.
    ///
    /// The renderer's job because it is the only thing that knows how long the page is and
    /// how much of it fits, which is why `G` here means "as far as it goes" rather than a
    /// number: a view that guessed would scroll a help page off the top of its own box.
    pub fn clamp_help(&mut self, furthest: usize) {
        if let Some(at) = self.help {
            self.help = Some(at.min(furthest));
        }
    }

    /// The question waiting for an answer.
    #[must_use]
    pub fn pending(&self) -> Option<&Pending> {
        self.pending.as_ref()
    }

    /// How many lines of the batch the confirmation has room for.
    ///
    /// The renderer's to say, exactly as [`View::viewport`] is, and for the same reason: the
    /// box's height depends on the frame, and a page size the view guessed would scroll the
    /// listing past its own border.
    pub fn listing(&mut self, page: usize) {
        if let Some(pending) = &mut self.pending {
            pending.page = page.max(1);
            pending.follow();
        }
    }

    /// Whether the walk is still running.
    #[must_use]
    pub fn is_scanning(&self) -> bool {
        self.scanning
    }

    /// Whether a removal is in flight.
    #[must_use]
    pub fn is_deleting(&self) -> bool {
        self.removing.is_some()
    }

    /// Where the removal in flight has got to, if there is one.
    #[must_use]
    pub fn removing(&self) -> Option<&Removing> {
        self.removing.as_ref()
    }

    /// Puts the view mid-removal without one having happened.
    ///
    /// The event loop's own tests need a view that is waiting on a deleter, and the honest
    /// door into that state runs an actual removal against an actual filesystem.
    #[cfg(test)]
    pub(crate) fn deleting_for_test(&mut self) {
        self.removing = Some(Removing::new(&[(PathBuf::from("/scan/target"), 0)]));
    }

    /// What just happened, for the footer.
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_ref().map(|notice| notice.said.as_str())
    }

    /// Whether what the footer is saying waits to be dismissed rather than going with the
    /// reader's next action.
    ///
    /// Deliberately not something the frame draws differently: a sentence saying a subtree was
    /// left alone is the safety model working, and an alarm-coloured footer would teach a
    /// reader that correct behaviour is a failure. See [`Notice`].
    #[must_use]
    pub fn notice_stands(&self) -> bool {
        self.notice.as_ref().is_some_and(|notice| notice.stands)
    }

    /// Says something in the footer until the reader's next action.
    fn says(&mut self, said: impl Into<String>) {
        self.notice = Some(Notice::passing(said));
    }

    /// Says something that waits to be dismissed, because it names a refusal or a failure.
    fn warns(&mut self, said: impl Into<String>) {
        self.notice = Some(Notice::standing(said));
    }

    /// Drops what the footer is saying, if the reader's next action has made it stale.
    ///
    /// A standing notice survives this — that is the whole of what "standing" means, and it is
    /// why the two are one field with a flag rather than two independent messages: there is
    /// only ever one footer, so the last thing said is the thing shown either way.
    fn expire(&mut self) {
        if !self.notice_stands() {
            self.notice = None;
        }
    }

    /// Which sort the levels are in.
    #[must_use]
    pub fn sort(&self) -> Sort {
        self.sort
    }

    /// Which surface has the keyboard.
    ///
    /// Ranked rather than exclusive, because the globals can open a prompt or the help page
    /// over a question: the confirmation is bottom of the stack, not top.
    #[must_use]
    pub fn overlay(&self) -> Option<Overlay> {
        if self.prompt.is_some() {
            Some(Overlay::Prompt)
        } else if self.help.is_some() {
            Some(Overlay::Help)
        } else if self.pending.is_some() {
            Some(Overlay::Confirm)
        } else {
            None
        }
    }

    // ---- what a keystroke does --------------------------------------------------------

    /// Carries out one action, and says what the event loop has to do about it.
    pub fn apply(&mut self, action: Action) -> Effect {
        self.sync();
        // What the footer says describes the frame *before* this keystroke, so the keystroke
        // takes it away — before the action runs, so that an action with something of its own
        // to say still gets the last word. `Ignore` is left out because a key nobody bound is
        // not the reader acting, and `Back` because dismissing is a rung of its own: one `Esc`
        // must step back exactly once. See [`Notice`].
        if !matches!(action, Action::Ignore | Action::Back) {
            self.expire();
        }
        match action {
            Action::Quit => return self.quit(),
            Action::Ignore => {}
            Action::Help => {
                self.help = if self.help.is_some() { None } else { Some(0) };
            }
            Action::Back => self.step_back(),
            // One rung, never the one below it: see [`Action::Dismiss`].
            Action::Dismiss => self.notice = None,
            Action::OpenFilter => {
                self.prompt = Some(Prompt::seeded(self.filter().unwrap_or_default()));
            }
            Action::Type(character) => self.edit(|prompt| {
                prompt.chars.insert(prompt.caret, character);
                prompt.caret += 1;
            }),
            Action::Erase => self.edit(|prompt| {
                if prompt.caret > 0 {
                    prompt.caret -= 1;
                    prompt.chars.remove(prompt.caret);
                }
            }),
            Action::EraseAhead => self.edit(|prompt| {
                if prompt.caret < prompt.chars.len() {
                    prompt.chars.remove(prompt.caret);
                }
            }),
            Action::Wipe => self.edit(|prompt| {
                prompt.chars.clear();
                prompt.caret = 0;
            }),
            Action::Caret(motion) => self.edit(|prompt| {
                prompt.caret = match motion {
                    Motion::Up | Motion::PageUp => prompt.caret.saturating_sub(1),
                    Motion::Down | Motion::PageDown => (prompt.caret + 1).min(prompt.chars.len()),
                    Motion::Top => 0,
                    Motion::Bottom => prompt.chars.len(),
                };
            }),
            Action::Submit => self.submit(),
            Action::Scroll(motion) => self.scroll_help(motion),
            Action::Highlight(turn) => {
                if let Some(pending) = &mut self.pending {
                    pending.answer = match turn {
                        Turn::Prev => Answer::Cancel,
                        Turn::Next => Answer::Delete,
                    };
                }
            }
            Action::Answer => return self.answer(),
            Action::Listing(motion) => {
                if let Some(pending) = &mut self.pending {
                    pending.walk(motion);
                }
            }
            Action::Spare => self.spare_entry(),
            Action::CyclePreset(turn) => self.cycle_preset(turn),
            Action::CycleTiers => self.cycle_tiers(),
            Action::ToggleFiles => self.toggle_files(),
            Action::ToggleKind(kind) => self.toggle_kind(kind),
            Action::Cursor(motion) => self.move_cursor(motion),
            Action::ScrollRows(motion) => self.scroll_rows(motion),
            Action::Expand => self.expand(),
            Action::Collapse => self.collapse(),
            Action::ToggleSubtree => self.toggle_subtree(),
            Action::CollapseAll => {
                self.expanded.retain(|&id| id == self.tree.root());
                self.stale = true;
            }
            Action::Mark => self.toggle_mark(),
            Action::MarkAll => self.mark_all(),
            Action::Commit => return self.commit(),
            Action::ToggleMap => self.toggle_map(),
            Action::CycleSort => self.resort(Sort {
                by: self.sort.by.next(),
                reverse: self.sort.reverse,
            }),
            Action::ReverseSort => self.resort(Sort {
                reverse: !self.sort.reverse,
                ..self.sort
            }),
            Action::SortBy(order) => self.sort_by(order),
            Action::Select(id) => {
                self.point_at(id);
            }
            Action::OpenRow(id) => self.open_row(id),
            Action::MarkRow(id) => self.mark_row(id),
            Action::Price(id) => return self.price_row(id),
        }
        self.sync();
        Effect::None
    }

    /// `m`: the map pane, or the reason there is not one.
    ///
    /// A terminal that cannot draw one is told so rather than left pressing a documented key
    /// that does nothing — the same rule as a mark box that is drawn and cannot be pressed.
    /// **Which** reason matters: "no pixel size" is usually a multiplexer in the way and is
    /// something the reader can do something about, where "not on the allowlist" is not.
    fn toggle_map(&mut self) {
        if let Some(why) = self.map.possible.why() {
            self.says(why);
            return;
        }
        self.map.on = !self.map.on;
        // As in [`View::allow_maps`]: the pane coming back has to find its stamps built.
        self.stale = true;
    }

    /// `q`: leave — unless something irreversible is in flight, in which case wait for it.
    ///
    /// The wait is bounded by the work the reader themselves asked for, and it is *visible*:
    /// rows keep disappearing as the deleter finishes each target. Tearing the batch in half
    /// would not be.
    fn quit(&mut self) -> Effect {
        if self.is_deleting() {
            self.quitting = true;
            self.says("the removal has to finish — closing the moment it does");
            return Effect::None;
        }
        Effect::Quit
    }

    /// Whether a quit that was held back by a removal can be honoured now.
    #[must_use]
    pub fn wants_to_quit(&self) -> bool {
        self.quitting && !self.is_deleting()
    }

    /// One rung down the ladder: whatever is in front of the reader, taken away.
    ///
    /// Never quits, which is the rule that makes `Esc` safe to press without looking. The
    /// bottom rung is doing nothing at all; `q` is the way out and it is on every help page.
    ///
    /// The rungs are in front-to-back order, and the notice sits where it does for two reasons.
    /// It is *behind* the overlays — the confirmation included — because they are literally
    /// drawn over it, and the prompt borrows the footer, so while one is up there is no notice
    /// on the screen to dismiss. It is *in front of* the narrowings and the marks because it is
    /// the cheapest rung to take by mistake: an `Esc` that dropped forty marks when the reader
    /// meant to get rid of a sentence is the one outcome this ladder exists to prevent.
    fn step_back(&mut self) {
        if self.prompt.take().is_some() {
            return;
        }
        if self.help.take().is_some() {
            return;
        }
        if self.pending.take().is_some() {
            return;
        }
        if self.notice.take().is_some() {
            return;
        }
        // The narrowings come off in the order they were put on: the pattern first, then the
        // preset. Two rungs rather than one, because they are two independent things and a
        // reader who typed a pattern over `dependencies` means to lose the pattern.
        if self.lens.pattern().is_some() {
            self.lens = self.lens.clone().matching(None);
            self.stale = true;
            return;
        }
        if self.lens != Lens::default() {
            self.lens = Lens::default();
            self.stale = true;
            return;
        }
        if !self.marks.is_empty() {
            self.clear_marks();
        }
    }

    fn edit(&mut self, change: impl FnOnce(&mut Prompt)) {
        if let Some(prompt) = &mut self.prompt {
            change(prompt);
            prompt.error = None;
        }
    }

    /// Applies what has been typed, or says why it cannot be.
    ///
    /// A pattern the engine refuses leaves the prompt up with the reason under it. Closing it
    /// and quietly showing an unfiltered tree would look exactly like a filter that matched
    /// everything.
    fn submit(&mut self) {
        let Some(prompt) = &mut self.prompt else {
            return;
        };
        let pattern = prompt.text();
        if pattern.is_empty() {
            self.prompt = None;
            self.lens = self.lens.clone().matching(None);
            self.stale = true;
            return;
        }
        match Regex::new(&pattern) {
            Ok(regex) => {
                self.lens = self.lens.clone().matching(Some(regex));
                self.prompt = None;
                self.stale = true;
            }
            Err(err) => {
                let reason = err.to_string();
                prompt.error = Some(reason.lines().last().unwrap_or("not a regex").to_owned());
            }
        }
    }

    /// `f` and `F`: the next named view, or the one before.
    ///
    /// A preset changes what is on screen and — deliberately — nothing about what is
    /// selected. That is the rule the whole model turns on, and it is why the notice says what
    /// the new view *hides* rather than what it shows: a claim that has gone from the screen
    /// is still in the batch, and the reader has to be able to tell that from a claim that was
    /// never found.
    fn cycle_preset(&mut self, turn: Turn) {
        // A reader who has moved an axis by hand is not on a preset at all, so the key puts
        // them back on the first one rather than stepping from a place they never were.
        let next = match self.preset() {
            Some(at) => match turn {
                Turn::Next => at.next(),
                Turn::Prev => at.prev(),
            },
            None => Preset::default(),
        };
        // The axes move and the `/` pattern does not. It narrows whatever the axes leave, so it
        // is orthogonal to both of them — and a preset that quietly dropped it would be using
        // an unrelated piece of state to mean something about the view.
        self.lens = Lens::showing(next).matching(self.held_pattern());
        self.narrowed(format!("showing {}", next.what()));
    }

    /// `t`: the tier axis, on its own.
    fn cycle_tiers(&mut self) {
        let tiers = self.lens.tiers().next();
        self.lens = self.lens.clone().with_tiers(tiers);
        self.off_the_presets();
    }

    /// `i`: gitignored files, on their own.
    fn toggle_files(&mut self) {
        let files = !self.lens.files();
        self.lens = self.lens.clone().with_files(files);
        self.off_the_presets();
    }

    /// `u` `d` `b` `c` `n`: one member of the kind axis, on its own.
    fn toggle_kind(&mut self, kind: Kind) {
        let kinds = self.lens.kinds().toggling(kind);
        self.lens = self.lens.clone().with_kinds(kinds);
        self.off_the_presets();
    }

    /// Applies an axis edit: the view is now whatever the two axes say, and not a preset.
    ///
    /// It says what the *axes* are rather than what the step was called, because there is no
    /// name to give — that is the whole point of the two keys, and rounding to the nearest
    /// preset would report a view the reader is not on.
    fn off_the_presets(&mut self) {
        // Nothing to record: [`View::preset`] reads the axes, so a hand-edited lens that lands
        // on a preset's point *is* that preset and one that does not has no name. What it says
        // is the axes, because that is the only description a nameless view has.
        let said = self.lens.axes_label();
        self.narrowed(format!("showing {said}"));
    }

    /// The `/` pattern as a fresh engine, for a lens being rebuilt around it.
    fn held_pattern(&self) -> Option<Regex> {
        self.lens.pattern().and_then(|held| Regex::new(held).ok())
    }

    /// Re-derives after a change of view and says what it left out.
    ///
    /// **The sentence names what is now missing**, which is the whole of what keeps a narrowed
    /// view from being the "silently keeps" failure: a claim that has gone from the screen is
    /// still in the batch, and a reader has no way to tell that from a claim that was never
    /// found unless something says so.
    ///
    /// [`Notice::passing`] rather than standing, and the count is what makes that safe rather
    /// than a hole. This sentence names nothing refused and nothing failed — it answers the
    /// keystroke that narrowed the view, so the reader's next one has seen it. What must not
    /// perish is the *fact* it carries, and that does not live here: the footer states how much
    /// of the selection is out of sight on every frame, notice or no notice, and the
    /// confirmation states it again on the one screen where it can still change a decision.
    /// A standing notice would instead park one keystroke's echo over the keys until it was
    /// dismissed, which is the report outliving what it reports on.
    fn narrowed(&mut self, said: String) {
        self.stale = true;
        self.sync();
        let hidden = self.hidden();
        self.says(if hidden == 0 {
            said
        } else {
            format!(
                "{said} · {} still marked and out of sight",
                plural(hidden, "directory", "directories")
            )
        });
    }

    fn scroll_help(&mut self, motion: Motion) {
        let Some(at) = self.help else {
            return;
        };
        let page = self.page;
        self.help = Some(match motion {
            Motion::Up => at.saturating_sub(1),
            Motion::Down => at + 1,
            Motion::PageUp => at.saturating_sub(page),
            Motion::PageDown => at + page,
            Motion::Top => 0,
            // Clamped by the renderer against the page it actually drew, which is the only
            // place the length is known.
            Motion::Bottom => usize::MAX,
        });
    }

    /// Takes the highlighted answer.
    ///
    /// The deed is the targets the dialog was holding, never a fresh reading of the marks:
    /// see [`Pending`].
    fn answer(&mut self) -> Effect {
        let Some(pending) = self.pending.take() else {
            return Effect::None;
        };
        match pending.answer {
            Answer::Cancel => Effect::None,
            Answer::Delete => {
                // The batch's size is fixed here and nowhere else, which is what makes the
                // progress a fraction rather than a running total. No notice beside it: the
                // footer draws the count while this is set, and a static "removing 12
                // directories…" sitting next to a live "removing 4 of 12" would be two
                // statements about one batch that stop agreeing on the second target.
                //
                // Cleared outright rather than expired, standing ones included: answering this
                // dialog is as deliberate as a reader gets, and what the last batch refused is
                // not a thing to leave sitting beside a live count of this one.
                // Each target's own weight, taken from the listing the reader just agreed to
                // rather than re-derived from the tree — the same reason the box adds its
                // headline up over the entries. A footer whose denominator disagreed with the
                // figure in the dialog would be two statements about one batch.
                //
                // Weighed before any of it goes, because the deleter can only ever report what
                // it has *given back*. An unpriced target is a zero, and a batch of nothing but
                // those gives no byte figure at all rather than a total that is quietly a
                // fraction of the truth.
                let weighed: Vec<(PathBuf, u64)> = pending
                    .entries
                    .iter()
                    .filter(|entry| entry.target.is_some())
                    .map(|entry| (entry.path.clone(), entry.size.bytes().unwrap_or(0)))
                    .collect();
                self.removing = Some(Removing::new(&weighed));
                self.notice = None;
                Effect::Delete(pending.targets)
            }
        }
    }

    /// `space` on a line of the confirmation: take that directory out of the batch.
    ///
    /// **The answer to a surprise has to be better than "cancel and start again".** A reader
    /// who reaches this screen and finds something they did not mean to have marked is
    /// looking at the one moment where they can still act on it, and a screen that could only
    /// be read would send them back to a tree where the offending row may not even be visible.
    ///
    /// It changes the marks as well as the listing, because the two have to keep saying the
    /// same thing: the deed shrinks with the line, and the tree behind the box agrees when
    /// the box goes.
    fn spare_entry(&mut self) {
        let Some(pending) = &self.pending else {
            return;
        };
        let at = pending.at;
        let Some(entry) = pending.current().cloned() else {
            return;
        };
        if let Some(pending) = &mut self.pending {
            pending.drop_at(at);
        }
        if let Some(id) = entry.id {
            self.unmark(id);
        }
        self.stale = true;
        self.sync();
        let left = self
            .pending
            .as_ref()
            .map_or(0, |pending| pending.entries.len());
        if left == 0 {
            // Nothing left to ask about. Closing is the honest answer rather than a box
            // offering to delete an empty set.
            self.pending = None;
            // Passing, both of these: the reader emptied the batch a line at a time, so this
            // reports what they just did rather than something that was refused or failed.
            self.says("nothing left in the batch");
            return;
        }
        self.says(format!("{} unmarked", entry.path.display()));
    }

    /// `x`: hand the marked batch out to be planned.
    fn commit(&mut self) -> Effect {
        if self.is_deleting() {
            self.says("a removal is already running");
            return Effect::None;
        }
        let batch = self.batch();
        if batch.is_empty() {
            self.says("nothing is marked — space marks a row's whole subtree");
            return Effect::None;
        }
        Effect::Plan(batch)
    }

    /// Every claim the marks select, which is what a batch is.
    ///
    /// **The whole selection, and never only the visible part.** A mark resolves through the
    /// lens it was *made* through, so narrowing the view afterwards takes nothing out of the
    /// batch — anything else would contradict the one promise the model makes, that toggling
    /// what is visible never changes what is selected. What the narrowing does instead is put
    /// entries on the confirmation marked as hidden, which is where a reader can act on the
    /// surprise.
    ///
    /// A row that is draining away is out. It is on screen for another third of a second
    /// saying what happened to it, and offering a directory that is already gone to a second
    /// removal would report a failure for a target the first removal succeeded on. That is
    /// decided in [`tally`], with the counter, so the two cannot part company.
    #[must_use]
    pub fn batch(&self) -> Vec<Target> {
        let mut batch: Vec<Target> = self
            .selection
            .iter()
            .filter_map(|&id| self.tree.node(id).hit.as_ref().map(Target::from))
            .collect();
        batch.sort_by(|a, b| a.path.cmp(&b.path));
        batch
    }

    fn resort(&mut self, sort: Sort) {
        self.sort = sort;
        self.sorted = false;
        self.stale = true;
    }

    /// A digit, or a click on a column heading — order the levels by that key.
    ///
    /// One method for both doors, so an ordering cannot become the one in force two ways.
    /// Naming the order **already in force** turns it upside down, which is what a reader
    /// clicking a heading twice means and what `1 1` should therefore mean too.
    ///
    /// **A new column starts in its own natural order**, rather than inheriting whichever way
    /// `S` last left things. The natural order is the useful one in every case — biggest
    /// subtree first, names A–Z, stalest first — and carrying a reversal across a column
    /// change would be reversing something the reader never asked to reverse.
    fn sort_by(&mut self, order: Order) {
        self.resort(if self.sort.by == order {
            Sort {
                by: order,
                reverse: !self.sort.reverse,
            }
        } else {
            Sort::by(order)
        });
    }

    // ---- what a pointer does ----------------------------------------------------------

    /// A click on a row — put the cursor on that **directory**.
    ///
    /// Selecting by identity rather than by the index the press resolved to, which is the
    /// rule the whole pointer model turns on: rows re-sort as prices land and vanish as
    /// removals finish, so an index taken at the press and acted on at the release names
    /// somebody else. A directory that is no longer on screen leaves the cursor where it is
    /// and says so by returning `false` — the honest outcome, since the row the reader aimed
    /// at is gone.
    fn point_at(&mut self, id: NodeId) -> bool {
        let Some(at) = self.rows.iter().position(|row| row.id == id) else {
            return false;
        };
        self.cursor = Some(at);
        self.deselected = false;
        self.follow_cursor();
        true
    }

    /// A click on a row's `▸` — select it, and open or close it.
    ///
    /// The two steps `→` and `←` produce between them, reached with one gesture. A leaf is
    /// selected and nothing else happens: there is nothing to open, and the cell its
    /// indicator would be in is blank, so a press there cannot have been aimed at one.
    fn open_row(&mut self, id: NodeId) {
        if !self.point_at(id) || self.tree.children(id).is_empty() {
            return;
        }
        if !self.expanded.insert(id) {
            self.expanded.remove(&id);
        }
        self.stale = true;
    }

    /// A click on a row's `[ ]` — select it, and mark its subtree or unmark it.
    fn mark_row(&mut self, id: NodeId) {
        if self.point_at(id) {
            self.mark_at(id);
        }
    }

    /// A double click on a row — ask for a price on everything under it that has none.
    ///
    /// The gesture for the expensive action a reader wants on one specific thing. Under
    /// `--breakdown-under` every row outside the named scope reads as a dash, and this is how
    /// one of them is asked about without re-running the scan; on a fully priced tree it
    /// finds nothing and says so rather than starting work with no result.
    ///
    /// Two things it refuses, and both are about work rather than about display. A row that
    /// is **no longer on screen** starts nothing: a detached node keeps its hit, so walking
    /// what is under one would hand back a path the reader can no longer see and the deleter
    /// has already removed — the identity rule running the other way round, since here the
    /// vanished target costs a traversal rather than a selection. And a claim **already being
    /// priced** is not asked for again: `Tree::price` rejects a duplicate result, but only
    /// after the expensive part has happened, so leaning on the button during a traversal of
    /// a real `node_modules` would queue that traversal over and over.
    fn price_row(&mut self, id: NodeId) -> Effect {
        if !self.point_at(id) {
            return Effect::None;
        }
        let (waiting, running): (Vec<PathBuf>, Vec<PathBuf>) = self
            .unpriced_under(id)
            .into_iter()
            .partition(|path| !self.pricing.contains(path));
        if waiting.is_empty() {
            // Two different facts, and the reader can act on the difference: one says there
            // is nothing to learn here, the other says to wait.
            if running.is_empty() {
                self.says("everything under here already carries a price");
            } else {
                self.says(format!(
                    "{} under here is already being priced",
                    plural(running.len(), "directory", "directories")
                ));
            }
            return Effect::None;
        }
        self.pricing.extend(waiting.iter().cloned());
        self.says(format!(
            "pricing {}…",
            plural(waiting.len(), "directory", "directories")
        ));
        Effect::Price(waiting)
    }

    /// Every claim under `id` that nobody has put a number on.
    ///
    /// Filtered, for [`View::batch`]'s reason: what a row acts on is what its own number
    /// describes, and a gesture that priced claims the filter is hiding would move a total
    /// the reader cannot see.
    fn unpriced_under(&self, id: NodeId) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut stack = vec![id];
        while let Some(id) = stack.pop() {
            if !self.shown(id) {
                continue;
            }
            let node = self.tree.node(id);
            match &node.hit {
                Some(hit) if hit.size.bytes().is_none() => found.push(hit.path.clone()),
                Some(_) => {}
                None => stack.extend(node.children.iter().copied()),
            }
        }
        found.sort();
        found
    }

    /// The wheel — move the viewport, and take the cursor with it.
    ///
    /// pua leaves its cursor behind when the wheel moves its tree, because there a cursor only
    /// highlights. Here it is what `space`, `→`, `←` and `*` act on, so a cursor scrolled off
    /// the screen is a mark aimed at a row nobody can see — and [`View::follow_cursor`] would
    /// drag the viewport back to it on the next frame anyway. It is pushed to the nearest row
    /// still drawn instead, which is where a reader who scrolled to look at something would
    /// have put it. A cursor that was taken away stays away: scrolling is not choosing.
    fn scroll_rows(&mut self, motion: Motion) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        // Never past the point where the last row is at the bottom of the pane. The cursor
        // keys reach that same limit through `follow_cursor`, which stops the moment the
        // cursor is on screen; a wheel has no cursor pulling it up, so the limit is its own.
        let furthest = self.rows.len().saturating_sub(self.page);
        self.scroll = match motion {
            Motion::Up => self.scroll.saturating_sub(WHEEL),
            Motion::Down => (self.scroll + WHEEL).min(furthest),
            Motion::PageUp => self.scroll.saturating_sub(self.page),
            Motion::PageDown => (self.scroll + self.page).min(furthest),
            Motion::Top => 0,
            Motion::Bottom => furthest,
        };
        if let Some(at) = self.cursor {
            self.cursor = Some(at.clamp(self.scroll, (self.scroll + self.page - 1).min(last)));
        }
    }

    // ---- the cursor -------------------------------------------------------------------

    fn move_cursor(&mut self, motion: Motion) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        let at = match self.cursor {
            // A view whose cursor was taken away picks up again at whichever end the key was
            // reaching for, deliberately: the reader is choosing a row rather than inheriting
            // one.
            None => match motion {
                Motion::Up | Motion::PageUp | Motion::Bottom => last,
                Motion::Down | Motion::PageDown | Motion::Top => 0,
            },
            Some(at) => match motion {
                Motion::Up => at.saturating_sub(1),
                Motion::Down => (at + 1).min(last),
                Motion::PageUp => at.saturating_sub(self.page),
                Motion::PageDown => (at + self.page).min(last),
                Motion::Top => 0,
                Motion::Bottom => last,
            },
        };
        self.cursor = Some(at);
        self.deselected = false;
        self.follow_cursor();
    }

    /// The row under the cursor.
    #[must_use]
    pub fn row(&self) -> Option<Row> {
        self.cursor.and_then(|at| self.rows.get(at).copied())
    }

    /// The cursor's directory and every directory above it, nearest first.
    ///
    /// The chain rather than just the row, because between one frame and the next the
    /// directory under the cursor can simply be gone. Walking outwards then lands on the
    /// nearest surviving ancestor — which is almost always what the reader was working on —
    /// instead of on row 0.
    fn anchor(&self) -> Vec<PathBuf> {
        let mut chain = Vec::new();
        let Some(row) = self.row() else {
            return chain;
        };
        let mut at = Some(row.id);
        while let Some(id) = at {
            let node = self.tree.node(id);
            chain.push(node.path.clone());
            at = node.parent;
        }
        chain
    }

    /// Puts the cursor on the first path of `chain` that is on screen, or nowhere.
    fn settle(&mut self, chain: &[PathBuf]) {
        self.cursor = chain.iter().find_map(|path| {
            self.rows
                .iter()
                .position(|row| self.tree.node(row.id).path == *path)
        });
        if self.cursor.is_none() {
            if chain.is_empty() && !self.deselected && !self.rows.is_empty() {
                // Nothing was selected because nothing had arrived yet. The first rows to
                // land get the cursor, so the view is usable without a keystroke to wake it.
                self.cursor = Some(0);
            } else if !chain.is_empty() {
                // Everything the reader was looking at has been deleted. Deselecting is
                // visible; clamping the old index would silently hand the next keystroke to
                // whatever fell into that position.
                self.deselected = true;
            }
        }
    }

    /// Keeps the viewport over the cursor, and inside the rows either way.
    ///
    /// The clamp runs even with no cursor, which is not belt-and-braces: a filter can take every
    /// row away while the viewport is a long way down, and a scroll offset past the end of the
    /// rows draws an empty pane over a tree that has plenty in it.
    fn follow_cursor(&mut self) {
        if let Some(at) = self.cursor {
            if at < self.scroll {
                self.scroll = at;
            } else if at >= self.scroll + self.page {
                self.scroll = at + 1 - self.page;
            }
        }
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(1));
    }

    // ---- opening and closing ----------------------------------------------------------

    /// `→`: open a closed row, or step into an open one.
    fn expand(&mut self) {
        let Some(row) = self.row() else {
            return;
        };
        if self.tree.children(row.id).is_empty() {
            return;
        }
        if self.expanded.insert(row.id) {
            self.stale = true;
        } else if self.cursor.is_some_and(|at| at + 1 < self.rows.len()) {
            self.move_cursor(Motion::Down);
        }
    }

    /// `←`: close an open row, or step out of a closed one.
    fn collapse(&mut self) {
        let Some(row) = self.row() else {
            return;
        };
        if self.expanded.remove(&row.id) {
            self.stale = true;
        } else if let Some(parent) = self.tree.node(row.id).parent {
            let path = self.tree.node(parent).path.clone();
            if let Some(at) = self
                .rows
                .iter()
                .position(|row| self.tree.node(row.id).path == path)
            {
                self.cursor = Some(at);
                self.deselected = false;
                self.follow_cursor();
            }
        }
    }

    /// `*`: open or close everything under the cursor.
    fn toggle_subtree(&mut self) {
        let Some(row) = self.row() else {
            return;
        };
        let opening = !self.expanded.contains(&row.id);
        let mut stack = vec![row.id];
        while let Some(id) = stack.pop() {
            if self.tree.children(id).is_empty() {
                continue;
            }
            if opening {
                self.expanded.insert(id);
            } else {
                self.expanded.remove(&id);
            }
            stack.extend(self.tree.children(id).iter().copied());
        }
        self.stale = true;
    }

    /// Walks the open rows into a flat list.
    fn reflatten(&mut self) {
        self.rows.clear();
        let mut stack = vec![(self.tree.root(), 0usize)];
        while let Some((id, depth)) = stack.pop() {
            if !self.shown(id) {
                continue;
            }
            self.rows.push(Row { id, depth });
            if self.expanded.contains(&id) {
                // Reversed, because a stack hands back what went in last and the levels are
                // already in the order the sort put them.
                for &child in self.tree.children(id).iter().rev() {
                    stack.push((child, depth + 1));
                }
            }
        }
    }

    /// Whether a node survives the current view. Everything survives when it hides nothing.
    ///
    /// The one exception is the scan root of a tree with nothing in it yet, and it earns its
    /// place now that the view a run opens on **narrows**: the root is the directory the reader
    /// typed rather than a claim, so a view has nothing to say about it, and hiding it would
    /// blank the pane for the first moments of every run and for the whole of a scan that finds
    /// only the tier `default` leaves out. A filter that matches nothing still empties the pane
    /// — that is a narrowing the reader asked for, and #602's deselection depends on it.
    fn shown(&self, id: NodeId) -> bool {
        if !self.is_sifted() {
            return true;
        }
        if id == self.tree.root() && self.tree.node(id).claims == 0 {
            return true;
        }
        self.roll(id).claims > 0
    }

    // ---- marking ----------------------------------------------------------------------

    /// `space`: mark the cursor's subtree, or unmark it.
    ///
    /// A mark runs visibly up the ancestors on its way in — see [`Moving::cascade`]. It is the
    /// signature interaction and the one whose effect is otherwise entirely off screen: a mark
    /// on a collapsed row takes everything underneath, and the only place that shows is on
    /// ancestors the reader is not looking at. The cascade is that fact, drawn.
    fn toggle_mark(&mut self) {
        if let Some(row) = self.row() {
            self.mark_at(row.id);
        }
    }

    /// Marking one row, whichever door reached it — the key or the box under the pointer.
    ///
    /// Both the guard and the cascade live here rather than in [`View::toggle_mark`], because
    /// they are facts about marking a row and not about the key that reached it: a press on
    /// the mark box of a row the deleter is emptying has to be refused for exactly the reason
    /// `space` on it is.
    fn mark_at(&mut self, id: NodeId) {
        // A directory the deleter has already finished with is not something to mark for
        // deletion. Its row is still on screen because it is emptying, which is a statement
        // about the past.
        if self.is_leaving(id) {
            return;
        }
        // What the *reader* can see is what the key toggles, which is why this asks the
        // filter-relative glyph rather than a global "is it covered": a row drawn full is a
        // row `space` empties, and a row drawn part-full is one it fills.
        if self.mark_of(id) == Mark::All {
            self.unmark(id);
        } else {
            let chain = self.ancestry(id);
            self.moving.cascade(&chain, self.now);
            self.mark(id);
        }
    }

    /// A row and every directory above it, nearest first — what a cascade runs through.
    fn ancestry(&self, id: NodeId) -> Vec<NodeId> {
        let mut chain = Vec::new();
        let mut at = Some(id);
        while let Some(current) = at {
            chain.push(current);
            at = self.tree.node(current).parent;
        }
        chain
    }

    /// `a`: mark everything, or — if anything at all is marked — clear.
    fn mark_all(&mut self) {
        if self.marks.is_empty() {
            self.mark(self.tree.root());
        } else {
            self.clear_marks();
        }
    }

    /// Marks a subtree, as the current view defines it.
    ///
    /// The lens is copied into the mark rather than referred to, which is what makes a later
    /// change of view inert: `~/repos` marked under `dependencies` keeps meaning the
    /// dependencies under `~/repos` when the reader widens to `all`, and the build artefacts
    /// beside them stay unmarked.
    ///
    /// Two tidyings, and neither of them ever loses a selection. Any exclusion at or under
    /// the new mark goes, because the reader has just said to take the lot; and a mark
    /// already inside it **through the same lens** is absorbed, because it now says nothing
    /// the outer one does not. A mark inside it through a *different* lens survives, since it
    /// may well cover claims this one does not.
    fn mark(&mut self, id: NodeId) {
        self.mark_stamp += 1;
        let lens = self.lens.clone();
        let inside: Vec<NodeId> = self
            .spared
            .iter()
            .copied()
            .filter(|&spared| self.descends_from(spared, id))
            .collect();
        for spared in inside {
            self.spared.remove(&spared);
        }
        let tree = &self.tree;
        self.marks.retain(|mark| {
            !(mark.lens == lens && mark.root != id && descends_from(tree, mark.root, id))
        });
        if !self
            .marks
            .iter()
            .any(|mark| mark.root == id && mark.lens == lens)
        {
            self.marks.push(Marked { root: id, lens });
        }
        self.stale = true;
    }

    /// Unmarks a subtree, whichever mark was covering it.
    ///
    /// A mark rooted here goes outright; anything else is an ancestor's mark reaching down,
    /// and what spares this subtree from it is an **exclusion** rather than a mark on every
    /// sibling along the path. The push-down that used to do this cost a mark per sibling on
    /// a level 8,660 wide, and — worse — it was a statement about the claims that existed at
    /// that instant, so a claim arriving next to a spared row a minute later was silently
    /// unmarked too.
    fn unmark(&mut self, id: NodeId) {
        self.mark_stamp += 1;
        self.marks.retain(|mark| mark.root != id);
        // **Re-derived before the next question rather than after this one.** The counts on
        // hand describe the state before the line above, so asking them whether anything is
        // still covering this row would answer about the mark that has just gone — and the
        // answer decides whether an exclusion is left behind. A stray exclusion is invisible
        // and outlives the keystroke that produced it: the next mark on an ancestor would
        // quietly spare a subtree nobody spared.
        self.stale = true;
        self.sync();
        if self.mark_of(id) == Mark::None && !self.selects_anything_under(id) {
            return;
        }
        // Still covered from above, so it is spared rather than unmarked — and every mark
        // that lived inside it goes with it, since nothing under an exclusion is selected.
        let tree = &self.tree;
        self.marks
            .retain(|mark| !descends_from(tree, mark.root, id));
        self.spared.insert(id);
        self.stale = true;
    }

    /// Whether anything at all under this node is selected, visible or not.
    ///
    /// The glyph cannot answer this on its own: a subtree whose every selected claim is
    /// hidden draws as unmarked, and it still has to be sparable — that is the whole hazard
    /// the confirmation screen exists for, reached from the tree instead.
    fn selects_anything_under(&self, id: NodeId) -> bool {
        self.counts
            .get(id)
            .is_some_and(|counts| counts.all.claims > 0)
    }

    fn clear_marks(&mut self) {
        if !self.marks.is_empty() || !self.spared.is_empty() {
            self.mark_stamp += 1;
        }
        self.marks.clear();
        self.spared.clear();
        self.counts.clear();
        self.selection.clear();
        self.map_stamps.clear();
        self.stale = true;
    }

    /// Rebuilds everything the lens and the marks decide, in one pass. See [`tally`].
    ///
    /// Skipped entirely on the view a run opens with — nothing marked and nothing hidden —
    /// so a reader watching a scan of a home directory pays for none of it. From the first
    /// mark on it is one traversal per frame, which is the price of a glyph that is
    /// **filter-relative**: an ancestor can be fully marked under `dependencies` and partly
    /// marked under `all`, and a glyph computed globally would contradict what the reader can
    /// see. The alternative — a subtree walk per row per frame — is what the cache the old
    /// `below` map existed for was already avoiding, and this keeps that shape rather than
    /// giving it up.
    fn recount(&mut self) {
        self.counts.clear();
        self.selection.clear();
        self.map_stamps.clear();
        if !self.is_sifted() && self.marks.is_empty() {
            return;
        }
        let mut out = Tallied {
            counts: std::mem::take(&mut self.counts),
            selection: std::mem::take(&mut self.selection),
            map_stamps: std::mem::take(&mut self.map_stamps),
        };
        // The stamps are sized only when there is a map to draw, because the map is the only
        // thing that asks and most terminals never have one. Left empty is how [`tally`] is
        // told not to fold them — see [`View::map_stamp`] for what a run without one falls
        // back to.
        if self.maps() {
            out.map_stamps.resize(self.tree.minted(), 0);
        }
        tally(
            &self.tree,
            &self.lens,
            &self.marks,
            &self.spared,
            &self.moving,
            &mut out,
        );
        self.counts = out.counts;
        self.selection = out.selection;
        self.map_stamps = out.map_stamps;
    }

    /// Whether the lens is hiding anything at all.
    fn is_sifted(&self) -> bool {
        !self.lens.is_everything()
    }

    /// Whether `id` is at or under `root`.
    fn descends_from(&self, id: NodeId, root: NodeId) -> bool {
        descends_from(&self.tree, id, root)
    }
}

/// Whether `id` is at or under `root`.
///
/// Walked upwards from the node rather than downwards from the root, because a chain is a
/// handful of steps and a subtree can be most of the tree. A free function so that it can be
/// asked while a `retain` holds the field it would otherwise be a method on.
fn descends_from(tree: &Tree, id: NodeId, root: NodeId) -> bool {
    let mut at = Some(id);
    while let Some(current) = at {
        if current == root {
            return true;
        }
        at = tree.node(current).parent;
    }
    false
}

/// Where a kind sorts in the listing, with the unnamed tier last.
///
/// Last rather than first deliberately: the groups a reader recognises come before the group
/// that says only that git knows about it, which is the one they will want to read most
/// carefully and so the one that should not be scrolled past on the way in.
fn kind_order(kind: Option<Kind>) -> usize {
    // Read off the vocabulary's own cost ordering rather than written out again, so the
    // confirmation groups the expensive end first without this file having a second opinion
    // about which end that is. A claim nothing named sorts last, after every kind.
    kind.map_or(Kind::ALL.len(), Kind::cost)
}

/// `1 directory`, `4 directories`.
#[must_use]
pub fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use super::{
        Action, Answer, Effect, Maps, Mark, Motion, Notice, Overlay, Planned, Preset, Turn, View,
    };
    use crate::delete::{Refusal, Refused};
    use crate::fixture::{gitignored, gitignored_file, hit, of_kind};
    use crate::rules::Kind;
    use crate::size::Size;
    use crate::tree::{Order, Sort, Tree};
    use crate::tui::moving::{ARRIVAL, COUNT_UP, DIM, FLASH, RUNG};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// A view over a fixed little tree:
    ///
    /// ```text
    /// /scan
    ///   nx           300
    ///     node_modules  200
    ///     packages
    ///       ui/node_modules  100
    ///   old          10
    ///     target        10
    /// ```
    fn view() -> View {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/nx/node_modules", Size::Measured(200), 900));
        tree.insert(hit(
            "/scan/nx/packages/ui/node_modules",
            Size::Measured(100),
            800,
        ));
        tree.insert(hit("/scan/old/target", Size::Measured(10), 100));
        let mut view = View::new(tree);
        view.viewport(40);
        view
    }

    /// Every visible row, as `<indent><name>`.
    fn shown(view: &View) -> Vec<String> {
        view.rows()
            .iter()
            .map(|row| {
                format!(
                    "{}{}",
                    "  ".repeat(row.depth),
                    view.tree().node(row.id).name.to_string_lossy()
                )
            })
            .collect()
    }

    fn at(view: &View, path: &str) -> crate::tree::NodeId {
        view.tree().find(Path::new(path)).unwrap()
    }

    /// Runs the clock past the drain, which is what actually takes a removed row out of the
    /// tree. Every test written before the drain existed used a bare `sync` for this, and the
    /// substitution is exact: the removal still happens, a third of a second later.
    fn settle(view: &mut View) {
        view.animate(Instant::now() + DIM * 2);
    }

    /// Moves the cursor onto a row that is already visible, using only the keys a reader has.
    fn select(view: &mut View, path: &str) {
        let want = PathBuf::from(path);
        let at = view
            .rows()
            .iter()
            .position(|row| view.tree().node(row.id).path == want)
            .unwrap_or_else(|| panic!("{path} is not on screen"));
        view.apply(Action::Cursor(Motion::Top));
        for _ in 0..at {
            view.apply(Action::Cursor(Motion::Down));
        }
    }

    /// Opens every directory on the way to `path` and puts the cursor on it.
    ///
    /// The target itself is left exactly as it was, opened or closed, because half these
    /// tests are about what a key does to a *collapsed* row.
    fn point_at(view: &mut View, path: &str) {
        let mut above = PathBuf::from("/scan");
        let below = Path::new(path).strip_prefix("/scan").unwrap().to_path_buf();
        for component in below.components() {
            let at = view.tree().find(&above).unwrap();
            if !view.is_expanded(at) {
                select(view, &above.to_string_lossy());
                view.apply(Action::Expand);
            }
            above.push(component);
        }
        select(view, path);
    }

    // ---- the tree itself --------------------------------------------------------------

    #[test]
    fn everything_but_the_root_starts_closed() {
        let view = view();
        assert_eq!(shown(&view), ["/scan", "  nx", "  old"]);
    }

    #[test]
    fn a_row_opens_onto_its_own_children_only() {
        let mut view = view();
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Expand);
        assert_eq!(
            shown(&view),
            ["/scan", "  nx", "    node_modules", "    packages", "  old"]
        );
    }

    #[test]
    fn opening_an_open_row_steps_into_it_and_closing_a_closed_one_steps_out() {
        let mut view = view();
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Expand);
        view.apply(Action::Expand);
        assert_eq!(
            view.tree().node(view.row().unwrap().id).path,
            PathBuf::from("/scan/nx/node_modules")
        );
        view.apply(Action::Collapse);
        assert_eq!(
            view.tree().node(view.row().unwrap().id).path,
            PathBuf::from("/scan/nx")
        );
    }

    #[test]
    fn the_star_key_opens_a_whole_subtree_and_z_closes_everything() {
        let mut view = view();
        point_at(&mut view, "/scan/nx");
        view.apply(Action::ToggleSubtree);
        assert_eq!(
            shown(&view),
            [
                "/scan",
                "  nx",
                "    node_modules",
                "    packages",
                "      ui",
                "        node_modules",
                "  old",
            ]
        );
        view.apply(Action::CollapseAll);
        assert_eq!(shown(&view), ["/scan", "  nx", "  old"]);
    }

    #[test]
    fn levels_sort_within_themselves_rather_than_globally() {
        let mut view = view();
        point_at(&mut view, "/scan/nx");
        view.apply(Action::ToggleSubtree);
        view.apply(Action::SortBy(Order::Path));
        assert_eq!(view.sort(), Sort::by(Order::Path));
        // `old` sorts after `nx` at the top level and stays there; the 100-byte
        // `ui/node_modules` stays under `packages` rather than sorting among the roots.
        assert_eq!(
            shown(&view),
            [
                "/scan",
                "  nx",
                "    node_modules",
                "    packages",
                "      ui",
                "        node_modules",
                "  old",
            ]
        );
    }

    #[test]
    fn a_row_that_has_not_been_priced_reads_as_unpriced_rather_than_as_empty() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/nx/node_modules", Size::Unmeasured, 900));
        let view = View::new(tree);

        let roll = view.roll(at(&view, "/scan/nx"));
        assert_eq!(roll.bytes, 0);
        assert_eq!(roll.unpriced, 1);
        assert_eq!(roll.label(), "—");

        // …and the moment a price lands it is a number, without the row moving.
        let mut view = view;
        view.priced(Path::new("/scan/nx/node_modules"), Size::Measured(2048));
        view.sync();
        assert_eq!(view.roll(at(&view, "/scan/nx")).label(), "2.0 KiB");
    }

    // ---- streaming --------------------------------------------------------------------

    #[test]
    fn a_claim_that_arrives_under_a_closed_row_moves_its_total_and_not_the_cursor() {
        let mut view = view();
        point_at(&mut view, "/scan/old");
        let before = view.total().bytes;

        view.found(hit(
            "/scan/nx/packages/api/node_modules",
            Size::Measured(5),
            1,
        ));
        view.sync();

        assert_eq!(view.total().bytes, before + 5);
        assert_eq!(
            view.tree().node(view.row().unwrap().id).path,
            PathBuf::from("/scan/old"),
            "an arrival elsewhere moved the cursor"
        );
    }

    #[test]
    fn a_claim_that_arrives_under_a_marked_row_is_marked_on_arrival() {
        let mut view = view();
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Mark);
        assert_eq!(view.marked().claims, 2);

        view.found(hit(
            "/scan/nx/packages/api/node_modules",
            Size::Measured(5),
            1,
        ));
        view.sync();

        // The reason a mark is a subtree root rather than the set of rows it covered: the
        // reader said "everything under nx", and the scan is still finding what that is.
        assert_eq!(view.marked().claims, 3);
        assert_eq!(view.marked().bytes, 305);
        assert!(batched(&view).contains(&PathBuf::from("/scan/nx/packages/api/node_modules")));
    }

    // ---- the cursor -------------------------------------------------------------------

    #[test]
    fn the_cursor_stays_on_its_directory_when_a_price_re_sorts_the_level_under_it() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/nx/node_modules", Size::Measured(200), 900));
        tree.insert(hit("/scan/old/target", Size::Unmeasured, 100));
        let mut view = View::new(tree);
        view.viewport(40);
        point_at(&mut view, "/scan/old");
        assert_eq!(view.cursor(), Some(2));

        // The claim arrived unpriced, as every tier-one claim does, and the pool has just
        // put a number on it that makes it the biggest thing in the scan. The row moves, by
        // design — and the cursor moves with it rather than staying on row 2, which is now
        // somebody else.
        view.priced(Path::new("/scan/old/target"), Size::Measured(9000));
        view.sync();

        assert_eq!(shown(&view), ["/scan", "  old", "  nx"]);
        assert_eq!(
            view.tree().node(view.row().unwrap().id).path,
            PathBuf::from("/scan/old")
        );
        assert_eq!(view.cursor(), Some(1));
    }

    #[test]
    fn a_deleted_row_leaves_the_cursor_on_the_nearest_directory_that_is_still_there() {
        let mut view = view();
        point_at(&mut view, "/scan/nx");
        view.apply(Action::ToggleSubtree);
        point_at(&mut view, "/scan/nx/packages/ui/node_modules");

        view.removed(Path::new("/scan/nx/packages/ui/node_modules"), 100, true);
        settle(&mut view);

        // `ui` and `packages` held nothing else, so they went too. `nx` is what is left of
        // where the reader was — and it is emphatically not row 0, which is the scan root.
        assert_eq!(
            view.tree().node(view.row().unwrap().id).path,
            PathBuf::from("/scan/nx")
        );
    }

    #[test]
    fn deleting_everything_a_reader_was_looking_at_lands_on_the_scan_root_and_not_on_a_stranger() {
        let mut view = view();
        point_at(&mut view, "/scan/old");
        view.removed(Path::new("/scan/old/target"), 10, true);
        settle(&mut view);

        // The chain ends at the scan root, so that is where a cursor with nothing else left
        // above it comes to rest. It is the *nearest surviving ancestor* rather than "row 0":
        // the difference shows here, because `nx` is what is at row 1 and the cursor has
        // deliberately not been handed it.
        assert_eq!(
            view.tree().node(view.row().unwrap().id).path,
            PathBuf::from("/scan")
        );
    }

    #[test]
    fn a_cursor_whose_whole_ancestry_is_off_screen_is_deselected_rather_than_moved_to_row_zero() {
        let mut view = view();
        point_at(&mut view, "/scan/old");
        filter(&mut view, "nothing matches this");

        assert!(view.rows().is_empty());
        assert_eq!(view.cursor(), None);

        // …and it stays deselected once there are rows again, rather than being handed row 0
        // by the next frame. Row 0 is the scan root, whose subtree is everything.
        view.apply(Action::Back);
        assert_eq!(view.cursor(), None);
        view.found(hit("/scan/other/node_modules", Size::Measured(1), 1));
        view.sync();
        assert_eq!(view.cursor(), None);

        // A deliberate keystroke is what picks a row again.
        view.apply(Action::Cursor(Motion::Down));
        assert_eq!(view.cursor(), Some(0));
    }

    #[test]
    fn a_target_the_deleter_could_not_finish_keeps_its_row() {
        let mut view = view();
        let before = view.total();

        view.removed(Path::new("/scan/old/target"), 0, false);
        view.sync();

        // The sweep went in and came out again — a checkout inside it, an unreadable
        // subtree. The directory is still on disk, so a row that vanished would be a lie.
        assert_eq!(view.total(), before);
        assert!(view.tree().find(Path::new("/scan/old/target")).is_some());
    }

    // ---- marking ----------------------------------------------------------------------

    #[test]
    fn marking_a_collapsed_row_marks_everything_beneath_it() {
        let mut view = view();
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Mark);

        assert_eq!(view.mark_of(at(&view, "/scan/nx")), Mark::All);
        assert_eq!(
            view.mark_of(at(&view, "/scan/nx/packages/ui/node_modules")),
            Mark::All
        );
        assert_eq!(view.mark_of(at(&view, "/scan/old/target")), Mark::None);
        assert_eq!(
            batched(&view),
            [
                PathBuf::from("/scan/nx/node_modules"),
                PathBuf::from("/scan/nx/packages/ui/node_modules"),
            ]
        );
    }

    #[test]
    fn an_ancestor_of_a_mark_shows_a_partial_state() {
        let mut view = view();
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Expand);
        point_at(&mut view, "/scan/nx/node_modules");
        view.apply(Action::Mark);

        assert_eq!(view.mark_of(at(&view, "/scan/nx")), Mark::Partial);
        assert_eq!(view.mark_of(view.tree().root()), Mark::Partial);
        assert_eq!(view.mark_of(at(&view, "/scan/old")), Mark::None);
    }

    #[test]
    fn unmarking_one_row_out_of_a_marked_subtree_spares_it_and_keeps_the_rest() {
        let mut view = view();
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Mark);
        view.apply(Action::ToggleSubtree);
        point_at(&mut view, "/scan/nx/node_modules");
        view.apply(Action::Mark);

        assert_eq!(view.mark_of(at(&view, "/scan/nx/node_modules")), Mark::None);
        assert_eq!(
            view.mark_of(at(&view, "/scan/nx/packages/ui/node_modules")),
            Mark::All
        );
        assert_eq!(view.mark_of(at(&view, "/scan/nx")), Mark::Partial);
        assert_eq!(
            batched(&view),
            [PathBuf::from("/scan/nx/packages/ui/node_modules")]
        );
    }

    #[test]
    fn marking_a_row_absorbs_the_marks_already_inside_it() {
        let mut view = view();
        point_at(&mut view, "/scan/nx");
        view.apply(Action::ToggleSubtree);
        point_at(&mut view, "/scan/nx/node_modules");
        view.apply(Action::Mark);
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Mark);

        assert_eq!(view.marked().claims, 2);
        assert_eq!(view.marked().bytes, 300);
        // …and unmarking the outer one leaves nothing behind, rather than uncovering the
        // inner mark it swallowed.
        view.apply(Action::Mark);
        assert_eq!(view.marked().claims, 0);
        assert_eq!(view.mark_of(at(&view, "/scan/nx/node_modules")), Mark::None);
    }

    #[test]
    fn a_key_marks_everything_and_the_same_key_clears_a_partial_selection() {
        let mut view = view();
        view.apply(Action::MarkAll);
        assert_eq!(view.marked().claims, 3);
        assert_eq!(view.marked().bytes, 310);

        view.apply(Action::MarkAll);
        assert_eq!(view.marked().claims, 0);

        // A partial selection clears rather than growing: a reader who has marked forty
        // directories can afford to lose the selection and cannot afford to gain thirty more.
        point_at(&mut view, "/scan/old");
        view.apply(Action::Mark);
        view.apply(Action::MarkAll);
        assert_eq!(view.marked().claims, 0);
    }

    #[test]
    fn a_mark_on_a_directory_the_deleter_has_taken_away_stops_counting() {
        let mut view = view();
        point_at(&mut view, "/scan/old");
        view.apply(Action::Mark);
        assert_eq!(view.marked().claims, 1);

        view.removed(Path::new("/scan/old/target"), 10, true);
        settle(&mut view);

        assert_eq!(view.marked().claims, 0);
        assert!(batched(&view).is_empty());
        assert_eq!(view.mark_of(view.tree().root()), Mark::None);
    }

    // ---- what is visible, and what that has to do with what is selected -----------------

    /// A view over a tree with one of each kind, plus a claim only git knows about:
    ///
    /// ```text
    /// /scan
    ///   nx
    ///     node_modules   200   Dependencies
    ///     dist           100   Build
    ///     .nx/cache       10   Cache
    ///     out              1   gitignored, kind unknown
    ///   old
    ///     target          20   Build
    /// ```
    fn mixed() -> View {
        let mut tree = Tree::new("/scan");
        tree.insert(sized(
            of_kind("/scan/nx/node_modules", Kind::Dependencies),
            200,
        ));
        tree.insert(sized(of_kind("/scan/nx/dist", Kind::Build), 100));
        tree.insert(sized(of_kind("/scan/nx/.nx/cache", Kind::Cache), 10));
        tree.insert(sized(gitignored("/scan/nx/out"), 1));
        tree.insert(sized(of_kind("/scan/old/target", Kind::Build), 20));
        let mut view = View::new(tree);
        view.viewport(40);
        view
    }

    /// A made-up claim with a price on it.
    fn sized(mut made: crate::walk::Hit, bytes: u64) -> crate::walk::Hit {
        made.size = Size::Measured(bytes);
        made
    }

    /// Every claim the current view shows, by path.
    fn shown_claims(view: &View) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = (0..view.tree().minted())
            .filter(|&id| view.tree().is_attached(id))
            .filter(|&id| view.tree().node(id).hit.is_some() && view.roll(id).claims > 0)
            .map(|id| view.tree().node(id).path.clone())
            .collect();
        found.sort();
        found
    }

    /// Presses `f` until the view is the one named.
    ///
    /// One press more than there are presets, because from a view the axis keys built the first
    /// press lands on `default` rather than stepping from a place the reader never was.
    fn showing(view: &mut View, preset: Preset) {
        for _ in 0..=Preset::ALL.len() {
            if view.preset() == Some(preset) {
                return;
            }
            view.apply(Action::CyclePreset(Turn::Next));
        }
        panic!("{preset} is not on the cycle");
    }

    #[test]
    fn a_run_opens_on_default_and_the_header_says_what_default_leaves_out() {
        // `default` narrows: it shows what rules named and hides the gitignore fallback. That
        // is a filter that is on without having been asked for, which is the shape the age
        // floor was resolved against — so what makes it honest rather than *silent* is that
        // the count it hides is on the header from the first frame, beside the number it
        // qualifies. A narrowed headline that does not say it is narrowed is the failure.
        let view = mixed();
        assert_eq!(view.preset(), Some(Preset::Default));
        assert_eq!(view.total().claims, 4);
        assert_eq!(view.out_of_view(), 1);
        assert_eq!(view.view_label(), "default");
    }

    #[test]
    fn one_key_walks_the_four_views_that_were_asked_for_in_that_order() {
        let mut view = mixed();
        let seen = |view: &View| view.total().claims;

        // default: everything a rule put a name to, and not the gitignored one.
        assert_eq!(view.preset(), Some(Preset::Default));
        assert_eq!(seen(&view), 4);

        view.apply(Action::CyclePreset(Turn::Next));
        // dependencies: one axis narrowed, the other left exactly as default had it.
        assert_eq!(view.preset(), Some(Preset::Dependencies));
        assert_eq!(seen(&view), 1);

        view.apply(Action::CyclePreset(Turn::Next));
        // all-ignored: the TIER axis widens and the kind narrowing is RETAINED, so this is the
        // step before it plus the gitignored tier — two claims, not five.
        assert_eq!(view.preset(), Some(Preset::AllIgnored));
        assert_eq!(seen(&view), 2);

        view.apply(Action::CyclePreset(Turn::Next));
        // all: the kind axis widens too, which is everything.
        assert_eq!(view.preset(), Some(Preset::All));
        assert_eq!(seen(&view), 5);
        assert_eq!(view.out_of_view(), 0);

        view.apply(Action::CyclePreset(Turn::Next));
        assert_eq!(view.preset(), Some(Preset::Default));

        // …and backwards, because a reader who overshoots by one keystroke should not have to
        // go all the way round.
        view.apply(Action::CyclePreset(Turn::Prev));
        assert_eq!(view.preset(), Some(Preset::All));
    }

    #[test]
    fn each_step_of_the_cycle_moves_one_axis_and_carries_the_other() {
        // What makes the asked-for order a path rather than four unrelated points, seen through
        // the keys: `dependencies` narrows the kind, `all-ignored` widens the tier and KEEPS
        // that narrowing, and `all` widens the kind back. Four presses, four different screens.
        let mut view = mixed();
        let seen = |view: &View| shown_claims(view);

        assert_eq!(view.view_label(), "default");
        view.apply(Action::CyclePreset(Turn::Next));
        assert_eq!(seen(&view), [PathBuf::from("/scan/nx/node_modules")]);

        view.apply(Action::CyclePreset(Turn::Next));
        assert_eq!(
            seen(&view),
            [
                PathBuf::from("/scan/nx/node_modules"),
                PathBuf::from("/scan/nx/out"),
            ],
            "all-ignored dropped the kind narrowing instead of carrying it"
        );

        view.apply(Action::CyclePreset(Turn::Next));
        assert_eq!(view.total().claims, 5);
    }

    #[test]
    fn no_preset_touches_the_pattern() {
        // The pattern narrows whatever the axes leave, so it is orthogonal to both — and a
        // preset that quietly dropped it would be using an unrelated piece of state to mean
        // something about the view. An earlier pass did exactly that to tell two presets apart.
        let mut view = mixed();
        filter(&mut view, "nx");
        for _ in 0..=Preset::ALL.len() {
            view.apply(Action::CyclePreset(Turn::Next));
            assert_eq!(view.filter(), Some("nx"), "{:?}", view.preset());
        }
    }

    #[test]
    fn the_two_axes_compose_rather_than_replacing_each_other() {
        // The whole reason the filter is two axes rather than four modes: a pattern narrows
        // whatever the axes left, and neither has to know the other exists.
        let mut view = mixed();
        showing(&mut view, Preset::Default);
        filter(&mut view, "nx");
        assert_eq!(
            view.total().claims,
            3,
            "the gitignored one is out either way"
        );
        assert_eq!(
            view.lens().describe(),
            "named · every kind · /nx"
        );
    }

    #[test]
    fn each_axis_has_a_key_of_its_own_so_a_non_preset_view_is_reachable() {
        // **Expressible has to mean expressible by a reader.** "Every cache a rule named" is
        // not one of the four presets and never will be, and a model that can hold it while no
        // keystroke can ask for it is a model with a claim it cannot cash. `d` and `b` take the
        // other two kinds off `default`, and what is left is exactly that view.
        let mut view = mixed();
        for kind in Kind::ALL.into_iter().filter(|&kind| kind != Kind::Cache) {
            view.apply(Action::ToggleKind(kind));
        }

        assert_eq!(view.preset(), None, "a hand-built view is not a preset");
        assert_eq!(view.view_label(), "named · cache");
        assert_eq!(shown_claims(&view), [PathBuf::from("/scan/nx/.nx/cache")]);

        // …and the tier axis moves on its own, leaving the kind axis exactly where it was.
        view.apply(Action::CycleTiers);
        assert_eq!(view.view_label(), "named + gitignored · cache");
        assert_eq!(
            shown_claims(&view),
            [
                PathBuf::from("/scan/nx/.nx/cache"),
                PathBuf::from("/scan/nx/out"),
            ]
        );

        // Once more and the named tier goes, which is the third state of that axis and the one
        // no preset names either.
        view.apply(Action::CycleTiers);
        assert_eq!(view.view_label(), "gitignored · cache");
        assert_eq!(shown_claims(&view), [PathBuf::from("/scan/nx/out")]);
    }

    #[test]
    fn moving_either_axis_by_hand_leaves_the_selection_exactly_where_it_was() {
        // The orthogonality rule does not get to be true only for the presets. An axis key is a
        // change to what is *visible*, so it must be as inert on the marks as `f` is.
        let mut view = mixed();
        showing(&mut view, Preset::All);
        point_at(&mut view, "/scan");
        view.apply(Action::Mark);
        let whole = batched(&view);
        assert_eq!(whole.len(), 5);

        for action in [
            Action::ToggleKind(Kind::Dependencies),
            Action::CycleTiers,
            Action::ToggleKind(Kind::Cache),
            Action::CycleTiers,
            Action::ToggleKind(Kind::Build),
        ] {
            view.apply(action);
            assert_eq!(batched(&view), whole, "{action:?} changed the batch");
            assert_eq!(view.marked().claims, 5, "{action:?} changed the counter");
        }
        // …and by this point the view shows nothing at all, which is a legitimate place for
        // the axes to be and changes nothing whatever about what is going to be deleted. That
        // is the strongest form of the rule: a screen with no rows on it and a batch of five.
        assert_eq!(view.total().claims, 0);
        assert_eq!(view.hidden(), 5);
        assert_eq!(batched(&view).len(), 5);
    }

    #[test]
    fn a_hand_built_view_that_lands_on_a_preset_is_called_by_its_name() {
        // The other half of naming the view honestly: a reader who toggles their way onto
        // `dependencies` is on `dependencies`, and the footer should say so rather than
        // spelling out axes that have a name.
        let mut view = mixed();
        for kind in Kind::ALL
            .into_iter()
            .filter(|&kind| kind != Kind::Dependencies)
        {
            view.apply(Action::ToggleKind(kind));
        }
        assert_eq!(view.preset(), Some(Preset::Dependencies));
        assert_eq!(view.view_label(), "dependencies");

        // …and one that lands nowhere near a preset says the axes instead, because that is the
        // only description a nameless view has.
        view.apply(Action::ToggleKind(Kind::Dependencies));
        view.apply(Action::ToggleKind(Kind::Build));
        view.apply(Action::CycleTiers);
        assert_eq!(view.preset(), None);
        assert_eq!(view.view_label(), "named + gitignored · build");
    }

    #[test]
    fn toggling_what_is_visible_never_changes_what_is_selected() {
        // **The rule the whole model turns on.** Hiding a row is not unselecting it.
        let mut view = mixed();
        // Marked through the widest view, so what follows is the whole scan being narrowed
        // around a selection rather than a selection that was never that big.
        showing(&mut view, Preset::All);
        point_at(&mut view, "/scan");
        view.apply(Action::Mark);
        let whole = batched(&view);
        assert_eq!(whole.len(), 5);

        for preset in Preset::ALL {
            showing(&mut view, preset);
            assert_eq!(batched(&view), whole, "{preset} changed the batch");
            assert_eq!(view.marked().claims, 5, "{preset} changed the counter");
        }
    }

    #[test]
    fn a_mark_keeps_meaning_the_view_it_was_made_through() {
        // Mark `~/repos` under Dependencies, widen to All, and the build artefacts under it
        // are still unmarked. A mark stored as "the subtree under N" could not do this: it
        // would re-derive under the new view and quietly take everything.
        let mut view = mixed();
        showing(&mut view, Preset::Dependencies);
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Mark);
        assert_eq!(batched(&view), [PathBuf::from("/scan/nx/node_modules")]);

        showing(&mut view, Preset::All);
        assert_eq!(
            batched(&view),
            [PathBuf::from("/scan/nx/node_modules")],
            "widening the view widened the selection"
        );
        assert_eq!(view.mark_of(at(&view, "/scan/nx")), Mark::Partial);
    }

    #[test]
    fn a_second_mark_through_a_second_view_adds_to_the_first() {
        // The axes are a way of saying what to select, not a mode the selection lives in, so
        // two passes over one directory under two views is a union rather than a replacement.
        // The second view here is one the axis keys built and no preset names, which is the
        // point: a mark carries whatever the reader could see, preset or not.
        let mut view = mixed();
        showing(&mut view, Preset::Dependencies);
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Mark);

        view.apply(Action::CycleTiers);
        view.apply(Action::CycleTiers);
        view.apply(Action::ToggleKind(Kind::Dependencies));
        assert_eq!(view.view_label(), "gitignored · none");
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Mark);

        showing(&mut view, Preset::All);
        assert_eq!(
            batched(&view),
            [
                PathBuf::from("/scan/nx/node_modules"),
                PathBuf::from("/scan/nx/out"),
            ]
        );
    }

    #[test]
    fn the_partial_glyph_is_computed_against_the_view_the_reader_is_looking_through() {
        // An ancestor can be FULLY marked under one view and PARTIALLY marked under another,
        // and the glyph has to say which — a box drawn full over rows that are visibly
        // unmarked is the screen contradicting itself.
        let mut view = mixed();
        showing(&mut view, Preset::Dependencies);
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Mark);
        assert_eq!(view.mark_of(at(&view, "/scan/nx")), Mark::All);
        assert!((view.share(at(&view, "/scan/nx")) - 1.0).abs() < f64::EPSILON);

        showing(&mut view, Preset::All);
        assert_eq!(view.mark_of(at(&view, "/scan/nx")), Mark::Partial);
        // 200 of the 311 bytes under `nx` are marked, and the glyph says so rather than
        // merely saying "some".
        assert!((view.share(at(&view, "/scan/nx")) - 200.0 / 311.0).abs() < 0.001);

        // The case a globally-computed glyph gets exactly backwards: a selection that is
        // entirely out of sight, over a row whose visible claims are all unmarked. Counting
        // the whole selection would draw the box FULL over rows the reader can see are empty.
        //
        // Changing the view says so in the footer, and that sentence is a rung of its own —
        // so it is taken by name rather than by spending one of the two `Esc`s below on it.
        // Those two are the rungs this setup is actually after: the view, then the marks.
        view.apply(Action::Dismiss);
        view.apply(Action::Back);
        view.apply(Action::Back);
        assert!(batched(&view).is_empty());
        point_at(&mut view, "/scan/nx/dist");
        view.apply(Action::Mark);
        showing(&mut view, Preset::Dependencies);
        assert_eq!(view.marked().claims, 1, "the selection is still there");
        assert_eq!(view.mark_of(at(&view, "/scan/nx")), Mark::None);
    }

    #[test]
    fn a_claim_that_streams_in_under_a_mark_joins_it_when_it_matches_that_marks_view() {
        // Why the pair is resolved on demand rather than frozen into a list of ids: results
        // stream in, so a subtree marked at seven seconds has to pick up what arrives at
        // forty. And only what the mark's own view would have taken.
        let mut view = mixed();
        showing(&mut view, Preset::Dependencies);
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Mark);

        view.found(sized(
            of_kind("/scan/nx/packages/ui/node_modules", Kind::Dependencies),
            50,
        ));
        view.found(sized(of_kind("/scan/nx/packages/ui/dist", Kind::Build), 50));
        view.sync();

        showing(&mut view, Preset::All);
        assert_eq!(
            batched(&view),
            [
                PathBuf::from("/scan/nx/node_modules"),
                PathBuf::from("/scan/nx/packages/ui/node_modules"),
            ],
            "a build artefact joined a dependencies mark"
        );
    }

    #[test]
    fn sparing_one_row_out_of_a_marked_subtree_keeps_sparing_it_as_more_arrives() {
        // The exclusion's advantage over the push-down it replaced. A push-down marks every
        // sibling *that exists at that instant*, so a claim arriving beside the spared row a
        // minute later would silently be spared too — a statement about a moment, standing in
        // for a statement about a directory.
        let mut view = mixed();
        point_at(&mut view, "/scan");
        view.apply(Action::Mark);
        point_at(&mut view, "/scan/nx/dist");
        view.apply(Action::Mark);
        assert!(!batched(&view).contains(&PathBuf::from("/scan/nx/dist")));

        view.found(sized(of_kind("/scan/nx/late", Kind::Build), 5));
        view.sync();
        assert!(
            batched(&view).contains(&PathBuf::from("/scan/nx/late")),
            "a claim that arrived beside the spared row was spared too"
        );
        assert!(!batched(&view).contains(&PathBuf::from("/scan/nx/dist")));
    }

    #[test]
    fn a_spared_subtree_can_be_marked_again_from_inside_it() {
        // Marks and exclusions interleave down a path, and the deepest thing on it is what
        // speaks. A shallower mark must not be able to reach back through an exclusion below
        // it, and an exclusion must not be able to hold out against a mark below itself.
        let mut view = mixed();
        point_at(&mut view, "/scan");
        view.apply(Action::Mark);
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Mark);
        assert_eq!(batched(&view), [PathBuf::from("/scan/old/target")]);

        point_at(&mut view, "/scan/nx/dist");
        view.apply(Action::Mark);
        assert_eq!(
            batched(&view),
            [
                PathBuf::from("/scan/nx/dist"),
                PathBuf::from("/scan/old/target"),
            ]
        );
    }

    #[test]
    fn unmarking_a_row_nothing_was_covering_leaves_no_exclusion_behind() {
        // An exclusion is invisible: two views with the same rows drawn, the same glyphs and
        // the same batch can differ by one, and it only shows up in what a *later* keystroke
        // does. Today every later keystroke that could be affected happens to clear it —
        // marking a directory drops the exclusions beneath it — so this is a test about the
        // state rather than about an observable difference, and that is the point. The
        // question `unmark` asks is answered by the counts, and the counts are one keystroke
        // behind until the pass is re-run.
        let mut view = mixed();
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Mark);
        view.apply(Action::Mark);

        assert!(batched(&view).is_empty());
        assert!(view.marks.is_empty());
        assert!(
            view.spared.is_empty(),
            "a plain unmark left an exclusion for an ancestor that does not exist"
        );
    }

    #[test]
    fn the_batch_is_the_whole_selection_and_the_counter_says_how_much_is_out_of_sight() {
        // Deleting acts on everything that is marked, so the number a reader checks has to
        // describe the same set. What narrowing the view changes is the count beside it.
        let mut view = mixed();
        showing(&mut view, Preset::All);
        point_at(&mut view, "/scan");
        view.apply(Action::Mark);
        assert_eq!(view.marked().claims, 5);
        assert_eq!(view.hidden(), 0);

        showing(&mut view, Preset::Dependencies);
        assert_eq!(view.marked().claims, 5, "the counter followed the view");
        assert_eq!(batched(&view).len(), 5, "the batch followed the view");
        assert_eq!(view.hidden(), 4);
        assert!(
            view.notice().unwrap().contains("out of sight"),
            "{:?}",
            view.notice()
        );
    }

    #[test]
    fn the_confirmation_lists_the_whole_batch_and_names_what_is_out_of_sight() {
        let mut view = mixed();
        showing(&mut view, Preset::All);
        point_at(&mut view, "/scan");
        view.apply(Action::Mark);
        showing(&mut view, Preset::Dependencies);
        let Effect::Plan(batch) = view.apply(Action::Commit) else {
            panic!("the key that writes did not ask");
        };
        view.asking(
            &batch
                .iter()
                .map(|target| Planned::at(target.path.clone(), target.size))
                .collect::<Vec<_>>(),
            &[],
        );

        let pending = view.pending().unwrap();
        assert_eq!(pending.entries().len(), 5);
        assert_eq!(pending.hidden(), 4);
        // Grouped by kind: dependencies, then build, then cache, then the tier nothing named.
        let kinds: Vec<Option<Kind>> = pending.entries().iter().map(|entry| entry.kind).collect();
        assert_eq!(
            kinds,
            [
                Some(Kind::Dependencies),
                Some(Kind::Build),
                Some(Kind::Build),
                Some(Kind::Cache),
                None,
            ]
        );
        // …and each line says whether the reader can currently see it.
        let seen: Vec<bool> = pending.entries().iter().map(|entry| entry.hidden).collect();
        assert_eq!(seen, [false, true, true, true, true]);
    }

    // ---- the report and the confirmation are two things, not one ----------------------
    //
    // Both are drawn over the tree and both say something about a batch, which is the whole
    // reason to pin them apart. A [`Notice`] is a report of what has already happened and its
    // lifetime is a reader's action; the confirmation is the question asked *before* anything
    // happens, and it is mandatory whenever part of the selection is out of sight. Nothing
    // that gets rid of the first may touch the second.

    #[test]
    fn a_standing_report_does_not_stand_in_for_the_confirmation() {
        let mut view = mixed();
        showing(&mut view, Preset::All);
        point_at(&mut view, "/scan");
        view.apply(Action::Mark);
        showing(&mut view, Preset::Dependencies);
        assert_eq!(view.hidden(), 4);

        // The stickiest thing the footer can be holding: a report naming what the safety model
        // left alone, which waits to be dismissed rather than going with the next keystroke.
        view.deleted(
            Notice::standing("removed 10 B from 1 directory, 1 directory left alone"),
            10,
        );

        // It does not answer the question, so it must not be allowed to look like an answer.
        // `x` still plans, and the box still opens on the whole selection — four fifths of
        // which the reader cannot currently see.
        let Effect::Plan(batch) = view.apply(Action::Commit) else {
            panic!("a report in the footer swallowed the batch");
        };
        assert!(view.notice_stands(), "the report went with the keystroke");
        view.asking(
            &batch
                .iter()
                .map(|target| Planned::at(target.path.clone(), target.size))
                .collect::<Vec<_>>(),
            &[],
        );

        let pending = view
            .pending()
            .expect("no confirmation over a standing report");
        assert_eq!(pending.entries().len(), 5);
        assert_eq!(pending.hidden(), 4);
        // Both on the screen at once, saying different things about different moments.
        assert!(view.notice().is_some());
    }

    #[test]
    fn getting_rid_of_the_report_never_gets_rid_of_the_confirmation() {
        let mut view = mixed();
        showing(&mut view, Preset::All);
        point_at(&mut view, "/scan");
        view.apply(Action::Mark);
        showing(&mut view, Preset::Dependencies);
        view.deleted(Notice::standing("1 directory left alone"), 10);
        view.asking(&planned(&["/scan/nx/node_modules", "/scan/nx/dist"]), &[]);
        assert!(view.pending().is_some());

        // A press on the footer is aimed at the sentence and nothing else. The confirmation is
        // the one screen where a batch the reader cannot fully see can still be changed, so a
        // gesture that means "I have read that" must never be what takes it away.
        view.apply(Action::Dismiss);
        assert_eq!(view.notice(), None);
        assert!(
            view.pending().is_some(),
            "dismissing the report took the question with it"
        );

        // And on the ladder the box is in front: one `Esc` closes the question, and a report
        // that was underneath it is still underneath it afterwards.
        view.deleted(Notice::standing("1 directory left alone"), 10);
        view.asking(&planned(&["/scan/nx/node_modules"]), &[]);
        view.apply(Action::Back);
        assert!(view.pending().is_none(), "Esc did not take the box first");
        assert!(view.notice().is_some(), "Esc took both rungs at once");
    }

    #[test]
    fn an_entry_can_be_taken_out_of_the_batch_from_the_confirmation_itself() {
        // The answer to a surprise has to be better than "cancel and start again" — especially
        // when the surprising row is one the current view is not even showing.
        let mut view = mixed();
        showing(&mut view, Preset::All);
        point_at(&mut view, "/scan");
        view.apply(Action::Mark);
        showing(&mut view, Preset::Dependencies);
        view.asking(&planned(&["/scan/nx/node_modules", "/scan/nx/dist"]), &[]);

        // The cursor starts on the first line, and `↓` reaches the hidden one.
        view.apply(Action::Listing(Motion::Down));
        assert_eq!(
            view.pending().unwrap().entries()[view.pending().unwrap().at()].path,
            PathBuf::from("/scan/nx/dist")
        );
        view.apply(Action::Spare);

        let pending = view.pending().unwrap();
        assert_eq!(pending.entries().len(), 1);
        assert_eq!(pending.targets, [PathBuf::from("/scan/nx/node_modules")]);
        // The deed shrank with the line, and so did the tree behind the box: a listing that
        // stopped showing a directory while the marks kept it would be the disagreement the
        // screen exists to prevent.
        assert!(!batched(&view).contains(&PathBuf::from("/scan/nx/dist")));
        assert_eq!(view.hidden(), 3);
    }

    #[test]
    fn taking_the_last_entry_out_closes_the_question_rather_than_asking_an_empty_one() {
        let mut view = mixed();
        point_at(&mut view, "/scan/nx/dist");
        view.apply(Action::Mark);
        view.asking(&planned(&["/scan/nx/dist"]), &[]);
        view.apply(Action::Spare);

        assert_eq!(view.overlay(), None);
        assert!(view.notice().unwrap().contains("nothing left"));
        assert!(batched(&view).is_empty());
    }

    #[test]
    fn a_refused_directory_says_so_on_the_confirmation_rather_than_in_the_report() {
        // The same refusal reporting, moved to the one moment where it can still change a
        // decision. A reader who marked forty and is going to get thirty-eight should not
        // learn that afterwards.
        let mut view = mixed();
        view.asking(
            &planned(&["/scan/nx/node_modules"]),
            &[Refused {
                path: "/scan/old/target".into(),
                reason: Refusal::HoldsCheckout,
            }],
        );
        let pending = view.pending().unwrap();
        assert_eq!(pending.kept(), 1);
        let refused = pending
            .entries()
            .iter()
            .find(|entry| entry.kept.is_some())
            .unwrap();
        assert_eq!(refused.path, PathBuf::from("/scan/old/target"));
        assert!(refused.kept.as_ref().unwrap().contains("git checkout"));
        // It is not on the deed, which is the point of saying it: the box promises exactly
        // what it lists as going.
        assert_eq!(pending.targets, [PathBuf::from("/scan/nx/node_modules")]);
    }

    #[test]
    fn escape_takes_the_pattern_off_before_the_view_and_the_marks_last_of_all() {
        // Two independent narrowings, so they come off as two rungs. A reader who typed a
        // pattern over `dependencies` means to lose the pattern.
        let mut view = mixed();
        showing(&mut view, Preset::Dependencies);
        filter(&mut view, "nx");
        point_at(&mut view, "/scan");
        view.apply(Action::Mark);

        view.apply(Action::Back);
        assert_eq!(view.filter(), None);
        assert_eq!(view.preset(), Some(Preset::Dependencies));

        view.apply(Action::Back);
        assert_eq!(view.preset(), Some(Preset::Default));
        assert!(!batched(&view).is_empty(), "the marks went with the view");

        view.apply(Action::Back);
        assert!(batched(&view).is_empty());
    }

    // ---- the filter -------------------------------------------------------------------

    #[test]
    fn a_filter_keeps_the_ancestors_of_what_it_matches() {
        let mut view = view();
        filter(&mut view, "ui/node_modules");
        assert_eq!(view.filter(), Some("ui/node_modules"));
        assert_eq!(shown(&view), ["/scan", "  nx"]);
    }

    #[test]
    fn a_filtered_row_is_worth_what_the_filter_shows_rather_than_what_is_under_it() {
        let mut view = view();
        filter(&mut view, "ui/node_modules");

        // `nx` is worth 300 in the scan and 100 of that matches. Showing 300 over a filtered
        // tree would be a row whose mark deletes twice what it says.
        assert_eq!(view.roll(at(&view, "/scan/nx")).bytes, 100);
        assert_eq!(view.roll(at(&view, "/scan/nx")).claims, 1);
        assert_eq!(view.total().bytes, 100);
    }

    #[test]
    fn marking_a_filtered_row_never_deletes_what_the_filter_is_hiding() {
        let mut view = view();
        filter(&mut view, "ui/node_modules");
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Mark);

        // The safety rule the filtered rollup exists for: `nx/node_modules` is under a marked
        // row, and it is not on screen, so it is not in the batch.
        assert_eq!(
            batched(&view),
            [PathBuf::from("/scan/nx/packages/ui/node_modules")]
        );
        assert_eq!(view.marked().bytes, 100);
    }

    #[test]
    fn a_pattern_the_engine_refuses_leaves_the_prompt_up_and_says_why() {
        let mut view = view();
        view.apply(Action::OpenFilter);
        for character in "node_(".chars() {
            view.apply(Action::Type(character));
        }
        view.apply(Action::Submit);

        assert_eq!(view.overlay(), Some(Overlay::Prompt));
        assert!(view.prompt().unwrap().error().is_some());
        // Closing the prompt on a bad pattern and showing an unfiltered tree would look
        // exactly like a filter that matched everything.
        assert_eq!(view.filter(), None);
    }

    #[test]
    fn the_prompt_edits_like_a_text_field() {
        let mut view = view();
        view.apply(Action::OpenFilter);
        for character in "node".chars() {
            view.apply(Action::Type(character));
        }
        view.apply(Action::Caret(Motion::Top));
        view.apply(Action::Type('x'));
        assert_eq!(view.prompt().unwrap().text(), "xnode");
        assert_eq!(view.prompt().unwrap().caret(), 1);
        view.apply(Action::Erase);
        assert_eq!(view.prompt().unwrap().text(), "node");
        view.apply(Action::EraseAhead);
        assert_eq!(view.prompt().unwrap().text(), "ode");
        view.apply(Action::Wipe);
        assert_eq!(view.prompt().unwrap().text(), "");
        // An empty pattern submitted is how a filter is taken off.
        view.apply(Action::Submit);
        assert_eq!(view.filter(), None);
        assert_eq!(view.overlay(), None);
    }

    // ---- committing a batch -----------------------------------------------------------

    #[test]
    fn the_delete_key_asks_before_anything_leaves_the_view() {
        let mut view = view();
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Mark);

        let effect = view.apply(Action::Commit);

        // It hands the batch out to be *planned*. Nothing is removed by pressing it, and the
        // view has no way to remove anything itself.
        let Effect::Plan(targets) = effect else {
            panic!("the delete key did something other than ask for a plan");
        };
        assert_eq!(
            targets.iter().map(|t| t.path.clone()).collect::<Vec<_>>(),
            [
                PathBuf::from("/scan/nx/node_modules"),
                PathBuf::from("/scan/nx/packages/ui/node_modules"),
            ]
        );
        // …carrying what the scan priced, because a plan built from bare paths would offer
        // to delete 300 bytes of `node_modules` "giving back 0 B" — which is what the box
        // said before this was a `Target`.
        assert_eq!(targets[0].size, Size::Measured(200));
        assert_eq!(view.overlay(), None);
    }

    #[test]
    fn the_confirmation_opens_on_cancel_and_enter_takes_the_highlighted_answer() {
        let mut view = view();
        view.asking(&planned(&["/scan/old/target"]), &[]);
        assert_eq!(view.overlay(), Some(Overlay::Confirm));
        assert_eq!(view.pending().unwrap().answer, Answer::Cancel);

        assert_eq!(view.apply(Action::Answer), Effect::None);
        assert_eq!(view.overlay(), None);
        assert!(!view.is_deleting());

        view.asking(&planned(&["/scan/old/target"]), &[]);
        view.apply(Action::Highlight(Turn::Next));
        assert_eq!(
            view.apply(Action::Answer),
            Effect::Delete(vec![PathBuf::from("/scan/old/target")])
        );
        assert!(view.is_deleting());
    }

    #[test]
    fn the_deed_is_what_the_question_named_rather_than_what_is_marked_when_it_is_answered() {
        let mut view = view();
        view.asking(&planned(&["/scan/old/target"]), &[]);

        // The tree moves while a box is up: a claim arrives, and the reader marks it. The
        // answer must still mean the directory the box described.
        view.found(hit("/scan/late/node_modules", Size::Measured(1), 1));
        view.sync();
        point_at(&mut view, "/scan/late");
        view.apply(Action::Mark);

        view.apply(Action::Highlight(Turn::Next));
        assert_eq!(
            view.apply(Action::Answer),
            Effect::Delete(vec![PathBuf::from("/scan/old/target")])
        );
    }

    #[test]
    fn committing_nothing_says_so_instead_of_asking_an_empty_question() {
        let mut view = view();
        assert_eq!(view.apply(Action::Commit), Effect::None);
        assert!(view.notice().unwrap().contains("nothing is marked"));

        // …and so does a batch the safety model refused in full.
        view.asking(
            &[],
            &[Refused {
                path: "/scan/old/target".into(),
                reason: Refusal::HoldsCheckout,
            }],
        );
        assert_eq!(view.overlay(), None);
        assert!(view.notice().unwrap().contains("left alone"));
    }

    #[test]
    fn a_second_delete_while_one_is_running_is_refused_rather_than_racing_it() {
        let mut view = view();
        view.asking(&planned(&["/scan/old/target"]), &[]);
        view.apply(Action::Highlight(Turn::Next));
        view.apply(Action::Answer);

        point_at(&mut view, "/scan/nx");
        view.apply(Action::Mark);
        assert_eq!(view.apply(Action::Commit), Effect::None);
        assert!(view.notice().unwrap().contains("already running"));
    }

    // ---- the overlays -----------------------------------------------------------------

    #[test]
    fn escape_walks_back_out_one_rung_at_a_time_and_never_quits() {
        let mut view = view();
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Mark);
        filter(&mut view, "node_modules");
        view.apply(Action::Help);
        view.apply(Action::OpenFilter);

        assert_eq!(view.apply(Action::Back), Effect::None);
        assert_eq!(view.overlay(), Some(Overlay::Help));
        view.apply(Action::Back);
        assert_eq!(view.overlay(), None);
        view.apply(Action::Back);
        assert_eq!(view.filter(), None);
        view.apply(Action::Back);
        assert_eq!(view.marked().claims, 0);
        // The bottom rung does nothing at all. `q` is the way out, and it is on the help page.
        assert_eq!(view.apply(Action::Back), Effect::None);
    }

    // ---- the map pane, and the two reasons there is not one ----------------------------

    #[test]
    fn m_on_a_terminal_that_reports_no_pixel_size_says_that_rather_than_blaming_the_protocol() {
        // #656. Both refusals were one boolean, so the only sentence the key had was the
        // protocol one — which is the wrong sentence inside tmux, where the terminal outside
        // reads the protocol perfectly well and the thing in the way is the multiplexer.
        let mut multiplexed = view();
        multiplexed.allow_maps(Maps::Unmeasured);
        multiplexed.apply(Action::ToggleMap);
        let said = multiplexed.notice().unwrap();
        assert!(said.contains("pixel size"), "{said}");
        assert!(
            !multiplexed.maps(),
            "a map was turned on that cannot be drawn"
        );

        // And the other terminal still gets the other sentence.
        let mut plain = view();
        plain.allow_maps(Maps::Unread);
        plain.apply(Action::ToggleMap);
        assert_eq!(
            plain.notice(),
            Some("this terminal does not read the graphics protocol, so there is no map")
        );
    }

    #[test]
    fn a_window_that_loses_its_pixel_size_gives_the_columns_back_and_says_why() {
        // A tmux client attaching to a session mid-run, or a window moving to a display the
        // terminal measures differently. The answer is not a start-up constant, so the pane
        // has to be able to go — and going without a word is the empty rectangle again, one
        // frame later.
        let mut view = view();
        view.allow_maps(Maps::Can);
        assert!(view.maps());

        view.allow_maps(Maps::Unmeasured);
        assert!(!view.maps(), "the tree is still paying for the pane");
        assert!(view.notice().unwrap().contains("pixel size"));

        // …and it comes back on its own when the window can be measured again, without the
        // reader having to press anything: `m` is theirs, and this is not.
        view.apply(Action::Back);
        view.allow_maps(Maps::Can);
        assert!(view.maps());
        assert_eq!(
            view.notice(),
            None,
            "it announced a map that is simply back"
        );
    }

    #[test]
    fn being_told_the_same_answer_again_is_not_news() {
        // Told ten times a second, so saying it twice is a footer that says nothing else for
        // the rest of the run — and marking the view stale each time is the whole tree's
        // stamps re-folded to learn that nothing changed.
        let mut view = view();
        view.allow_maps(Maps::Unmeasured);
        view.apply(Action::Back);
        assert_eq!(view.notice(), None);

        for _ in 0..10 {
            view.allow_maps(Maps::Unmeasured);
        }
        assert_eq!(view.notice(), None, "it said it again");
    }

    #[test]
    fn a_terminal_that_never_could_draw_one_says_nothing_at_start_up() {
        // The edge and not the state: a run that opens inside tmux has lost nothing, and a
        // footer that opens by naming a feature the reader never asked about is noise.
        let mut view = view();
        view.allow_maps(Maps::Unmeasured);
        assert_eq!(view.notice(), None);
    }

    // ---- what the footer says, and how it stops saying it ------------------------------

    #[test]
    fn a_report_of_what_was_removed_can_be_got_rid_of() {
        let mut view = view();
        view.deleted(Notice::passing("removed 10 B from 1 directory"), 10);
        assert_eq!(view.notice(), Some("removed 10 B from 1 directory"));

        // The bug this rung exists for: without it the sentence sits over the keys for the
        // rest of the run, and there is no key that takes it away.
        view.apply(Action::Back);
        assert_eq!(view.notice(), None);
    }

    #[test]
    fn the_next_thing_the_reader_does_takes_an_ordinary_report_away() {
        let mut view = view();
        view.deleted(Notice::passing("removed 10 B from 1 directory"), 10);

        // Moving the cursor is the reader looking at the tree the report describes, by which
        // point the report describes the frame before. No timer: every lifetime here is an
        // action, so nothing can expire while somebody is reading it.
        view.apply(Action::Cursor(Motion::Down));
        assert_eq!(view.notice(), None);
    }

    #[test]
    fn a_key_nobody_bound_is_not_the_reader_having_read_the_report() {
        let mut view = view();
        view.deleted(Notice::passing("removed 10 B from 1 directory"), 10);
        view.apply(Action::Ignore);
        assert!(view.notice().is_some());
    }

    #[test]
    fn a_report_naming_a_refusal_outlives_the_keys_that_clear_an_ordinary_one() {
        let mut view = view();
        view.deleted(
            Notice::standing("removed 10 B from 1 directory, 1 directory failed"),
            10,
        );
        assert!(view.notice_stands());

        // The safety model's counts are what the run exits non-zero on, so this line is where
        // a reader learns of them. An arrow key pressed while reading it, or a sort they
        // reach for to go and look, must not be what takes it away.
        for action in [
            Action::Cursor(Motion::Down),
            Action::Expand,
            Action::CycleSort,
            Action::Mark,
        ] {
            view.apply(action);
            assert!(view.notice().is_some(), "{action:?} took it away");
        }

        // Asked for explicitly, it goes — a reader who has been given the chance to see it.
        view.apply(Action::Back);
        assert_eq!(view.notice(), None);
        assert!(!view.notice_stands());
    }

    #[test]
    fn a_newer_report_answers_the_keystroke_that_asked_for_it_even_over_a_standing_one() {
        let mut view = view();
        view.deleted(
            Notice::standing("removed 10 B from 1 directory, 1 failed"),
            10,
        );

        // Not an incidental keypress: `x` on nothing marked is the reader asking a question,
        // and the footer is where it is answered. There is only ever one footer, so the newer
        // sentence wins — the alternative is a key that visibly does nothing.
        assert_eq!(view.apply(Action::Commit), Effect::None);
        assert!(view.notice().unwrap().contains("nothing is marked"));
        assert!(!view.notice_stands());
    }

    #[test]
    fn dismissing_a_report_is_one_rung_and_does_not_also_drop_the_filter() {
        let mut view = view();
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Mark);
        filter(&mut view, "node_modules");
        view.deleted(Notice::passing("removed 10 B from 1 directory"), 10);

        // One `Esc`, one rung. The notice goes first because it is the cheapest rung to take
        // by mistake; dropping the marks instead would be the expensive one.
        view.apply(Action::Back);
        assert_eq!(view.notice(), None);
        assert!(view.filter().is_some());
        assert_ne!(view.marked().claims, 0);

        view.apply(Action::Back);
        assert_eq!(view.filter(), None);
        view.apply(Action::Back);
        assert_eq!(view.marked().claims, 0);
    }

    #[test]
    fn a_press_on_a_report_that_has_already_gone_does_not_take_the_filter_with_it() {
        let mut view = view();
        filter(&mut view, "node_modules");
        view.deleted(Notice::passing("removed 10 B from 1 directory"), 10);

        // A press is aimed at the frame the reader was looking at and acted on at the release,
        // so the report can go in between — a keystroke during a held button is all it takes.
        // `Back` would fall through to the rung below, and the rung below is their filter.
        view.apply(Action::Cursor(Motion::Down));
        assert_eq!(view.notice(), None);

        view.apply(Action::Dismiss);
        assert!(view.filter().is_some(), "the dismissal fell through");
    }

    #[test]
    fn an_overlay_is_dismissed_before_the_report_behind_it() {
        let mut view = view();
        view.deleted(
            Notice::standing("removed 10 B from 1 directory, 1 failed"),
            10,
        );
        view.apply(Action::Help);

        // The overlays are drawn over the footer, so they are what is in front of the reader
        // and the first `Esc` is theirs. The report is still underneath afterwards — which is
        // the point of a standing one: going to read the key list is not having read it.
        view.apply(Action::Back);
        assert_eq!(view.overlay(), None);
        assert!(view.notice().is_some(), "the help took the report with it");
        view.apply(Action::Back);
        assert_eq!(view.notice(), None);
    }

    #[test]
    fn the_clock_that_drives_everything_else_on_the_frame_does_not_reach_the_report() {
        let mut view = view();
        let start = Instant::now();
        view.deleted(
            Notice::standing("removed 10 B from 1 directory, 1 directory failed"),
            10,
        );

        // A minute of frames, which is what a reader who walked away comes back to. Every
        // other moving thing on screen has long since settled — the arrival wash, the dimmed
        // rows, the counter climbing — and this is the one that must not, because the tree it
        // reports on is gone and the exit status is the only other place these counts appear.
        for tick in 1..=600 {
            view.animate(start + Duration::from_millis(100) * tick);
        }
        assert!(view.notice().is_some(), "a clock took the report away");
        assert!(view.notice_stands());

        // And an ordinary report is no more perishable — what ends one is an action, so a
        // frame is not it. The difference between the two lifetimes is *which* actions count.
        view.apply(Action::Dismiss);
        view.deleted(Notice::passing("removed 10 B from 1 directory"), 20);
        for tick in 1..=600 {
            view.animate(start + Duration::from_millis(100) * tick);
        }
        assert!(view.notice().is_some(), "a clock took the report away");
    }

    #[test]
    fn the_viewport_follows_the_cursor_and_never_hangs_off_the_end_of_the_rows() {
        let mut tree = Tree::new("/scan");
        for n in 0..30 {
            tree.insert(hit(
                &format!("/scan/p{n:02}/node_modules"),
                Size::Measured(1),
                0,
            ));
        }
        let mut view = View::new(tree);
        view.viewport(10);
        assert_eq!(view.scroll(), 0);

        view.apply(Action::Cursor(Motion::Bottom));
        assert_eq!(view.cursor(), Some(30));
        assert_eq!(
            view.scroll(),
            21,
            "the cursor is off the bottom of the pane"
        );

        view.apply(Action::Cursor(Motion::PageUp));
        assert_eq!(view.cursor(), Some(20));
        assert_eq!(view.scroll(), 20);

        // A filter that hides everything leaves the viewport a long way down with nothing
        // under it. Left there, the pane draws as empty over a tree that is full.
        filter(&mut view, "matches nothing at all");
        assert_eq!(view.scroll(), 0);
    }

    // ---- what a pointer does ----------------------------------------------------------

    #[test]
    fn a_click_selects_the_directory_it_landed_on_and_not_the_position_it_was_at() {
        let mut view = view();
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Expand);
        let old = at(&view, "/scan/old");
        assert_eq!(
            shown(&view),
            ["/scan", "  nx", "    node_modules", "    packages", "  old"]
        );

        // A claim arrives and re-sorts the level: `old` is now row 1 where it was row 4. A
        // press taken as a *position* would select `nx`, whose subtree is everything the
        // reader was looking at.
        view.found(hit("/scan/old/big/node_modules", Size::Measured(9_000), 50));
        view.sync();
        assert_eq!(
            shown(&view),
            ["/scan", "  old", "  nx", "    node_modules", "    packages"]
        );

        view.apply(Action::Select(old));
        assert_eq!(
            view.tree().node(view.row().unwrap().id).path,
            PathBuf::from("/scan/old")
        );
    }

    #[test]
    fn a_click_on_a_row_that_is_gone_leaves_the_cursor_where_it_is() {
        let mut view = view();
        point_at(&mut view, "/scan/nx");
        let target = at(&view, "/scan/old/target");

        // The row the press aimed at has been deleted between the press and the release.
        // Doing nothing is the honest outcome; the alternative is acting on whatever is now
        // at that position.
        view.removed(Path::new("/scan/old/target"), 10, true);
        // Past the dimmed beat, which is what actually detaches the row now: a complete
        // removal empties it on this frame and takes it out of the tree a moment later.
        settle(&mut view);
        view.apply(Action::Select(target));

        assert_eq!(
            view.tree().node(view.row().unwrap().id).path,
            PathBuf::from("/scan/nx")
        );
    }

    #[test]
    fn a_click_on_the_indicator_opens_the_row_and_a_click_on_a_leafs_does_nothing_but_select() {
        let mut view = view();
        let nx = at(&view, "/scan/nx");
        view.apply(Action::OpenRow(nx));
        assert_eq!(
            shown(&view),
            ["/scan", "  nx", "    node_modules", "    packages", "  old"]
        );
        view.apply(Action::OpenRow(nx));
        assert_eq!(shown(&view), ["/scan", "  nx", "  old"]);

        // A leaf leaves the indicator's cell blank, so a press there cannot have been aimed
        // at one. It selects the row and stops.
        view.apply(Action::OpenRow(nx));
        let leaf = at(&view, "/scan/nx/node_modules");
        view.apply(Action::OpenRow(leaf));
        assert_eq!(
            view.tree().node(view.row().unwrap().id).path,
            PathBuf::from("/scan/nx/node_modules")
        );
        assert_eq!(
            shown(&view),
            ["/scan", "  nx", "    node_modules", "    packages", "  old"]
        );
    }

    #[test]
    fn a_click_on_the_box_marks_exactly_what_the_key_marks() {
        let mut view = view();
        let nx = at(&view, "/scan/nx");
        view.apply(Action::MarkRow(nx));

        // One door, not two: the box under the pointer and `space` reach the same code, so a
        // mark cannot mean one thing pressed and another typed.
        assert_eq!(view.mark_of(nx), Mark::All);
        assert_eq!(view.marked().claims, 2);
        assert_eq!(
            view.tree().node(view.row().unwrap().id).path,
            PathBuf::from("/scan/nx")
        );

        view.apply(Action::MarkRow(nx));
        assert_eq!(view.marked().claims, 0);
    }

    #[test]
    fn naming_an_order_twice_turns_it_upside_down_and_a_new_column_starts_the_right_way_up() {
        let mut view = view();
        assert_eq!(view.sort(), Sort::by(Order::Size));

        view.apply(Action::SortBy(Order::Path));
        assert_eq!(view.sort(), Sort::by(Order::Path));
        view.apply(Action::SortBy(Order::Path));
        assert_eq!(
            view.sort(),
            Sort {
                by: Order::Path,
                reverse: true
            }
        );

        // A new column starts in its own natural order rather than inheriting the reversal.
        // Carrying it across would reverse something the reader never asked to reverse.
        view.apply(Action::SortBy(Order::Size));
        assert_eq!(view.sort(), Sort::by(Order::Size));
    }

    #[test]
    fn the_wheel_moves_the_viewport_and_takes_the_cursor_with_it() {
        let mut tree = Tree::new("/scan");
        for n in 0..30 {
            tree.insert(hit(
                &format!("/scan/p{n:02}/node_modules"),
                Size::Measured(1),
                0,
            ));
        }
        let mut view = View::new(tree);
        view.viewport(10);
        view.apply(Action::Cursor(Motion::Top));
        assert_eq!((view.scroll(), view.cursor()), (0, Some(0)));

        view.apply(Action::ScrollRows(Motion::Down));
        // Three rows a notch, and the cursor is pushed to the top of what is now drawn: a
        // cursor left off the screen is a `space` aimed at a row nobody can see.
        assert_eq!((view.scroll(), view.cursor()), (3, Some(3)));

        view.apply(Action::ScrollRows(Motion::Up));
        assert_eq!(view.scroll(), 0);
        // Coming back up leaves the cursor where it was — it is inside the pane again, and
        // scrolling is not choosing.
        assert_eq!(view.cursor(), Some(3));

        // …and the wheel cannot scroll the last row off into an empty pane.
        for _ in 0..40 {
            view.apply(Action::ScrollRows(Motion::Down));
        }
        assert_eq!(view.scroll(), view.rows().len() - 10);
    }

    #[test]
    fn a_wheel_over_a_view_with_no_cursor_does_not_hand_it_one() {
        let mut view = view();
        filter(&mut view, "nothing matches this");
        view.apply(Action::Back);
        assert_eq!(view.cursor(), None);

        view.apply(Action::ScrollRows(Motion::Down));
        assert_eq!(view.cursor(), None, "scrolling chose a row");
    }

    #[test]
    fn a_double_click_asks_for_a_price_on_what_is_under_the_row_and_nothing_else() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/nx/node_modules", Size::Unmeasured, 900));
        tree.insert(hit(
            "/scan/nx/packages/ui/node_modules",
            Size::Measured(5),
            800,
        ));
        tree.insert(hit("/scan/old/target", Size::Unmeasured, 100));
        let mut view = View::new(tree);
        view.viewport(40);

        let nx = at(&view, "/scan/nx");
        let effect = view.apply(Action::Price(nx));

        // Only what carries no price, and only what is under the row that was pressed —
        // `old/target` is unpriced too and was not aimed at.
        assert_eq!(
            effect,
            Effect::Price(vec![PathBuf::from("/scan/nx/node_modules")])
        );
        assert!(view.notice().unwrap().contains("pricing 1 directory"));

        // A subtree that is already priced says so rather than starting work with no result.
        // Opened on the way, because a press can only land on a row that is drawn — which is
        // also why an off-screen row starts nothing at all.
        point_at(&mut view, "/scan/nx/packages");
        let packages = at(&view, "/scan/nx/packages");
        assert_eq!(view.apply(Action::Price(packages)), Effect::None);
        assert!(
            view.notice().unwrap().contains("already carries a price"),
            "{:?}",
            view.notice()
        );
    }

    #[test]
    fn a_double_click_on_a_row_that_has_gone_prices_nothing() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/old/target", Size::Unmeasured, 100));
        let mut view = View::new(tree);
        view.viewport(40);
        point_at(&mut view, "/scan/old");
        view.apply(Action::Expand);
        let target = at(&view, "/scan/old/target");

        // The row was pressed and then deleted before the button came up. A detached node
        // keeps its hit, so "walk what is under this id" would happily hand back a path that
        // is no longer on screen and no longer on disk — which is the identity rule going
        // one way for the cursor and the other way for the work.
        view.removed(Path::new("/scan/old/target"), 0, true);
        settle(&mut view);

        assert_eq!(view.apply(Action::Price(target)), Effect::None);
    }

    #[test]
    fn a_subtree_already_being_priced_is_not_asked_for_a_second_time() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/nx/node_modules", Size::Unmeasured, 900));
        let mut view = View::new(tree);
        view.viewport(40);
        let nx = at(&view, "/scan/nx");
        let claim = PathBuf::from("/scan/nx/node_modules");

        assert_eq!(
            view.apply(Action::Price(nx)),
            Effect::Price(vec![claim.clone()])
        );

        // Leaning on the button during a traversal of a real `node_modules` would otherwise
        // queue the same traversal again and again. `Tree::price` rejecting the duplicate
        // *result* is no help: by then the expensive part has already happened.
        assert_eq!(view.apply(Action::Price(nx)), Effect::None);
        assert!(
            view.notice().unwrap().contains("already being priced"),
            "{:?}",
            view.notice()
        );

        // …and the two facts stay apart once the pass reports: nothing left to ask for
        // because it has a price now, rather than because somebody is still working on it.
        view.priced(&claim, Size::Measured(64));
        view.repriced(&[claim], Notice::passing("priced 1 directory"));
        assert_eq!(view.apply(Action::Price(nx)), Effect::None);
        assert!(
            view.notice().unwrap().contains("already carries a price"),
            "{:?}",
            view.notice()
        );
    }

    #[test]
    fn a_pricing_pass_that_never_reports_does_not_strand_its_rows() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/nx/node_modules", Size::Unmeasured, 900));
        let mut view = View::new(tree);
        view.viewport(40);
        let nx = at(&view, "/scan/nx");
        let claim = PathBuf::from("/scan/nx/node_modules");
        view.apply(Action::Price(nx));

        // Handing the claims back is what the loop does when the worker has gone. Without
        // it the in-flight set leaks and the subtree can never be asked about again for the
        // rest of the run — a quiet, permanent no-op on a gesture the reader keeps making.
        view.repriced(
            std::slice::from_ref(&claim),
            Notice::passing("the pricing went away"),
        );
        assert_eq!(view.notice(), Some("the pricing went away"));
        assert_eq!(view.apply(Action::Price(nx)), Effect::Price(vec![claim]));
    }

    #[test]
    fn a_double_click_never_prices_what_the_filter_is_hiding() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/nx/node_modules", Size::Unmeasured, 900));
        tree.insert(hit(
            "/scan/nx/packages/ui/node_modules",
            Size::Unmeasured,
            800,
        ));
        let mut view = View::new(tree);
        view.viewport(40);
        filter(&mut view, "ui/node_modules");

        // The filter's own safety rule, kept: a row acts on what its number describes. A
        // price landing on a hidden claim would move a total the reader cannot see.
        let nx = at(&view, "/scan/nx");
        assert_eq!(
            view.apply(Action::Price(nx)),
            Effect::Price(vec![PathBuf::from("/scan/nx/packages/ui/node_modules")])
        );
    }

    #[test]
    fn the_footer_stops_saying_a_price_is_being_worked_out_once_it_is() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/nx/node_modules", Size::Unmeasured, 900));
        let mut view = View::new(tree);
        let nx = at(&view, "/scan/nx");
        view.apply(Action::Price(nx));
        assert!(view.notice().unwrap().contains("pricing"));

        view.repriced(
            &[PathBuf::from("/scan/nx/node_modules")],
            Notice::passing("priced 1 directory"),
        );
        assert_eq!(view.notice(), Some("priced 1 directory"));
    }

    #[test]
    fn quitting_is_the_only_thing_that_ends_the_view() {
        let mut view = view();
        assert_eq!(view.apply(Action::Quit), Effect::Quit);
    }

    #[test]
    fn quitting_cannot_end_a_view_that_is_half_way_through_a_removal() {
        let mut view = view();
        view.asking(&planned(&["/scan/old/target"]), &[]);
        view.apply(Action::Highlight(Turn::Next));
        view.apply(Action::Answer);
        assert!(view.is_deleting());

        // Leaving would take the pool with it, and what is left on disk would be neither the
        // tree the reader had nor the one they asked for — with nothing to report which.
        assert_eq!(view.apply(Action::Quit), Effect::None);
        assert!(!view.wants_to_quit());
        assert!(view.notice().unwrap().contains("has to finish"));

        // Pressing it again does not wear the rule down either.
        assert_eq!(view.apply(Action::Quit), Effect::None);
        assert!(!view.wants_to_quit());

        // …and the keystroke is remembered rather than dropped: the loop leaves on the first
        // frame after the removal reports.
        view.deleted(Notice::passing("removed 10 B from 1 directory"), 10);
        assert!(view.wants_to_quit());
    }

    #[test]
    fn a_view_that_was_never_asked_to_quit_does_not_want_to() {
        let mut view = view();
        assert!(!view.wants_to_quit());
        view.asking(&planned(&["/scan/old/target"]), &[]);
        view.apply(Action::Highlight(Turn::Next));
        view.apply(Action::Answer);
        view.deleted(Notice::passing("removed 10 B from 1 directory"), 10);
        assert!(!view.wants_to_quit());
    }

    // ---- what moves, and what it is saying ---------------------------------------------

    #[test]
    fn a_rolled_up_total_climbs_toward_what_arrived_rather_than_snapping_to_it() {
        let mut view = view();
        let start = Instant::now();
        view.animate(start);
        assert_eq!(view.drawn_total().bytes, 310);

        view.found(hit("/scan/big/node_modules", Size::Measured(690), 1));
        view.animate(start + COUNT_UP / 2);

        // The tree knows the answer immediately; the screen takes a moment to say it, and
        // that moment is the information — a number climbing fast is a scan finding fast,
        // which the count of directories beside it cannot express.
        assert_eq!(view.total().bytes, 1000);
        let climbing = view.drawn_total().bytes;
        assert!(climbing > 310 && climbing < 1000, "{climbing}");
        assert!(view.is_moving());

        view.animate(start + COUNT_UP * 8);
        assert_eq!(view.drawn_total().bytes, 1000);
        assert!(
            !view.is_moving(),
            "a settled view is still asking for frames"
        );
    }

    #[test]
    fn a_row_the_walk_has_just_found_is_lit_and_the_light_goes_out() {
        let mut view = view();
        let start = Instant::now();
        view.animate(start);
        // Nothing here was found while anybody was watching, so nothing is lit. A view that
        // opened onto a tree it already had would otherwise flash all of it at once.
        assert!(view.freshness(at(&view, "/scan/nx")).abs() < f64::EPSILON);

        view.found(hit("/scan/late/node_modules", Size::Measured(1), 1));
        view.animate(start);

        let late = at(&view, "/scan/late");
        assert!(view.freshness(late) > 0.9, "{}", view.freshness(late));
        assert!(view.freshness(at(&view, "/scan/nx")).abs() < f64::EPSILON);

        view.animate(start + ARRIVAL * 2);
        assert!(view.freshness(late).abs() < f64::EPSILON);
    }

    #[test]
    fn an_ancestor_whose_children_are_still_being_priced_says_its_number_is_a_floor() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/nx/a/node_modules", Size::Measured(4200), 1));
        tree.insert(hit("/scan/nx/b/node_modules", Size::Unmeasured, 1));
        let mut view = View::new(tree);
        let nx = at(&view, "/scan/nx");

        // Not `4.1 KiB`, which would be wrong in the one direction a cleaner must not be
        // wrong in, and not a dash, which throws away a number that is already known.
        assert_eq!(view.roll(nx).label(), "> 4.1 KiB");

        view.priced(Path::new("/scan/nx/b/node_modules"), Size::Measured(700));
        view.sync();
        assert_eq!(view.roll(nx).label(), "4.8 KiB");
    }

    #[test]
    fn only_the_claims_a_pricing_thread_is_inside_are_hot() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/a/node_modules", Size::Unmeasured, 1));
        tree.insert(hit("/scan/b/node_modules", Size::Unmeasured, 1));
        let mut view = View::new(tree);
        let a = at(&view, "/scan/a/node_modules");
        let b = at(&view, "/scan/b/node_modules");
        assert!(!view.is_pricing(a) && !view.is_pricing(b));

        view.pricing(Path::new("/scan/a/node_modules"));

        // The whole claim the effect makes, and the reason it is worth an event of its own:
        // the pool is bounded, so as many rows are hot as there are threads working. `b` is
        // queued, which is a different fact about a dash and used to be indistinguishable.
        assert!(view.is_pricing(a));
        assert!(!view.is_pricing(b));

        view.priced(Path::new("/scan/a/node_modules"), Size::Measured(64));
        assert!(!view.is_pricing(a));
    }

    #[test]
    fn a_walk_that_has_finished_leaves_nothing_shimmering_for_a_thread_that_is_gone() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/a/node_modules", Size::Unmeasured, 1));
        let mut view = View::new(tree);
        let a = at(&view, "/scan/a/node_modules");
        view.pricing(Path::new("/scan/a/node_modules"));

        view.scanned();

        // A pool that has stopped can leave a claim hot if it died on the way. A row moving
        // for a thread that no longer exists is the one thing here that would say nothing.
        assert!(!view.is_pricing(a));
        assert!(!view.is_moving());
    }

    #[test]
    fn a_claim_deleted_while_it_was_being_priced_stops_shimmering() {
        let mut view = view();
        let start = Instant::now();
        view.animate(start);
        view.pricing(Path::new("/scan/old/target"));
        assert!(view.is_moving());

        // The price for this claim will never arrive, because `priced` resolves a path and
        // the path is gone. Nothing else would ever cool it: it would shimmer for a thread
        // that finished long ago, and hold the whole view at the animating frame rate to do
        // it — a quiet, permanent cost on a row nobody can see.
        view.removed(Path::new("/scan/old/target"), 10, true);
        view.animate(start + DIM);

        assert!(view.tree().find(Path::new("/scan/old/target")).is_none());
        assert!(!view.is_moving());
    }

    #[test]
    fn marking_a_row_runs_the_mark_up_its_ancestors_rather_than_flashing_all_of_them() {
        let mut view = view();
        let start = Instant::now();
        view.animate(start);
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Mark);
        view.animate(start);

        // The signature interaction, and the one whose effect is otherwise entirely off
        // screen: `nx` is collapsed, so everything the mark took is out of sight and the only
        // visible consequence is on ancestors the reader is not looking at.
        let root = view.tree().root();
        assert!(view.is_cascading(at(&view, "/scan/nx")));
        assert!(!view.is_cascading(root), "the whole chain flashed at once");

        view.animate(start + RUNG);
        assert!(view.is_cascading(root), "the mark never reached the root");

        view.animate(start + RUNG + FLASH);
        assert!(!view.is_cascading(root));
        assert!(!view.is_moving());
    }

    #[test]
    fn a_partial_ancestor_says_what_share_of_its_bytes_is_marked() {
        let mut view = view();
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Expand);
        point_at(&mut view, "/scan/nx/node_modules");
        view.apply(Action::Mark);

        // 200 of `nx`'s 300 bytes, and 200 of the root's 310. A bare partial marker says
        // "some of this"; the share is what tells a reader whether opening the row is worth
        // the keystroke.
        assert!((view.share(at(&view, "/scan/nx")) - 200.0 / 300.0).abs() < 1e-9);
        assert!((view.share(view.tree().root()) - 200.0 / 310.0).abs() < 1e-9);
        assert!(view.share(at(&view, "/scan/old")).abs() < f64::EPSILON);
        assert!((view.share(at(&view, "/scan/nx/node_modules")) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_share_that_cannot_be_stated_in_bytes_is_stated_in_claims() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/a/node_modules", Size::Measured(1000), 1));
        tree.insert(hit("/scan/b/node_modules", Size::Unmeasured, 1));
        let mut view = View::new(tree);
        point_at(&mut view, "/scan/a");
        view.apply(Action::Mark);

        // By bytes this is 100% marked, which would read as "all but a sliver of this is
        // spoken for" — for a subtree whose one marked claim is the only one anybody has
        // measured. Claims are always known, so they are what the glyph reports until the
        // bytes can be trusted.
        assert!((view.share(view.tree().root()) - 0.5).abs() < 1e-9);

        view.priced(Path::new("/scan/b/node_modules"), Size::Measured(1000));
        view.sync();
        assert!((view.share(view.tree().root()) - 0.5).abs() < 1e-9);
    }

    // ---- deletion, restrained ---------------------------------------------------------

    #[test]
    fn a_row_empties_on_the_bytes_the_deleter_says_have_gone() {
        let mut view = view();
        let start = Instant::now();
        view.animate(start);
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Expand);
        let claim = at(&view, "/scan/nx/node_modules");

        // Half of the 200-byte target is off the disk. Nothing about this is a timer: the
        // row is worth exactly what is left of it, and the next report decides the next
        // frame — so a target that takes ten seconds empties over ten seconds and one that
        // takes ten milliseconds does not pretend otherwise.
        view.freeing(Path::new("/scan/nx/node_modules"), 100);
        view.animate(start);
        assert!(view.is_freeing(claim));
        assert!(!view.is_spent(claim), "dimmed while it is still emptying");
        assert_eq!(view.drawn(claim).bytes, 100);
        // Its ancestors are lighter by the same bytes, on the same event.
        assert_eq!(view.drawn(at(&view, "/scan/nx")).bytes, 200);

        view.freeing(Path::new("/scan/nx/node_modules"), 180);
        view.animate(start + Duration::from_millis(10));
        assert_eq!(view.drawn(claim).bytes, 20);

        // The sweep finishes. Only now is the row dim, and only after the beat does it go.
        view.removed(Path::new("/scan/nx/node_modules"), 200, true);
        view.animate(start + Duration::from_millis(20));
        assert!(view.is_spent(claim));
        assert!(!view.is_freeing(claim));
        assert_eq!(view.drawn(claim).bytes, 0);
        assert!(
            view.tree()
                .find(Path::new("/scan/nx/node_modules"))
                .is_some()
        );

        view.animate(start + Duration::from_millis(20) + DIM);
        assert!(
            view.tree()
                .find(Path::new("/scan/nx/node_modules"))
                .is_none()
        );
        assert_eq!(view.drawn(at(&view, "/scan/nx")).bytes, 100);
    }

    #[test]
    fn a_row_the_sweep_could_not_finish_keeps_what_is_left_of_it() {
        let mut view = view();
        let start = Instant::now();
        view.animate(start);
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Expand);
        let claim = at(&view, "/scan/nx/node_modules");

        // A checkout inside it, an unreadable corner: the sweep went in, freed some of it and
        // came out again. The directory is still there, so the row stays — worth what is left
        // rather than what it was, which is the only figure that is true of the disk.
        view.removed(Path::new("/scan/nx/node_modules"), 150, false);
        view.animate(start + DIM * 2);

        assert!(
            view.tree()
                .find(Path::new("/scan/nx/node_modules"))
                .is_some()
        );
        assert!(!view.is_spent(claim), "a row that survived was collapsed");
        assert_eq!(view.drawn(claim).bytes, 50);
    }

    #[test]
    fn a_running_removal_counts_targets_against_the_batch_it_was_given() {
        let mut view = view();
        view.asking(
            &priced(&[
                ("/scan/nx/node_modules", 200),
                ("/scan/nx/packages/ui/node_modules", 100),
                ("/scan/old/target", 10),
            ]),
            &[],
        );
        view.apply(Action::Highlight(Turn::Next));
        view.apply(Action::Answer);

        // The denominator is fixed here, by the question the reader answered — which is what
        // separates this bar from the pricing one, whose total grows as the walk finds claims.
        assert_eq!(view.removing().unwrap().counted(), (0, 3));
        assert_eq!(view.removing().unwrap().percent(), 0);
        // The batch's weight comes from the tree, so the denominator is there from the first
        // frame — which is the whole point of it. `2162 of 2188` cannot say whether the rest is
        // a second or an hour, and `0 B of 310 B` can.
        assert_eq!(view.removing().unwrap().weighed(), Some((0, 310)));
        assert_eq!(
            view.removing().unwrap().label(),
            "removing 0 of 3 directories · 0% · 0 B of 310 B"
        );

        // The count moves on the deleter leaving a target, never on what it did there — the
        // row work is `removed`'s and the position is this. A removal reports both, in that
        // order, and only the second one advances the bar.
        view.removed(Path::new("/scan/nx/node_modules"), 200, true);
        assert_eq!(
            view.removing().unwrap().counted(),
            (0, 3),
            "the position moved on what happened to a row"
        );
        // The bytes are not the position and do move here: this target has given back what it
        // was worth, whatever the count says about where the pool is.
        assert_eq!(view.removing().unwrap().weighed(), Some((200, 310)));
        view.swept(Path::new("/scan/nx/node_modules"));
        assert_eq!(view.removing().unwrap().counted(), (1, 3));

        // A target the sweep went into and came back out of counts the same: it is not a claim
        // that the target was removed — the row is still there saying what is left of it. Its
        // bytes count for what actually went, which is less than the plan expected of it.
        view.removed(Path::new("/scan/nx/packages/ui/node_modules"), 40, false);
        view.swept(Path::new("/scan/nx/packages/ui/node_modules"));
        assert_eq!(view.removing().unwrap().counted(), (2, 3));
        assert_eq!(view.removing().unwrap().percent(), 66);
        assert_eq!(view.removing().unwrap().weighed(), Some((240, 310)));

        // The third target turns out to be gone already, so nothing happened to it and no row
        // moves — but the deleter still worked through it and said so, so the count reaches
        // its total rather than stopping one short for the rest of the run.
        view.swept(Path::new("/scan/old/target"));
        assert_eq!(view.removing().unwrap().counted(), (3, 3));
        assert_eq!(view.removing().unwrap().percent(), 100);
        // …and the bytes stop short, because they describe the outcome and it fell short. The
        // two figures disagreeing is them answering different questions, not a fault.
        assert_eq!(view.removing().unwrap().weighed(), Some((240, 310)));

        view.deleted(Notice::passing("removed 240 B from 1 directory"), 240);
        assert!(view.removing().is_none());
        assert!(!view.is_deleting());
    }

    #[test]
    fn the_footer_names_the_target_the_batch_is_waiting_on() {
        let mut view = view();
        view.asking(
            &priced(&[
                ("/scan/nx/node_modules", 200),
                ("/scan/nx/packages/ui/node_modules", 100),
                ("/scan/old/target", 10),
            ]),
            &[],
        );
        view.apply(Action::Highlight(Turn::Next));
        view.apply(Action::Answer);

        // Nothing has started, so there is nothing to name. A footer that guessed here would be
        // naming a target the pool may not have reached.
        assert_eq!(view.removing().unwrap().busiest(), None);

        // Two in flight at once, which is the normal state of a batch: the pool runs as many
        // targets as it has threads. The one worth naming is the larger — a target is swept by
        // a single thread, so it is the one that decides when the batch ends.
        view.freeing(Path::new("/scan/old/target"), 4);
        assert_eq!(
            view.removing().unwrap().busiest(),
            Some(Path::new("/scan/old/target"))
        );
        view.freeing(Path::new("/scan/nx/node_modules"), 8);
        assert_eq!(
            view.removing().unwrap().busiest(),
            Some(Path::new("/scan/nx/node_modules")),
            "the smaller target was named while a larger one was still going"
        );

        // Weighed by what the plan expected rather than by what has gone, so the name does not
        // hand over the moment a big target gets ahead on bytes. `old/target` is worth 10 and
        // `nx/node_modules` 200, and the second stays named while it is still running.
        view.freeing(Path::new("/scan/old/target"), 10);
        assert_eq!(
            view.removing().unwrap().busiest(),
            Some(Path::new("/scan/nx/node_modules"))
        );

        // The pool moves off it, and the name hands over to the largest still going rather
        // than sticking on a target that is finished.
        view.swept(Path::new("/scan/nx/node_modules"));
        assert_eq!(
            view.removing().unwrap().busiest(),
            Some(Path::new("/scan/old/target"))
        );

        // And when the last one is done there is nobody left to be waiting on.
        view.swept(Path::new("/scan/old/target"));
        assert_eq!(view.removing().unwrap().busiest(), None);
    }

    #[test]
    fn an_unpriced_batch_gives_no_byte_figure_rather_than_a_misleading_one() {
        // A default scan prices a fraction of what it finds, so a batch can be entirely
        // unpriced. The count still works — it never needed a size — and the byte pair is
        // withheld outright. Drawing `0 B of 0 B`, or a total that is quietly a fraction of the
        // truth, would be worse than saying nothing: a reader would read it as "nearly done".
        let mut view = View::new(Tree::new("/scan"));
        view.viewport(10);
        view.found(hit("/scan/app/node_modules", Size::Unmeasured, 0));
        view.asking(
            &[Planned::at("/scan/app/node_modules", Size::Unmeasured)],
            &[],
        );
        view.apply(Action::Highlight(Turn::Next));
        view.apply(Action::Answer);

        assert_eq!(view.removing().unwrap().weighed(), None);
        assert_eq!(
            view.removing().unwrap().label(),
            "removing 0 of 1 directory · 0%"
        );

        // The name still works, because what has been freed is the fallback ordering when
        // nothing priced the batch.
        view.freeing(Path::new("/scan/app/node_modules"), 512);
        assert_eq!(
            view.removing().unwrap().busiest(),
            Some(Path::new("/scan/app/node_modules"))
        );
    }

    #[test]
    fn a_batch_that_fails_on_everything_still_shows_the_deleter_working_through_it() {
        let mut view = view();
        view.asking(
            &priced(&[
                ("/scan/nx/node_modules", 200),
                ("/scan/nx/packages/ui/node_modules", 100),
                ("/scan/old/target", 10),
            ]),
            &[],
        );
        view.apply(Action::Highlight(Turn::Next));
        view.apply(Action::Answer);

        // Every target fails before unlinking a single entry, so not one of them is a removal
        // and not one row moves. The deleter is working through them all the same, and a bar
        // that read 0% for the whole run and then vanished would be reporting the OUTCOME
        // while claiming to report the position.
        for (done, path) in [
            "/scan/nx/node_modules",
            "/scan/nx/packages/ui/node_modules",
            "/scan/old/target",
        ]
        .iter()
        .enumerate()
        {
            view.swept(Path::new(path));
            assert_eq!(view.removing().unwrap().counted(), (done + 1, 3));
        }
        assert_eq!(view.removing().unwrap().percent(), 100);
        // The position reaches its total and the bytes stay at nothing, which is the pair
        // saying exactly what happened: the deleter went everywhere it was sent and came back
        // with nothing. A single bar weighted by bytes would have read 0% throughout and then
        // vanished, and one weighted by targets alone could not tell this from a batch that
        // freed 300 GiB.
        assert_eq!(view.removing().unwrap().weighed(), Some((0, 310)));
        assert_eq!(
            view.removing().unwrap().label(),
            "removing 3 of 3 directories · 100% · 0 B of 310 B"
        );

        // …and nothing was deleted, which is the other half of the same claim: the position
        // says where the deleter got to and never that anything went.
        assert_eq!(view.roll(view.tree().root()).bytes, 310);
        assert_eq!(view.roll(view.tree().root()).claims, 3);
    }

    #[test]
    fn a_second_removal_is_refused_while_one_is_running() {
        let mut view = view();
        view.asking(&planned(&["/scan/old/target"]), &[]);
        view.apply(Action::Highlight(Turn::Next));
        view.apply(Action::Answer);

        point_at(&mut view, "/scan/nx");
        view.apply(Action::Mark);
        assert_eq!(view.apply(Action::Commit), Effect::None);
        assert_eq!(view.notice(), Some("a removal is already running"));
    }

    #[test]
    fn a_part_emptied_row_does_not_spring_back_when_the_batch_reports() {
        let mut view = view();
        let start = Instant::now();
        view.animate(start);
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Expand);
        let claim = at(&view, "/scan/nx/node_modules");
        let root = view.tree().root();

        // 150 of the target's 200 bytes go, and then the sweep comes out again — a checkout
        // inside it, an unreadable corner. Both events, because that is the order the deleter
        // reports in and the reduction has to survive either one arriving last.
        view.freeing(Path::new("/scan/nx/node_modules"), 150);
        view.removed(Path::new("/scan/nx/node_modules"), 150, false);
        view.animate(start);
        assert_eq!(view.drawn(claim).bytes, 50);
        assert_eq!(view.drawn_total().bytes, 160);

        // The batch reports. Its per-target figures are dropped for the report's own
        // arithmetic — and until this was fixed, the *reduction* went with them: the tree still
        // held 200 for a directory that has 50 left, so the row and the headline both jumped
        // back to what they were worth before a single byte was deleted. That is the direction
        // a cleaner may never be wrong in, because the number that rose is the one a reader
        // came back to check.
        // Standing, because this is the shape `summarise` gives a batch that left something
        // behind: the sweep came out of this target without finishing it.
        view.deleted(Notice::standing("freed 150 B"), 150);
        view.animate(start + DIM * 2);

        assert_eq!(view.drawn(claim).bytes, 50, "the row sprang back");
        assert_eq!(view.drawn_total().bytes, 160, "the headline rose again");
        // Durably, not just on this frame: the tree itself is what a later sort, filter or
        // second batch reads, and it has to agree with the screen.
        assert_eq!(view.roll(claim).bytes, 50);
        assert_eq!(view.roll(root).bytes, 160);
        // And the freed figure is untouched — the bytes are counted once, by the batch.
        assert_eq!(view.drawn_freed(), 150);
    }

    #[test]
    fn a_row_the_deleter_has_touched_is_out_of_the_batch_and_out_of_the_counter() {
        let mut view = view();
        let start = Instant::now();
        view.animate(start);
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Mark);
        assert_eq!(view.marked().claims, 2);

        // Part way through, not finished: the bytes are gone but the directory is not, so the
        // counter loses the bytes and keeps the count.
        view.freeing(Path::new("/scan/nx/node_modules"), 200);
        view.animate(start);
        assert_eq!(view.marked().claims, 2);
        assert_eq!(view.marked().bytes, 100);
        // It is out of the batch from the moment the sweep first touched it, though. Offering
        // a directory that is being deleted to a second removal would report a failure for
        // the one thing that worked.
        assert_eq!(
            batched(&view),
            [PathBuf::from("/scan/nx/packages/ui/node_modules")]
        );

        // And the claim stops counting when the sweep says it has finished with it.
        view.removed(Path::new("/scan/nx/node_modules"), 200, true);
        view.animate(start);
        assert_eq!(view.marked().claims, 1);
        assert_eq!(view.marked().bytes, 100);
    }

    #[test]
    fn a_row_the_deleter_has_touched_cannot_be_marked() {
        let mut view = view();
        let start = Instant::now();
        view.animate(start);
        point_at(&mut view, "/scan/old");
        view.apply(Action::Expand);
        point_at(&mut view, "/scan/old/target");

        view.freeing(Path::new("/scan/old/target"), 5);
        view.animate(start);
        view.apply(Action::Mark);

        // The cursor is still on it, because it is still on screen saying what is happening to
        // it. A mark aimed at a directory that is being deleted is a keystroke with nowhere
        // to land.
        assert_eq!(view.marked().claims, 0);
        assert!(view.batch().is_empty());
    }

    #[test]
    fn the_freed_counter_climbs_on_the_same_bytes_the_reclaimable_one_loses() {
        // Its own tree, with sizes a disk would actually have: a chase snaps once it is
        // within a byte of its target, so a ten-byte counter arrives before it has moved.
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/nx/node_modules", Size::Measured(300_000), 900));
        tree.insert(hit("/scan/old/target", Size::Measured(100_000), 100));
        let mut view = View::new(tree);
        view.viewport(40);
        let start = Instant::now();
        view.animate(start);
        assert!(!view.has_freed());
        assert_eq!(view.drawn_total().bytes, 400_000);

        // One event, both counters. They are not two accounts of a deletion that have to be
        // kept in step — they are one number read from each end, which is why they cannot
        // drift apart or lag one another.
        view.freeing(Path::new("/scan/old/target"), 40_000);
        view.animate(start + COUNT_UP * 8);
        assert!(view.has_freed());
        assert_eq!(view.drawn_total().bytes, 360_000);
        assert_eq!(view.drawn_freed(), 40_000);

        view.removed(Path::new("/scan/old/target"), 100_000, true);
        view.animate(start + COUNT_UP * 16);
        assert_eq!(view.drawn_total().bytes, 300_000);
        assert_eq!(view.drawn_freed(), 100_000);
    }

    #[test]
    fn the_batch_report_replaces_the_running_total_rather_than_adding_to_it() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/old/target", Size::Measured(100_000), 100));
        let mut view = View::new(tree);
        view.viewport(40);
        let start = Instant::now();
        view.animate(start);

        view.freeing(Path::new("/scan/old/target"), 60_000);
        view.removed(Path::new("/scan/old/target"), 100_000, true);
        view.animate(start + COUNT_UP * 8);
        assert_eq!(view.drawn_freed(), 100_000);

        // The batch's own arithmetic over the same bytes. Added to what the counter had
        // already climbed, this would read 200_000 — every byte counted twice, on the one
        // number a reader came back for.
        view.deleted(
            Notice::passing("removed 97.7 KiB from 1 directory"),
            100_000,
        );
        view.animate(start + COUNT_UP * 16);
        assert_eq!(view.drawn_freed(), 100_000);

        // …and it survives the row finally collapsing away, which is what would drop the
        // per-target figures if they were still the ones being counted.
        view.animate(start + COUNT_UP * 16 + DIM * 2);
        assert_eq!(view.drawn_freed(), 100_000);
        assert!(view.tree().find(Path::new("/scan/old/target")).is_none());
    }

    #[test]
    fn a_directory_the_safety_model_refused_says_so_on_its_own_row() {
        let mut view = view();
        view.refused(&[Refused {
            path: PathBuf::from("/scan/old/target"),
            reason: Refusal::HoldsCheckout,
        }]);

        // The footer says how many were left alone and then moves on to the next thing. Only
        // the row can say *which*, and it is the tool working rather than a fault — so it is
        // kept apart from the walk's errors, which is what lets the renderer draw it calmly.
        assert_eq!(
            view.kept_reason(at(&view, "/scan/old/target")),
            Some("holds a git checkout")
        );
        assert_eq!(view.kept_reason(at(&view, "/scan/nx/node_modules")), None);
    }

    // ---- what shows a precious file, and what a mark then takes -------------------------

    /// A tree with an unrecoverable file parked under a directory worth deleting for its bytes.
    ///
    /// The shape the whole rule is about: a reader marks `nx` to get 200 bytes back, and whether
    /// the `.env` two levels down goes with it is decided by one thing only — whether the view
    /// they marked through was showing it.
    fn tree_with_an_env_file() -> Tree {
        let mut tree = Tree::new("/scan");
        tree.insert(sized(
            of_kind("/scan/nx/node_modules", Kind::Dependencies),
            200,
        ));
        tree.insert(sized(
            gitignored_file("/scan/nx/app/.env", Some(Kind::Unrecoverable)),
            40,
        ));
        tree.insert(sized(
            gitignored_file("/scan/nx/app/build.log", Some(Kind::Noise)),
            10,
        ));
        tree
    }

    /// The same tree, on a view that has been told to show files.
    fn with_an_env_file() -> View {
        let mut view = View::new(tree_with_an_env_file());
        view.viewport(40);
        // Showing files, because a rule about what a mark takes has to be tested on a view
        // that can see what it is taking.
        view.apply(Action::ToggleFiles);
        view
    }

    #[test]
    fn a_run_that_asked_for_files_on_the_command_line_opens_showing_them() {
        // The two front ends have to mean the same thing by `--ignored-files`. The walk claims
        // files either way, so without this the flag would be a silent no-op in the tree —
        // which is worse than not having it, because it reads as a request that was honoured.
        let mut tree = Tree::new("/scan");
        tree.insert(sized(
            gitignored_file("/scan/nx/app/.env", Some(Kind::Unrecoverable)),
            40,
        ));

        let mut shut = View::new(tree);
        shut.viewport(40);
        assert_eq!(shut.total().claims, 0);
        assert_eq!(shut.out_of_view(), 1);

        let mut open = View::new({
            let mut tree = Tree::new("/scan");
            tree.insert(sized(
                gitignored_file("/scan/nx/app/.env", Some(Kind::Unrecoverable)),
                40,
            ));
            tree
        })
        .showing_files();
        open.viewport(40);
        assert_eq!(open.total().claims, 1);
        assert_eq!(open.out_of_view(), 0);
    }

    #[test]
    fn a_mark_on_a_parent_takes_every_visible_claim_under_it_precious_ones_included() {
        // **A mark is a statement about a subtree**, and there is no exception to it. An
        // earlier pass excepted the unrecoverable kind unless the mark sat at its exact depth,
        // which is the thing this asserts is gone: it would make the ancestor's fractional
        // glyph describe a set nobody could see, and the mark model would need a rule with no
        // visible spelling. What keeps a `.env` out of a batch is the lens — see the test
        // below — and once it is on screen it is a row like any other.
        let mut view = with_an_env_file();
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Mark);
        view.sync();

        let mut took = batched(&view);
        took.sort();
        assert_eq!(
            took,
            [
                PathBuf::from("/scan/nx/app/.env"),
                PathBuf::from("/scan/nx/app/build.log"),
                PathBuf::from("/scan/nx/node_modules"),
            ]
        );
        // The counter and the batch are one traversal, so the number a reader is shown agrees
        // with what the deed would take — the env file's 40 bytes included.
        assert_eq!(view.marked().claims, 3);
        assert_eq!(view.marked().bytes, 250);
    }

    #[test]
    fn the_lens_a_mark_was_made_through_is_the_only_thing_holding_a_precious_file_back() {
        // **The whole safety design, as one assertion.** A run opens with files off, so a
        // reader who never pressed `i` cannot see a `.env` — and because a mark carries the
        // lens it was made through (#626), widening the view afterwards does not reach back
        // and add one. That is the same lever `default` already pulls on the gitignored tier,
        // and it is the reason no second flag and no special deletion path are needed.
        let mut view = View::new(tree_with_an_env_file());
        view.viewport(40);
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Mark);
        view.sync();

        assert_eq!(batched(&view), [PathBuf::from("/scan/nx/node_modules")]);

        view.apply(Action::ToggleFiles);
        view.sync();
        assert_eq!(
            batched(&view),
            [PathBuf::from("/scan/nx/node_modules")],
            "widening the view changed what an existing mark covers"
        );
    }

    #[test]
    fn a_precious_row_marked_on_its_own_is_an_ordinary_row() {
        // Same keys, same rules: the label names what the thing is and gates nothing.
        let mut view = with_an_env_file();
        point_at(&mut view, "/scan/nx/app/.env");
        view.apply(Action::Mark);
        view.sync();

        assert_eq!(batched(&view), [PathBuf::from("/scan/nx/app/.env")]);
        assert_eq!(view.marked().claims, 1);
    }

    #[test]
    fn a_precious_claim_arriving_under_a_mark_later_joins_it_like_any_other() {
        // Marks resolve on demand as the scan streams claims in, which is what makes "mark
        // this directory" mean the directory rather than the rows found so far. A claim of any
        // kind arriving under it is therefore covered on arrival — that is what the reader
        // asked for, and singling one kind out of it would be the exception this no longer has.
        let mut view = with_an_env_file();
        point_at(&mut view, "/scan/nx");
        view.apply(Action::Mark);
        view.sync();
        let before = view.marked().claims;

        view.found(sized(
            gitignored_file("/scan/nx/deep/id_rsa", Some(Kind::Unrecoverable)),
            8,
        ));
        view.found(sized(of_kind("/scan/nx/deep/dist", Kind::Build), 5));
        view.sync();

        assert_eq!(view.marked().claims, before + 2);
        assert!(batched(&view).contains(&PathBuf::from("/scan/nx/deep/id_rsa")));
    }

    #[test]
    fn an_unrecoverable_entry_is_countable_on_the_confirmation() {
        // The confirmation is the last place a reader can change their mind, so what it holds
        // has to be distinguishable *there* rather than only styled differently in the tree.
        let mut view = with_an_env_file();
        view.asking(
            &[
                Planned::at("/scan/nx/app/.env", Size::Measured(40)),
                Planned::at("/scan/nx/node_modules", Size::Measured(200)),
            ],
            &[],
        );

        let pending = view.pending().expect("the box is up");
        assert_eq!(pending.unrecoverable(), 1);
        // Listed first, because the vocabulary is ordered by what it costs to lose and the
        // listing groups by that order.
        assert_eq!(pending.entries()[0].path, PathBuf::from("/scan/nx/app/.env"));
        assert_eq!(pending.entries()[0].kind, Some(Kind::Unrecoverable));
    }

    #[test]
    fn a_refused_unrecoverable_entry_is_not_what_the_warning_is_about() {
        // A line the safety model is leaving standing is not a line this warning is about,
        // and counting it would put a red sentence over a batch that takes nothing precious.
        let mut view = with_an_env_file();
        view.asking(
            &[Planned::at("/scan/nx/node_modules", Size::Measured(200))],
            &[Refused {
                path: PathBuf::from("/scan/nx/app/.env"),
                reason: Refusal::HoldsCheckout,
            }],
        );

        assert_eq!(view.pending().expect("the box is up").unrecoverable(), 0);
    }

    /// The batch, as paths, for the tests that are about *what* is in it.
    fn batched(view: &View) -> Vec<PathBuf> {
        view.batch().into_iter().map(|target| target.path).collect()
    }

    /// Applies a filter the way a reader does.
    fn filter(view: &mut View, pattern: &str) {
        view.apply(Action::OpenFilter);
        for character in pattern.chars() {
            view.apply(Action::Type(character));
        }
        view.apply(Action::Submit);
    }

    /// A resolved plan's worth of question, without needing a filesystem to resolve one.
    /// A resolved plan's worth of targets, without a filesystem to resolve one against.
    fn planned(targets: &[&str]) -> Vec<Planned> {
        targets
            .iter()
            .map(|path| Planned::at(*path, Size::Measured(10)))
            .collect()
    }

    /// The same, for tests that care what each target is worth — the batch's weight is what
    /// the footer's byte figure is a fraction of, and a batch of equal targets cannot show
    /// that the largest one is the one being named.
    fn priced(targets: &[(&str, u64)]) -> Vec<Planned> {
        targets
            .iter()
            .map(|(path, bytes)| Planned::at(*path, Size::Measured(*bytes)))
            .collect()
    }
}

#[cfg(test)]
mod scale {
    //! The spike the task asked for, kept so it can be re-run rather than believed.
    //!
    //! `pristine ~` on one real machine finds **16,013 claims across 22,765 directories**, and
    //! the shape that matters is not the total: one level is **8,660 wide**
    //! (`definitely-typed/types`, one `node_modules` per package). The question was whether a
    //! tree survives that, or whether "collapse everything above N children" has to be
    //! designed in before the layout is locked.
    //!
    //! It survives, and not narrowly. Fully expanded — 32,634 rows in the fixture below, which
    //! is deliberately larger than the real thing — a re-sort and a re-flatten of *everything*
    //! costs **1.5 ms** in release and 23 ms in debug, against a 100 ms frame. A row is never
    //! measured, only counted, so the cost is in the sort and the walk rather than in the
    //! drawing, and the drawing is bounded by the viewport whatever the tree does.
    //!
    //! So there is no fan-out cap, and the reason is measured rather than assumed. Collapsed
    //! by default was already the answer; the numbers say it did not need a second one.
    //!
    //! # What the motion costs, on the same fixture
    //!
    //! The frame rate doubles and a bit while something is moving, so the budget these have to
    //! fit inside is **33 ms** rather than 100. Release, fully expanded, all 32,634 rows:
    //!
    //! | | per frame |
    //! |---|---|
    //! | nothing arriving | **1 µs** |
    //! | a claim arriving, on a view that hides nothing | **782 µs** |
    //! | the same, on the narrowed view a run opens on | **1.7 ms** |
    //! | the same, everything marked | **1.9 ms** |
    //! | the same, one row spared out of it | **2.4 ms** |
    //! | the same, with a treemap on the screen | **+67 µs** |
    //!
    //! Six things worth reading off that. The interpolation itself is the first row — one
    //! microsecond, because it is one entry per row the *pane* drew and the pane is fifty rows
    //! whatever the tree holds. The second is #602's own number: the sort and the re-flatten of
    //! everything, which neither the animation nor the marks touched.
    //!
    //! The gap between the second row and the third is **what the narrowed default costs**, and
    //! it is the one number #626 had to measure rather than argue. A view that hides something
    //! has to be able to say what each row is worth *under it*, and a mark is a (directory,
    //! view) pair whose partial glyph is computed against the **current** view — so "what is
    //! visible and what of it is selected" is a walk of the whole tree once per frame that does
    //! any work. The run opens on `default`, which hides the gitignored tier, so that walk
    //! happens from the first frame: about 870 µs for 32,634 nodes, or 2.6% of the budget. On
    //! `all-ignored` or `all` with nothing marked the pass is skipped outright, which is the
    //! second row.
    //!
    //! **That walk is a `Vec` rather than a `HashMap`, and the difference was 3×.** The first
    //! version hashed on [`NodeId`] and cost 7.2 ms — four `SipHash`es per node, for a key that
    //! is already a dense arena index the tree never recycles. Indexing is what makes a
    //! whole-tree pass per frame affordable at all, and it is worth knowing before the next
    //! per-node cache is added.
    //!
    //! Sparing a row out of a marked-everything used to be the expensive case rather than a
    //! rounding error on it: the mark was pushed down onto every sibling along the path, one of
    //! those levels is 8,660 wide, and the per-frame fold then ran over 8,661 marks. #626
    //! replaced the push-down with a single exclusion, so it is now **1 mark and 1 exclusion** —
    //! which was a correctness change first (a push-down silently spares whatever streams in
    //! beside the spared row) and a performance one by accident.
    //!
    //! The last row is what #631 added, and it is the cheapest thing in the table for what it
    //! buys. [`View::map_stamp`] folds one FNV per node onto this same walk, which lets the
    //! treemap answer "has anything I draw changed" without laying the map out — 467 µs a
    //! frame, forever, replaced by 50 ns. It is folded here rather than kept beside the tree
    //! because the tree's own stamp is **lens-blind**, and a run opens on a view that hides a
    //! whole tier: those arrivals must not buy a megabyte of redraw. It is skipped entirely
    //! when there is no pane, which on most terminals is always.
    //!
    //! And a frame only pays any of this when something moved. `sync` runs the pass behind the
    //! same `stale` flag as the sort, so a reader sitting looking at a still tree pays the first
    //! row and nothing else.
    //!
    //! `cargo test --release --lib scale -- --ignored --nocapture` re-runs it.

    use crate::fixture::priced;
    use crate::tree::{Order, Tree};
    use crate::tui::keymap::{Action, Motion, Turn};
    use crate::tui::state::View;
    use crate::tui::treemap::Maps;
    use std::time::Instant;

    /// A tree shaped like the home directory the spike measured.
    fn home() -> Tree {
        let mut tree = Tree::new("/home");
        for n in 0..8_660 {
            tree.insert(priced(&format!("/home/types/p{n}/node_modules"), 1024));
        }
        for repo in 0..300 {
            for pkg in 0..20 {
                tree.insert(priced(
                    &format!("/home/repos/r{repo}/packages/p{pkg}/node_modules"),
                    4096,
                ));
            }
        }
        for n in 0..1_353 {
            tree.insert(priced(&format!("/home/cache/a/b/c/d/e{n}/target"), 512));
        }
        tree
    }

    #[test]
    #[ignore = "a measurement rather than an assertion; timings are not a pass or a fail"]
    fn measure_a_home_directorys_worth_of_rows() {
        let tree = home();
        println!("nodes: {}, claims: {}", tree.len(), tree.claims());

        let started = Instant::now();
        let mut view = View::new(tree);
        view.viewport(50);
        println!("open, collapsed:            {:?}", started.elapsed());

        let started = Instant::now();
        view.apply(Action::Cursor(Motion::Top));
        // Twice: the root starts open, so the first `*` closes it and the second opens
        // everything underneath.
        view.apply(Action::ToggleSubtree);
        view.apply(Action::ToggleSubtree);
        println!(
            "expand everything:          {:?} -> {} rows",
            started.elapsed(),
            view.rows().len()
        );

        let started = Instant::now();
        view.apply(Action::SortBy(Order::Path));
        println!("re-sort, fully expanded:    {:?}", started.elapsed());

        let started = Instant::now();
        view.found(priced("/home/repos/late/node_modules", 1));
        view.sync();
        println!("one arrival, fully expanded: {:?}", started.elapsed());
    }

    #[test]
    #[ignore = "a measurement rather than an assertion; timings are not a pass or a fail"]
    fn measure_what_one_animated_frame_costs() {
        // The budget the motion work had to fit inside, and the reason every effect is keyed
        // by node and advanced from the viewport rather than from the tree. The frame rate
        // goes to 33 ms while something is moving, so that — not 100 ms — is what these have
        // to fit in, and the interesting cases are the two per-frame folds that are *not*
        // bounded by the pane: the marks and the drains.
        let tree = home();
        let mut view = View::new(tree);
        view.viewport(50);
        view.apply(Action::Cursor(Motion::Top));
        view.apply(Action::ToggleSubtree);
        view.apply(Action::ToggleSubtree);
        let epoch = Instant::now();
        println!("rows: {}", view.rows().len());

        let started = Instant::now();
        for tick in 1..=100 {
            view.animate(epoch + super::super::moving::COUNT_UP * tick);
        }
        println!("quiet frame:                {:?}", started.elapsed() / 100);

        // A frame with something arriving on it, which is the only kind that does any work:
        // `sync` folds the marks and the drains only when the tree or the marks moved, so a
        // view a reader is sitting and looking at costs the line above and nothing more.
        let started = Instant::now();
        for tick in 101..=200u32 {
            view.found(priced(&format!("/home/repos/late{tick}/node_modules"), 1));
            view.animate(epoch + super::super::moving::COUNT_UP * tick);
        }
        println!("frame during a scan:        {:?}", started.elapsed() / 100);

        // The same frame on a view that hides nothing, which is the one case that skips the
        // whole-tree pass. It is what the run *used* to open on, so the difference between
        // this line and the one above is exactly what the narrowed default costs per frame.
        showing(&mut view, super::Preset::All);
        let started = Instant::now();
        for tick in 201..=300u32 {
            view.found(priced(&format!("/home/repos/wide{tick}/node_modules"), 1));
            view.animate(epoch + super::super::moving::COUNT_UP * tick);
        }
        println!("frame, nothing hidden:      {:?}", started.elapsed() / 100);

        // And the expensive one, which is the whole reason this is measured rather than
        // assumed. A mark is a (directory, view) pair resolved on demand, so "what is
        // selected" is a walk of the whole tree rather than a fold over a handful of marks —
        // one pass per frame that does any work, over 32,634 nodes.
        showing(&mut view, super::Preset::Default);
        view.apply(Action::MarkAll);
        let started = Instant::now();
        for tick in 301..=400u32 {
            view.found(priced(&format!("/home/repos/later{tick}/node_modules"), 1));
            view.animate(epoch + super::super::moving::COUNT_UP * tick);
        }
        println!("frame, everything marked:   {:?}", started.elapsed() / 100);

        // Sparing one row out of that used to push the root's mark down onto every sibling
        // along the path — and one of those levels is 8,660 wide. An exclusion says the same
        // thing in one entry, so this is now the line above plus nothing.
        select(&mut view, "/home/types/p0/node_modules");
        view.apply(Action::Mark);
        println!(
            "marks and exclusions:       {} + {}",
            view.marks.len(),
            view.spared.len()
        );
        let started = Instant::now();
        for tick in 401..=500u32 {
            view.found(priced(&format!("/home/repos/spared{tick}/node_modules"), 1));
            view.animate(epoch + super::super::moving::COUNT_UP * tick);
        }
        println!("frame, one row spared:      {:?}", started.elapsed() / 100);

        // …and with a view narrowing on top, which is the pass at its most expensive: every
        // claim is asked whether it survives the lens as well as whether a mark covers it.
        view.apply(Action::ToggleKind(crate::rules::Kind::Dependencies));
        let started = Instant::now();
        for tick in 501..=600u32 {
            view.found(priced(&format!("/home/repos/lens{tick}/node_modules"), 1));
            view.animate(epoch + super::super::moving::COUNT_UP * tick);
        }
        println!("frame, marked and narrowed: {:?}", started.elapsed() / 100);

        // And the same again with a treemap on the screen, which is what [`View::map_stamp`]
        // adds: one FNV fold per node, on the pass that is already walking every one of them.
        // It buys the map the right to ask "has anything I draw changed" for nothing, on a
        // frame where the answer is usually no — see [`super::super::treemap`]. Off by default
        // and computed only when there is a pane, because most terminals never draw one.
        view.allow_maps(Maps::Can);
        let started = Instant::now();
        for tick in 601..=700u32 {
            view.found(priced(&format!("/home/repos/map{tick}/node_modules"), 1));
            view.animate(epoch + super::super::moving::COUNT_UP * tick);
        }
        println!("…the same, with a map up:   {:?}", started.elapsed() / 100);
    }

    /// Presses `f` until the view is the one named.
    fn showing(view: &mut View, preset: super::Preset) {
        for _ in 0..super::Preset::ALL.len() {
            if view.preset() == Some(preset) {
                return;
            }
            view.apply(Action::CyclePreset(Turn::Next));
        }
        panic!("{preset} is not on the cycle");
    }

    /// Puts the cursor on a row by walking to it, which is all a reader can do.
    fn select(view: &mut View, path: &str) {
        let want = std::path::PathBuf::from(path);
        let at = view
            .rows()
            .iter()
            .position(|row| view.tree().node(row.id).path == want)
            .expect("the fixture is fully expanded");
        view.apply(Action::Cursor(Motion::Top));
        for _ in 0..at {
            view.apply(Action::Cursor(Motion::Down));
        }
    }
}
