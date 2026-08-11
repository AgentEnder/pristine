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
//! # The clock is handed in, like everything else
//!
//! This file animates ([`super::moving`]) and still has no terminal and no filesystem in it,
//! because time arrives the same way a keystroke does: [`View::animate`] is given the instant
//! and everything else reads it off [`View`]. So "a removed row empties for a third of a
//! second and then collapses away" is an assertion with three `advance`s in it rather than a
//! test that sleeps, and the drain's *consequences* — a row that can no longer be marked,
//! deleted a second time, or counted into a batch — are assertions too.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use regex::Regex;

use super::keymap::{Action, Motion, Overlay, Turn};
use super::moving::Moving;
use crate::delete::{Plan, Refused, Target};
use crate::size::{Size, human};
use crate::tree::{NodeId, Sort, Tree};
use crate::walk::Hit;

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Removing {
    /// Targets the confirmed plan handed to the deleter.
    total: usize,
    /// Targets the deleter has reported finishing with, whole or in part.
    done: usize,
}

impl Removing {
    /// The start of a batch of `total` targets.
    fn new(total: usize) -> Self {
        Self { total, done: 0 }
    }

    /// Notes one more target the deleter has come back out of.
    ///
    /// Capped at the total rather than allowed past it: the count is a position in a batch of
    /// known size, and a `13 of 12` would say the batch was not what the confirmation said it
    /// was — which is the one thing the dialog promises.
    fn finished(&mut self) {
        self.done = self.done.saturating_add(1).min(self.total);
    }

    /// Targets done, and how many there are.
    #[must_use]
    pub fn counted(self) -> (usize, usize) {
        (self.done, self.total)
    }

    /// How far through, for the footer and for the dock.
    #[must_use]
    pub fn percent(self) -> u8 {
        percent(self.done, self.total)
    }

    /// What the footer says, which is where the deleter is rather than only what it has given
    /// back. The freed counter beside it already carries the bytes.
    #[must_use]
    pub fn label(self) -> String {
        format!(
            "removing {} of {} · {}%",
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

/// The question the delete key asks, and everything it is holding while it asks.
///
/// It carries the **targets the plan resolved**, not "whatever is marked when the answer is
/// taken". The tree moves while a dialog is up — claims arrive, prices land, an earlier
/// deletion finishes — and a deed that re-read the marks at the moment of the answer would
/// remove a different set from the one the box described.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    /// Exactly what will be removed.
    pub targets: Vec<PathBuf>,
    /// What the plan says that is worth, which is only the part anybody has priced.
    pub bytes: u64,
    /// How many of the targets carry no price.
    pub unpriced: usize,
    /// Directories the plan refused, with the reason, so the box can say what it is *not*
    /// going to do. The safety model working is not an error, but it is news.
    pub kept: Vec<String>,
    /// Which answer is highlighted. Starts on cancel — the key a reader presses to get rid of
    /// what is in front of them has to be the safe one.
    pub answer: Answer,
}

impl Pending {
    /// The question a resolved [`Plan`] asks.
    #[must_use]
    pub fn of(plan: &Plan) -> Self {
        Self {
            targets: plan
                .targets()
                .iter()
                .map(|target| target.path.clone())
                .collect(),
            bytes: plan.measured_bytes(),
            unpriced: plan.unpriced(),
            kept: plan
                .kept()
                .iter()
                .map(|refused| format!("{}: {}", refused.path.display(), refused.reason))
                .collect(),
            answer: Answer::Cancel,
        }
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

/// An applied filter, and what it makes each surviving node worth.
///
/// The rolled-up numbers are recomputed rather than read off the tree, and that is a **safety**
/// property before it is a cosmetic one. A row showing 312 GiB while the filter hides all but
/// 2 GiB of it is a row whose mark would delete 310 GiB the reader cannot see. Under a filter,
/// a row's number, its selection counter and its batch all describe the same thing: the claims
/// that match.
#[derive(Debug)]
struct Filter {
    pattern: String,
    regex: Regex,
    /// Only the nodes with at least one matching claim beneath them. Absent means hidden.
    rolls: HashMap<NodeId, Roll>,
}

impl Filter {
    fn new(pattern: &str) -> Result<Self, regex::Error> {
        Ok(Self {
            pattern: pattern.to_owned(),
            regex: Regex::new(pattern)?,
            rolls: HashMap::new(),
        })
    }

    /// Re-asks the whole tree what matches.
    ///
    /// Whole, on every change, rather than incrementally: the scan streams claims into
    /// arbitrary places in the tree, so an incremental update would have to be right about
    /// every arrival, every price and every deletion — three chances to leave a stale number
    /// on a row that a mark then acts on. One post-order pass over 22,765 nodes is a fraction
    /// of a frame.
    fn recompute(&mut self, tree: &Tree) {
        self.rolls.clear();
        // An explicit stack rather than recursion: the depth here is the filesystem's, and
        // nothing stops a checkout from being nested far deeper than the ten levels a real
        // home directory reaches.
        let mut stack = vec![(tree.root(), false)];
        while let Some((id, folding)) = stack.pop() {
            let node = tree.node(id);
            if let Some(hit) = &node.hit {
                if self.regex.is_match(&hit.path.to_string_lossy()) {
                    self.rolls.insert(
                        id,
                        Roll {
                            bytes: node.reclaimable,
                            claims: 1,
                            unpriced: node.unmeasured,
                        },
                    );
                }
            } else if folding {
                let roll = node
                    .children
                    .iter()
                    .fold(Roll::default(), |mut roll, child| {
                        let child = self.rolls.get(child).copied().unwrap_or_default();
                        roll.bytes += child.bytes;
                        roll.claims += child.claims;
                        roll.unpriced += child.unpriced;
                        roll
                    });
                if roll.claims > 0 {
                    self.rolls.insert(id, roll);
                }
            } else {
                stack.push((id, true));
                for &child in &node.children {
                    stack.push((child, false));
                }
            }
        }
    }
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
    /// The roots of the marked subtrees. See the module docs.
    marks: HashSet<NodeId>,
    /// What is marked strictly below each node — what makes a partial state O(1) to ask about
    /// instead of a subtree walk per row per frame.
    ///
    /// A [`Roll`] rather than a count, because a partial ancestor now says *how* partial it
    /// is: the glyph is a block filled in proportion to the marked share of that subtree's
    /// bytes, which is a real number put in one character. Recomputed whole from the marks
    /// rather than adjusted as they change, for the reason the filter states about its own
    /// rolls — bytes move under a mark as prices land and rows are deleted, and an
    /// incremental count would have to be right about all of that. It costs one walk up the
    /// ancestors per mark, and a reader has a handful of marks.
    below: HashMap<NodeId, Roll>,
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
    filter: Option<Filter>,
    prompt: Option<Prompt>,
    help: Option<usize>,
    pending: Option<Pending>,
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
    /// What just happened, for the footer to say.
    notice: Option<String>,
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
            marks: HashSet::new(),
            below: HashMap::new(),
            rows: Vec::new(),
            cursor: None,
            deselected: false,
            scroll: 0,
            page: 20,
            filter: None,
            prompt: None,
            help: None,
            pending: None,
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
        };
        view.sync();
        view
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
        // Counted before the row is looked up, and counted whether or not the sweep finished:
        // this says where the deleter *is*, and a target it went into and came back out of is
        // one it is no longer working on. A target whose row has already gone from the tree —
        // the reader filtered it away, an earlier batch took it — has still been dealt with,
        // so a progress bar that skipped it would stall short of the truth.
        if let Some(removing) = &mut self.removing {
            removing.finished();
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

    /// The removal is over, `notice` is what it did, and `freed` is what the session has given
    /// back across every batch it has run.
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
    pub fn deleted(&mut self, notice: String, freed: u64) {
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
    /// An empty plan is not a question. It happens for a real reason — every marked directory
    /// was refused by the safety model — so it says so rather than putting up a box with
    /// nothing in it.
    pub fn ask(&mut self, pending: Pending) {
        if pending.targets.is_empty() {
            let kept = pending.kept.len();
            self.notice = Some(if kept == 0 {
                "nothing to delete".to_owned()
            } else {
                format!("nothing to delete: {kept} left alone by the safety model")
            });
            return;
        }
        self.pending = Some(pending);
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
        self.marks
            .retain(|&id| tree.is_attached(id) && !(moving.is_freeing(id) || moving.is_spent(id)));
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
        self.recount_marks();
        if let Some(filter) = &mut self.filter {
            filter.recompute(&self.tree);
        }
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

    /// What this row's subtree is worth — under the filter, when there is one.
    #[must_use]
    pub fn roll(&self, id: NodeId) -> Roll {
        if let Some(filter) = &self.filter {
            return filter.rolls.get(&id).copied().unwrap_or_default();
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

    /// How much of this row's subtree is marked.
    #[must_use]
    pub fn mark_of(&self, id: NodeId) -> Mark {
        if self.covered(id) {
            Mark::All
        } else if self.below.get(&id).is_some_and(|roll| roll.claims > 0) {
            Mark::Partial
        } else {
            Mark::None
        }
    }

    /// What share of this row's subtree is marked, between 0.0 and 1.0.
    ///
    /// By **bytes**, which is what a reader deciding whether a partial ancestor is worth
    /// opening actually wants: forty marked directories out of fifty means nothing if the
    /// other ten hold all the space. Claims are the fallback for a subtree nobody has priced
    /// yet, where bytes cannot answer and the count is the only thing that is true.
    #[must_use]
    pub fn share(&self, id: NodeId) -> f64 {
        if self.covered(id) {
            return 1.0;
        }
        let whole = self.roll(id);
        let marked = self.below.get(&id).copied().unwrap_or_default();
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
    /// Net of the rows the deleter has already finished with, which are in the tree for
    /// another third of a second while they empty. They are out of [`View::batch`], so they
    /// have to be out of the number that describes it: a counter and a batch that disagree is
    /// exactly the arithmetic a reader would catch first.
    #[must_use]
    pub fn marked(&self) -> Roll {
        self.marks.iter().fold(Roll::default(), |mut total, &id| {
            let roll = self.live(id);
            total.bytes += roll.bytes;
            total.claims += roll.claims;
            total.unpriced += roll.unpriced;
            total
        })
    }

    /// The whole scan, as the header states it.
    #[must_use]
    pub fn total(&self) -> Roll {
        self.roll(self.tree.root())
    }

    /// The applied filter's pattern, if there is one.
    #[must_use]
    pub fn filter(&self) -> Option<&str> {
        self.filter.as_ref().map(|filter| filter.pattern.as_str())
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
    pub fn removing(&self) -> Option<Removing> {
        self.removing
    }

    /// Puts the view mid-removal without one having happened.
    ///
    /// The event loop's own tests need a view that is waiting on a deleter, and the honest
    /// door into that state runs an actual removal against an actual filesystem.
    #[cfg(test)]
    pub(crate) fn deleting_for_test(&mut self) {
        self.removing = Some(Removing::new(1));
    }

    /// What just happened, for the footer.
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
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
        match action {
            Action::Quit => return self.quit(),
            Action::Ignore => {}
            Action::Help => {
                self.help = if self.help.is_some() { None } else { Some(0) };
            }
            Action::Back => self.step_back(),
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
            Action::Cursor(motion) => self.move_cursor(motion),
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
            Action::CycleSort => self.resort(Sort {
                by: self.sort.by.next(),
                reverse: self.sort.reverse,
            }),
            Action::ReverseSort => self.resort(Sort {
                reverse: !self.sort.reverse,
                ..self.sort
            }),
            Action::SortBy(order) => self.resort(Sort::by(order)),
        }
        self.sync();
        Effect::None
    }

    /// `q`: leave — unless something irreversible is in flight, in which case wait for it.
    ///
    /// The wait is bounded by the work the reader themselves asked for, and it is *visible*:
    /// rows keep disappearing as the deleter finishes each target. Tearing the batch in half
    /// would not be.
    fn quit(&mut self) -> Effect {
        if self.is_deleting() {
            self.quitting = true;
            self.notice = Some("the removal has to finish — closing the moment it does".to_owned());
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
        if self.filter.take().is_some() {
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
            self.filter = None;
            self.stale = true;
            return;
        }
        match Filter::new(&pattern) {
            Ok(filter) => {
                self.filter = Some(filter);
                self.prompt = None;
                self.stale = true;
            }
            Err(err) => {
                let reason = err.to_string();
                prompt.error = Some(reason.lines().last().unwrap_or("not a regex").to_owned());
            }
        }
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
                self.removing = Some(Removing::new(pending.targets.len()));
                self.notice = None;
                Effect::Delete(pending.targets)
            }
        }
    }

    /// `x`: hand the marked batch out to be planned.
    fn commit(&mut self) -> Effect {
        if self.is_deleting() {
            self.notice = Some("a removal is already running".to_owned());
            return Effect::None;
        }
        let batch = self.batch();
        if batch.is_empty() {
            self.notice = Some("nothing is marked — space marks a row's whole subtree".to_owned());
            return Effect::None;
        }
        Effect::Plan(batch)
    }

    /// Every claim under a mark, which is what a batch is.
    ///
    /// Filtered, and that is the safety rule the filter's own rolled-up numbers exist to make
    /// visible: what a mark deletes is what its row says it is worth. A claim the filter is
    /// hiding is not in the batch, however marked its ancestor.
    ///
    /// A row that is draining away is out too. It is on screen for another third of a second
    /// saying what happened to it, and offering a directory that is already gone to a second
    /// removal would report a failure for a target the first removal succeeded on.
    #[must_use]
    pub fn batch(&self) -> Vec<Target> {
        let mut batch = Vec::new();
        let mut stack: Vec<NodeId> = self.marks.iter().copied().collect();
        while let Some(id) = stack.pop() {
            if !self.shown(id) || self.is_leaving(id) {
                continue;
            }
            let node = self.tree.node(id);
            if let Some(hit) = &node.hit {
                batch.push(Target::from(hit));
            } else {
                stack.extend(node.children.iter().copied());
            }
        }
        batch.sort_by(|a, b| a.path.cmp(&b.path));
        batch
    }

    fn resort(&mut self, sort: Sort) {
        self.sort = sort;
        self.sorted = false;
        self.stale = true;
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

    /// Whether a node survives the filter. Everything survives when there is not one.
    fn shown(&self, id: NodeId) -> bool {
        match &self.filter {
            Some(filter) => filter.rolls.contains_key(&id),
            None => true,
        }
    }

    // ---- marking ----------------------------------------------------------------------

    /// `space`: mark the cursor's subtree, or unmark it.
    ///
    /// A mark runs visibly up the ancestors on its way in — see [`Moving::cascade`]. It is the
    /// signature interaction and the one whose effect is otherwise entirely off screen: a mark
    /// on a collapsed row takes everything underneath, and the only place that shows is on
    /// ancestors the reader is not looking at. The cascade is that fact, drawn.
    fn toggle_mark(&mut self) {
        let Some(row) = self.row() else {
            return;
        };
        // A directory the deleter has already finished with is not something to mark for
        // deletion. Its row is still on screen because it is emptying, which is a statement
        // about the past.
        if self.is_leaving(row.id) {
            return;
        }
        if self.covered(row.id) {
            self.unmark(row.id);
        } else {
            let chain = self.ancestry(row.id);
            self.moving.cascade(&chain, self.now);
            self.mark(row.id);
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

    /// Whether this node is inside a marked subtree.
    fn covered(&self, id: NodeId) -> bool {
        let mut at = Some(id);
        while let Some(current) = at {
            if self.marks.contains(&current) {
                return true;
            }
            at = self.tree.node(current).parent;
        }
        false
    }

    /// Marks a subtree, absorbing any marks already inside it.
    fn mark(&mut self, id: NodeId) {
        let inside: Vec<NodeId> = self
            .marks
            .iter()
            .copied()
            .filter(|&mark| mark != id && self.descends_from(mark, id))
            .collect();
        for mark in inside {
            self.drop_mark(mark);
        }
        self.add_mark(id);
    }

    /// Unmarks a subtree, pushing an ancestor's mark down around it if that is what is
    /// covering it.
    ///
    /// The push-down is the whole reason marks are subtree roots rather than a flat set, and
    /// it is what makes "mark the lot, then spare this one" a two-keystroke operation. Every
    /// sibling along the way inherits the mark, so what was covered stays covered except for
    /// the one subtree being spared.
    fn unmark(&mut self, id: NodeId) {
        if self.marks.contains(&id) {
            self.drop_mark(id);
            return;
        }
        let mut path = vec![id];
        let mut at = self.tree.node(id).parent;
        while let Some(current) = at {
            path.push(current);
            if self.marks.contains(&current) {
                break;
            }
            at = self.tree.node(current).parent;
        }
        let Some(&holder) = path.last().filter(|&&top| self.marks.contains(&top)) else {
            return;
        };
        self.drop_mark(holder);
        // From the holder downwards, marking everything beside the path.
        for step in (1..path.len()).rev() {
            let above = path[step];
            let spared = path[step - 1];
            for child in self.tree.children(above).to_vec() {
                if child != spared {
                    self.add_mark(child);
                }
            }
        }
    }

    fn clear_marks(&mut self) {
        self.marks.clear();
        self.below.clear();
        self.stale = true;
    }

    fn add_mark(&mut self, id: NodeId) {
        self.stale = self.marks.insert(id) || self.stale;
    }

    fn drop_mark(&mut self, id: NodeId) {
        self.stale = self.marks.remove(&id) || self.stale;
    }

    /// Rebuilds what is marked below each node, from the marks that are left.
    ///
    /// From scratch on every sync rather than adjusted as marks come and go, and the reason is
    /// the filter's own: this carries **bytes** now, and bytes under a mark move without the
    /// marks moving at all — a price lands, a deletion finishes, a claim streams in under a
    /// directory that was marked while it was still empty. An incremental total would have to
    /// be right about every one of those, and being wrong leaves a partial glyph reporting a
    /// share of a subtree that is no longer that shape.
    ///
    /// One walk up the ancestors per mark, so it is bounded by marks × depth rather than by
    /// the tree: a reader's handful of marks against ten levels. The one case that is not a
    /// handful is a push-down (see [`View::unmark`]), which can leave a mark per sibling on a
    /// wide level — and that is what the `scale` module measures rather than assumes.
    fn recount_marks(&mut self) {
        self.below.clear();
        for id in self.marks.clone() {
            let roll = self.roll(id);
            let mut at = self.tree.node(id).parent;
            while let Some(current) = at {
                let below = self.below.entry(current).or_default();
                below.bytes += roll.bytes;
                below.claims += roll.claims;
                below.unpriced += roll.unpriced;
                at = self.tree.node(current).parent;
            }
        }
    }

    /// Whether `id` is at or under `root`.
    ///
    /// Walked upwards from the node rather than downwards from the root, because a chain is a
    /// handful of steps and a subtree can be most of the tree.
    fn descends_from(&self, id: NodeId, root: NodeId) -> bool {
        let mut at = Some(id);
        while let Some(current) = at {
            if current == root {
                return true;
            }
            at = self.tree.node(current).parent;
        }
        false
    }
}

/// `1 directory`, `4 directories`.
#[must_use]
pub fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use super::{Action, Answer, Effect, Mark, Motion, Overlay, Pending, Turn, View};
    use crate::delete::{Refusal, Refused};
    use crate::fixture::hit;
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
        view.ask(pending(&["/scan/old/target"]));
        assert_eq!(view.overlay(), Some(Overlay::Confirm));
        assert_eq!(view.pending().unwrap().answer, Answer::Cancel);

        assert_eq!(view.apply(Action::Answer), Effect::None);
        assert_eq!(view.overlay(), None);
        assert!(!view.is_deleting());

        view.ask(pending(&["/scan/old/target"]));
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
        view.ask(pending(&["/scan/old/target"]));

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
        view.ask(Pending {
            targets: Vec::new(),
            bytes: 0,
            unpriced: 0,
            kept: vec!["/scan/old/target: it holds a git checkout".to_owned()],
            answer: Answer::Cancel,
        });
        assert_eq!(view.overlay(), None);
        assert!(view.notice().unwrap().contains("left alone"));
    }

    #[test]
    fn a_second_delete_while_one_is_running_is_refused_rather_than_racing_it() {
        let mut view = view();
        view.ask(pending(&["/scan/old/target"]));
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

    #[test]
    fn quitting_is_the_only_thing_that_ends_the_view() {
        let mut view = view();
        assert_eq!(view.apply(Action::Quit), Effect::Quit);
    }

    #[test]
    fn quitting_cannot_end_a_view_that_is_half_way_through_a_removal() {
        let mut view = view();
        view.ask(pending(&["/scan/old/target"]));
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
        view.deleted("removed 10 B from 1 directory".to_owned(), 10);
        assert!(view.wants_to_quit());
    }

    #[test]
    fn a_view_that_was_never_asked_to_quit_does_not_want_to() {
        let mut view = view();
        assert!(!view.wants_to_quit());
        view.ask(pending(&["/scan/old/target"]));
        view.apply(Action::Highlight(Turn::Next));
        view.apply(Action::Answer);
        view.deleted("removed 10 B from 1 directory".to_owned(), 10);
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
        view.ask(pending(&[
            "/scan/nx/node_modules",
            "/scan/nx/packages/ui/node_modules",
            "/scan/old/target",
        ]));
        view.apply(Action::Highlight(Turn::Next));
        view.apply(Action::Answer);

        // The denominator is fixed here, by the question the reader answered — which is what
        // separates this bar from the pricing one, whose total grows as the walk finds claims.
        assert_eq!(view.removing().unwrap().counted(), (0, 3));
        assert_eq!(view.removing().unwrap().percent(), 0);
        assert_eq!(
            view.removing().unwrap().label(),
            "removing 0 of 3 directories · 0%"
        );

        view.removed(Path::new("/scan/nx/node_modules"), 200, true);
        assert_eq!(view.removing().unwrap().counted(), (1, 3));

        // A target the sweep went into and came back out of counts too: this says where the
        // deleter *is*, and it is no longer working on that one. It is not a claim that the
        // target was removed — the row is still there saying what is left of it.
        view.removed(Path::new("/scan/nx/packages/ui/node_modules"), 40, false);
        assert_eq!(view.removing().unwrap().counted(), (2, 3));
        assert_eq!(view.removing().unwrap().percent(), 66);

        // The third target turns out to be gone already, so the deleter never reports it —
        // `Sweep::reported` only speaks for a target something happened to. The count
        // therefore stops at two of three, which is true, and the batch reporting is what
        // ends the state rather than the count reaching its total.
        view.deleted("removed 240 B from 1 directory".to_owned(), 240);
        assert!(view.removing().is_none());
        assert!(!view.is_deleting());
    }

    #[test]
    fn a_second_removal_is_refused_while_one_is_running() {
        let mut view = view();
        view.ask(pending(&["/scan/old/target"]));
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
        view.deleted("freed 150 B".to_owned(), 150);
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
        view.deleted("removed 97.7 KiB from 1 directory".to_owned(), 100_000);
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
    fn pending(targets: &[&str]) -> Pending {
        Pending {
            targets: targets.iter().map(PathBuf::from).collect(),
            bytes: 10,
            unpriced: 0,
            kept: Vec::new(),
            answer: Answer::Cancel,
        }
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
    //! | a frame with nothing arriving | **2 µs** |
    //! | a frame with a claim arriving | **806 µs** |
    //! | the same, with 8,661 marks | **1.5 ms** |
    //!
    //! Three things worth reading off that. The interpolation itself is the first row — two
    //! microseconds, because it is one entry per row the *pane* drew and the pane is fifty
    //! rows whatever the tree is. The second row is #602's own number: it is the sort and the
    //! re-flatten of everything, which the animation did not touch. The third is the one that
    //! had to be measured rather than argued: sparing one row out of a marked-everything
    //! pushes the mark down onto every sibling along the path, and one of those levels is
    //! 8,660 wide — so the per-frame fold over the marks really does run over 8,661 of them,
    //! and it costs 0.7 ms.
    //!
    //! And a frame only pays any of the last two when something moved. `sync` folds the marks
    //! and the drains behind the same `stale` flag as the sort, so a reader sitting looking at
    //! a still tree pays the 2 µs and nothing else.
    //!
    //! `cargo test --release --lib scale -- --ignored --nocapture` re-runs it.

    use crate::fixture::priced;
    use crate::tree::{Order, Tree};
    use crate::tui::keymap::{Action, Motion};
    use crate::tui::state::View;
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

        // And the expensive one, which is the whole reason this is measured rather than
        // assumed: sparing one row out of a marked-everything pushes the root's mark down the
        // path to it, leaving a mark per sibling on the way — and one of those levels is
        // 8,660 wide. Every frame that does any work then re-folds all of them.
        view.apply(Action::MarkAll);
        select(&mut view, "/home/types/p0/node_modules");
        view.apply(Action::Mark);
        println!("marks after a push-down:    {}", view.marks.len());
        let started = Instant::now();
        for tick in 201..=300u32 {
            view.found(priced(&format!("/home/repos/later{tick}/node_modules"), 1));
            view.animate(epoch + super::super::moving::COUNT_UP * tick);
        }
        println!("frame, marks pushed down:   {:?}", started.elapsed() / 100);
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
