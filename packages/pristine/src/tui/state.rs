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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use regex::Regex;

use super::keymap::{Action, Motion, Overlay, Turn};
use crate::delete::{Plan, Target};
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
    #[must_use]
    pub fn label(&self) -> String {
        if self.bytes == 0 && self.unpriced > 0 {
            Size::Unmeasured.label()
        } else {
            human(self.bytes)
        }
    }
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
    /// Put a size on these claims, which nobody has priced.
    ///
    /// Filesystem work, so the view asks for it rather than doing it — the same split
    /// [`Plan`](Self::Plan) draws. Paths and not [`Target`]s, because the whole point is that
    /// these carry no size yet: the answer comes back as the prices the walk would have sent.
    Price(Vec<PathBuf>),
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
    /// How many marks sit strictly below each node — what makes a partial state O(1) to ask
    /// about instead of a subtree walk per row per frame.
    below: HashMap<NodeId, usize>,
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
    /// Claims a pricing pass has been asked for and has not answered yet.
    ///
    /// The one piece of state that stops a gesture from being repeatable into unbounded
    /// work: a claim in here is one somebody is already traversing, so a second double click
    /// on the row above it asks for nothing. Emptied by [`View::repriced`] on every way a
    /// pass can end — see there for why that matters more than it looks.
    pricing: HashSet<PathBuf>,
    /// Whether the walk is still running, for the header.
    scanning: bool,
    /// Whether a removal is in flight. A second one would race the first over the same tree.
    deleting: bool,
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
    /// The treemap pane: whether this terminal could draw one, and whether the reader wants
    /// it. Told to the view the way [`View::viewport`] is — the renderer owns the fact and
    /// the view owns the decision, so `m` has one place to act on and the layout has one
    /// place to read.
    map: Map,
}

/// Whether the map pane is possible, and whether it is on.
///
/// Two booleans rather than one, because the answer to `m` differs: a reader on a terminal
/// that cannot draw one has to be *told* that, where a silent no-op on a documented key is
/// the same failure shape as a mark box that cannot be pressed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Map {
    possible: bool,
    on: bool,
}

impl View {
    /// A view of a scan that has not found anything yet.
    #[must_use]
    pub fn new(tree: Tree) -> Self {
        let mut view = Self {
            expanded: HashSet::from([tree.root()]),
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
            pricing: HashSet::new(),
            scanning: true,
            deleting: false,
            quitting: false,
            notice: None,
            stale: true,
            sorted: false,
            // On wherever it is possible, which is the spike's own bet: a feature nobody
            // turns on is a feature nobody judges.
            map: Map {
                possible: false,
                on: true,
            },
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

    /// A price for a claim that was published without one.
    pub fn priced(&mut self, path: &Path, size: Size) {
        self.tree.price(path, size);
        self.stale = true;
        self.sorted = false;
    }

    /// A target the deleter has finished with.
    ///
    /// Only a *complete* removal takes the row away. A target the sweep entered and did not
    /// finish — a checkout inside it, an unreadable subtree — is still on disk, and dropping
    /// its row would tell a reader something was deleted that was not.
    pub fn removed(&mut self, path: &Path, complete: bool) {
        if complete {
            self.tree.remove(path);
            self.stale = true;
        }
    }

    /// The walk is over.
    pub fn scanned(&mut self) {
        self.scanning = false;
    }

    /// A pricing pass is over, and these are the claims it was holding.
    ///
    /// The prices themselves arrived one at a time through [`View::priced`], exactly as the
    /// walk's do; this hands the claims back, which is what lets the next double click on
    /// them mean something again. It is called on **every** way a pass can end, the worker
    /// dying included — an in-flight set that leaked would be a subtree the reader can never
    /// ask about again for the rest of the run, which is a quiet permanent no-op on a gesture
    /// they keep making.
    pub fn repriced(&mut self, claims: &[PathBuf], notice: String) {
        for claim in claims {
            self.pricing.remove(claim);
        }
        self.notice = Some(notice);
    }

    /// The removal is over, and this is what it did.
    pub fn deleted(&mut self, notice: String) {
        self.deleting = false;
        self.notice = Some(notice);
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
        // A mark or an open row can outlive the directory it names, because the deleter takes
        // rows away while the reader is looking at them. Dropped here, once per frame, rather
        // than per removal: a batch of ten thousand deletions would otherwise re-scan the
        // whole mark set ten thousand times.
        if self.marks.iter().any(|&id| !self.tree.is_attached(id)) {
            self.marks.retain(|&id| self.tree.is_attached(id));
            self.recount_marks();
        }
        self.expanded.retain(|&id| self.tree.is_attached(id));
        if let Some(filter) = &mut self.filter {
            filter.recompute(&self.tree);
        }
        self.reflatten();
        self.settle(&anchor);
        self.follow_cursor();
        self.stale = false;
    }

    /// Whether this terminal can draw a map at all. Told once, at start-up.
    pub fn allow_maps(&mut self, possible: bool) {
        self.map.possible = possible;
    }

    /// Whether the map pane is on the screen.
    #[must_use]
    pub fn maps(&self) -> bool {
        self.map.possible && self.map.on
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

    /// How much of this row's subtree is marked.
    #[must_use]
    pub fn mark_of(&self, id: NodeId) -> Mark {
        if self.covered(id) {
            Mark::All
        } else if self.below.get(&id).copied().unwrap_or(0) > 0 {
            Mark::Partial
        } else {
            Mark::None
        }
    }

    /// What is marked, all together — the selection counter.
    #[must_use]
    pub fn marked(&self) -> Roll {
        self.marks.iter().fold(Roll::default(), |mut total, &id| {
            let roll = self.roll(id);
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
        self.deleting
    }

    /// Puts the view mid-removal without one having happened.
    ///
    /// The event loop's own tests need a view that is waiting on a deleter, and the honest
    /// door into that state runs an actual removal against an actual filesystem.
    #[cfg(test)]
    pub(crate) fn deleting_for_test(&mut self) {
        self.deleting = true;
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
    fn toggle_map(&mut self) {
        if !self.map.possible {
            self.notice = Some(
                "this terminal does not read the graphics protocol, so there is no map".to_owned(),
            );
            return;
        }
        self.map.on = !self.map.on;
    }

    /// `q`: leave — unless something irreversible is in flight, in which case wait for it.
    ///
    /// The wait is bounded by the work the reader themselves asked for, and it is *visible*:
    /// rows keep disappearing as the deleter finishes each target. Tearing the batch in half
    /// would not be.
    fn quit(&mut self) -> Effect {
        if self.deleting {
            self.quitting = true;
            self.notice = Some("the removal has to finish — closing the moment it does".to_owned());
            return Effect::None;
        }
        Effect::Quit
    }

    /// Whether a quit that was held back by a removal can be honoured now.
    #[must_use]
    pub fn wants_to_quit(&self) -> bool {
        self.quitting && !self.deleting
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
                self.deleting = true;
                self.notice = Some(format!(
                    "removing {}…",
                    plural(pending.targets.len(), "directory", "directories")
                ));
                Effect::Delete(pending.targets)
            }
        }
    }

    /// `x`: hand the marked batch out to be planned.
    fn commit(&mut self) -> Effect {
        if self.deleting {
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
    #[must_use]
    pub fn batch(&self) -> Vec<Target> {
        let mut batch = Vec::new();
        let mut stack: Vec<NodeId> = self.marks.iter().copied().collect();
        while let Some(id) = stack.pop() {
            if !self.shown(id) {
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
            self.notice = Some(if running.is_empty() {
                "everything under here already carries a price".to_owned()
            } else {
                format!(
                    "{} under here is already being priced",
                    plural(running.len(), "directory", "directories")
                )
            });
            return Effect::None;
        }
        self.pricing.extend(waiting.iter().cloned());
        self.notice = Some(format!(
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

    /// Whether a node survives the filter. Everything survives when there is not one.
    fn shown(&self, id: NodeId) -> bool {
        match &self.filter {
            Some(filter) => filter.rolls.contains_key(&id),
            None => true,
        }
    }

    // ---- marking ----------------------------------------------------------------------

    /// `space`: mark the cursor's subtree, or unmark it.
    fn toggle_mark(&mut self) {
        if let Some(row) = self.row() {
            self.mark_at(row.id);
        }
    }

    /// Marking one row, whichever door reached it — the key or the box under the pointer.
    fn mark_at(&mut self, id: NodeId) {
        if self.covered(id) {
            self.unmark(id);
        } else {
            self.mark(id);
        }
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
    }

    fn add_mark(&mut self, id: NodeId) {
        if !self.marks.insert(id) {
            return;
        }
        let mut at = self.tree.node(id).parent;
        while let Some(current) = at {
            *self.below.entry(current).or_insert(0) += 1;
            at = self.tree.node(current).parent;
        }
    }

    fn drop_mark(&mut self, id: NodeId) {
        if !self.marks.remove(&id) {
            return;
        }
        let mut at = self.tree.node(id).parent;
        while let Some(current) = at {
            if let Some(count) = self.below.get_mut(&current) {
                *count = count.saturating_sub(1);
            }
            at = self.tree.node(current).parent;
        }
    }

    /// Rebuilds the partial-state counts from the marks that are left.
    ///
    /// From scratch rather than adjusted, because this runs after marks have been dropped
    /// wholesale by a deletion and a count that drifted would leave a partial marker on a
    /// row with nothing marked under it.
    fn recount_marks(&mut self) {
        self.below.clear();
        for id in self.marks.clone() {
            let mut at = self.tree.node(id).parent;
            while let Some(current) = at {
                *self.below.entry(current).or_insert(0) += 1;
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
    use crate::fixture::hit;
    use crate::size::Size;
    use crate::tree::{Order, Sort, Tree};
    use std::path::{Path, PathBuf};

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

        view.removed(Path::new("/scan/nx/packages/ui/node_modules"), true);
        view.sync();

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
        view.removed(Path::new("/scan/old/target"), true);
        view.sync();

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

        view.removed(Path::new("/scan/old/target"), false);
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

        view.removed(Path::new("/scan/old/target"), true);
        view.sync();

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
        view.removed(Path::new("/scan/old/target"), true);
        view.sync();
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
        view.removed(Path::new("/scan/old/target"), true);
        view.sync();

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
        view.repriced(&[claim], "priced 1 directory".to_owned());
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
            "the pricing went away".to_owned(),
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
            "priced 1 directory".to_owned(),
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
        view.deleted("removed 10 B from 1 directory".to_owned());
        assert!(view.wants_to_quit());
    }

    #[test]
    fn a_view_that_was_never_asked_to_quit_does_not_want_to() {
        let mut view = view();
        assert!(!view.wants_to_quit());
        view.ask(pending(&["/scan/old/target"]));
        view.apply(Action::Highlight(Turn::Next));
        view.apply(Action::Answer);
        view.deleted("removed 10 B from 1 directory".to_owned());
        assert!(!view.wants_to_quit());
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
}
