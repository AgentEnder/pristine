//! The rollup tree: the filesystem tree pruned to paths that lead to something reclaimable,
//! with each node carrying the bytes recoverable beneath it.
//!
//! A node's number is not "how big is this directory" — that is `dua`'s question — but "how
//! much would I get back by emptying this subtree". A source directory with nothing
//! reclaimable under it never appears.
//!
//! The rollup is a post-order sum, accumulated on the way down rather than in a second pass.
//! It can be, because the only nodes carrying weight are the claims and claims are always
//! leaves (the walk prunes there). So totals are correct after every insert, which is what lets
//! the TUI render a partial tree while the scan is still running.
//!
//! Nodes do leave, and only one way: [`Tree::remove`], when the deleter reports a claim gone.
//! Every rollup here is therefore a quantity that can be taken back out again — which is a
//! constraint on what may be rolled up rather than an incidental property. Sums subtract.
//! [`Node::modified`] does not, and is recomputed from what is left.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::size::Size;
use crate::walk::Hit;

/// A handle to a node. Stable for the life of the tree.
pub type NodeId = usize;

/// One directory on a path to something reclaimable.
#[derive(Debug)]
pub struct Node {
    /// The final path component. The root node carries the whole scan root instead.
    pub name: OsString,
    /// The full path.
    pub path: PathBuf,
    /// The directory this one is in, and `None` for the root.
    ///
    /// Carried rather than derived, because everything the front end does to a row is a
    /// statement about its ancestry: a mark covers a subtree, so "is this row marked" is a
    /// walk upwards, and so is putting a cursor back on the nearest surviving ancestor of a
    /// row the deleter has just taken away. Ten steps at this tree's real depth.
    pub parent: Option<NodeId>,
    /// Measured reclaimable bytes in this subtree, this node included.
    pub reclaimable: u64,
    /// How many claims in this subtree were recorded but not measured, because the scan
    /// pruned at them. A node with `reclaimable == 0` and `unmeasured > 0` is not empty; it
    /// is unpriced, and a breakdown is what puts a number on it.
    pub unmeasured: usize,
    /// How many claims are in this subtree, priced or not.
    ///
    /// The count a selection is stated in. Bytes alone cannot be, while a default scan
    /// prices 8% of what it finds: "41.2 GiB marked" over a tree that is mostly unpriced
    /// reads as a small selection, and "120 directories" is the part that is always true.
    pub claims: usize,
    /// The newest mtime anywhere in this subtree, which is what the age column reports.
    ///
    /// The *newest* rather than the oldest, because the question a row answers is "has
    /// anything under here been touched lately" — one file rebuilt this morning is what
    /// makes a subtree still in use, and an average or an oldest would hide it behind
    /// everything beside it that is stale.
    pub modified: Option<SystemTime>,
    /// Set when this node is itself a claimed directory, in which case it has no children.
    pub hit: Option<Hit>,
    /// Children, in insertion order until [`Tree::sort_by`] is called.
    pub children: Vec<NodeId>,
    /// Whether this node is still part of the tree. See [`Tree::remove`].
    ///
    /// Private, and the one field here that is: it is the tree's own bookkeeping rather than
    /// anything about the directory, and [`Tree::is_attached`] is the question worth asking.
    attached: bool,
}

/// How a level orders its children.
///
/// Per level, because a tree and a global sort are not compatible: children have to stay under
/// their parent, so the only ordering a tree can express is a sibling ordering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Order {
    /// Biggest subtree first — what the tool is for.
    #[default]
    Size,
    /// By name, which is the only order that does not move while the scan is running.
    Path,
    /// Stalest first: the subtree nothing has touched for longest, first.
    ///
    /// The opposite direction from [`Size`](Self::Size) and deliberately so — both put the
    /// best candidate for deletion at the top, and "sorted by age" meaning "newest first"
    /// would put the one directory you are still building at the top of a cleanup list.
    Age,
}

impl Order {
    /// Every order, in the sequence the sort key cycles through.
    pub const ALL: [Self; 3] = [Self::Size, Self::Path, Self::Age];

    /// The next one round.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Size => Self::Path,
            Self::Path => Self::Age,
            Self::Age => Self::Size,
        }
    }

    /// What the header calls it.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Size => "size",
            Self::Path => "path",
            Self::Age => "age",
        }
    }
}

/// An order and whether it is upside down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sort {
    /// Which key.
    pub by: Order,
    /// Whether to run it backwards.
    pub reverse: bool,
}

impl Sort {
    /// A sort on `by`, the right way up.
    #[must_use]
    pub fn by(by: Order) -> Self {
        Self { by, reverse: false }
    }
}

/// The pruned filesystem tree produced by a walk.
#[derive(Debug)]
pub struct Tree {
    nodes: Vec<Node>,
    /// `(parent, name) -> child`, so inserting into a directory with thousands of children
    /// stays linear in the number of hits rather than quadratic.
    index: HashMap<(NodeId, OsString), NodeId>,
    root: PathBuf,
    /// How many nodes are still attached to the root.
    ///
    /// Counted rather than read off `nodes.len()`, because [`Tree::remove`] detaches a node
    /// instead of taking it out of the arena — see there for why an id has to stay valid for
    /// the life of the tree even after the directory behind it is gone.
    live: usize,
}

impl Tree {
    /// An empty tree rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let node = Node {
            name: root.as_os_str().to_os_string(),
            path: root.clone(),
            parent: None,
            reclaimable: 0,
            unmeasured: 0,
            claims: 0,
            modified: None,
            hit: None,
            children: Vec::new(),
            attached: true,
        };
        Self {
            nodes: vec![node],
            index: HashMap::new(),
            root,
            live: 1,
        }
    }

    /// Files a hit, creating any missing ancestors and adding its bytes to each of them.
    ///
    /// Returns `None`, without changing the tree, if the hit is not under the root. The
    /// walker turns that into a reported error rather than dropping the hit silently.
    pub fn insert(&mut self, hit: Hit) -> Option<NodeId> {
        let relative = hit.path.strip_prefix(&self.root).ok()?;
        let bytes = hit.size.bytes().unwrap_or(0);
        let unmeasured = usize::from(hit.size.bytes().is_none());
        let modified = hit.modified;

        let mut parent = self.root();
        let mut path = self.root.clone();
        self.credit(parent, bytes, unmeasured, modified);

        for component in relative.components() {
            let name = component.as_os_str().to_os_string();
            path.push(&name);
            let id = if let Some(&id) = self.index.get(&(parent, name.clone())) {
                id
            } else {
                let id = self.nodes.len();
                self.nodes.push(Node {
                    name: name.clone(),
                    path: path.clone(),
                    parent: Some(parent),
                    reclaimable: 0,
                    unmeasured: 0,
                    claims: 0,
                    modified: None,
                    hit: None,
                    children: Vec::new(),
                    attached: true,
                });
                self.nodes[parent].children.push(id);
                self.index.insert((parent, name), id);
                self.live += 1;
                id
            };
            self.credit(id, bytes, unmeasured, modified);
            parent = id;
        }

        self.nodes[parent].hit = Some(hit);
        Some(parent)
    }

    /// Adds one claim's worth of everything to a node on its ancestor chain.
    fn credit(&mut self, id: NodeId, bytes: u64, unmeasured: usize, modified: Option<SystemTime>) {
        let node = &mut self.nodes[id];
        node.reclaimable += bytes;
        node.unmeasured += unmeasured;
        node.claims += 1;
        node.modified = node.modified.max(modified);
    }

    /// Puts a number on a claim that was filed without one, and adds it to every ancestor.
    ///
    /// This is the other half of streaming: a claim is published as soon as it is judged and
    /// priced afterwards, so the tree has to be able to learn a size for a node it already
    /// holds. The rollup stays correct after this as it does after [`Tree::insert`], because
    /// the ancestor chain is the same one the insert walked.
    ///
    /// Returns `None`, changing nothing, when the path is not a claim in this tree or already
    /// carries a size. Both would be a caller error rather than a fact about the filesystem,
    /// and pricing one claim twice would count its bytes twice — so it is refused rather than
    /// absorbed, exactly as an out-of-root insert is.
    pub fn price(&mut self, path: &Path, size: Size) -> Option<NodeId> {
        let bytes = size.bytes()?;
        // Resolved in full before anything is written, so a path that turns out not to be a
        // claim cannot leave half the chain updated.
        let chain = self.chain(path)?;
        let &current = chain.last()?;
        let hit = self.nodes[current].hit.as_mut()?;
        if hit.size.bytes().is_some() {
            return None;
        }
        hit.size = size;

        for id in chain {
            self.nodes[id].reclaimable += bytes;
            // Saturating because the alternative is a silent wrap to `usize::MAX` in release,
            // which would render as an enormous unpriced count. The guard above makes it
            // unreachable: the insert set exactly one on each of these nodes.
            self.nodes[id].unmeasured = self.nodes[id].unmeasured.saturating_sub(1);
        }
        Some(current)
    }

    /// Takes a claim out of the tree, with everything it was contributing to its ancestors.
    ///
    /// This is what a deletion is, seen from here, and it is the only way a node ever leaves.
    /// A directory that has been removed is not a row that should be marked, counted or
    /// cursored onto, and the front end learns each removal as the deleter finishes it — so
    /// the tree has to lose one claim at a time while a reader is looking at it.
    ///
    /// **An ancestor with nothing left beneath it goes too**, up to but never including the
    /// root. The tree's whole premise is that a path only appears because something
    /// reclaimable is under it; a directory that kept a row after its last claim was deleted
    /// would be the one row on screen that offers nothing.
    ///
    /// The node is *detached* rather than taken out of the arena, because [`NodeId`] promises
    /// to stay valid for the life of the tree and the front end holds ids across frames —
    /// marks, the expansion set, the cursor. Compacting would silently point every one of
    /// them at a different directory. The cost is one dead `Node` per deleted claim, which is
    /// bounded by what the reader deleted this session.
    ///
    /// Returns the nearest ancestor still standing, so a caller that was looking at the row
    /// has somewhere to look now. `None`, changing nothing, if the path is not a claim in
    /// this tree.
    pub fn remove(&mut self, path: &Path) -> Option<NodeId> {
        let chain = self.chain(path)?;
        let &id = chain.last()?;
        let hit = self.nodes[id].hit.as_ref()?;
        let bytes = hit.size.bytes().unwrap_or(0);
        let unmeasured = usize::from(hit.size.bytes().is_none());

        for &ancestor in &chain {
            let node = &mut self.nodes[ancestor];
            node.reclaimable -= bytes;
            node.unmeasured -= unmeasured;
            node.claims -= 1;
        }

        self.detach(id);
        // Upwards while each one is now an empty directory in a tree that only holds
        // non-empty ones. The root stays whatever happens: an empty scan is still a scan of
        // somewhere, and the header says where.
        let mut surviving = self.nodes[id].parent;
        while let Some(above) = surviving {
            if above == self.root()
                || !self.nodes[above].children.is_empty()
                || self.nodes[above].hit.is_some()
            {
                break;
            }
            self.detach(above);
            surviving = self.nodes[above].parent;
        }

        // The one rollup that cannot be subtracted: a maximum forgets everything that was
        // not the maximum, so the only way to know the newest mtime left under a node is to
        // ask what is left. Bottom-up, so each ancestor sees children that have already
        // answered.
        let surviving = surviving.unwrap_or_else(|| self.root());
        let mut at = Some(surviving);
        while let Some(id) = at {
            self.nodes[id].modified = self.freshest(id);
            at = self.nodes[id].parent;
        }
        Some(surviving)
    }

    /// The newest mtime under `id`, read off what is currently attached to it.
    fn freshest(&self, id: NodeId) -> Option<SystemTime> {
        let own = self.nodes[id].hit.as_ref().and_then(|hit| hit.modified);
        self.nodes[id]
            .children
            .iter()
            .map(|&child| self.nodes[child].modified)
            .fold(own, std::cmp::Ord::max)
    }

    /// Unhooks a node from its parent and from the path index. Its rollups are already out.
    fn detach(&mut self, id: NodeId) {
        let Some(parent) = self.nodes[id].parent else {
            return;
        };
        let name = self.nodes[id].name.clone();
        self.nodes[parent].children.retain(|&child| child != id);
        self.index.remove(&(parent, name));
        self.nodes[id].attached = false;
        self.live -= 1;
    }

    /// Every node from the root down to `path`, or `None` if the path is not in this tree.
    fn chain(&self, path: &Path) -> Option<Vec<NodeId>> {
        let relative = path.strip_prefix(&self.root).ok()?;
        let mut chain = vec![self.root()];
        let mut current = self.root();
        for component in relative.components() {
            current = *self
                .index
                .get(&(current, component.as_os_str().to_os_string()))?;
            chain.push(current);
        }
        Some(chain)
    }

    /// The root node's id.
    #[must_use]
    pub fn root(&self) -> NodeId {
        0
    }

    /// The node behind an id.
    ///
    /// # Panics
    ///
    /// If `id` did not come from this tree.
    #[must_use]
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    /// A node's children.
    ///
    /// # Panics
    ///
    /// If `id` did not come from this tree.
    #[must_use]
    pub fn children(&self, id: NodeId) -> &[NodeId] {
        &self.nodes[id].children
    }

    /// Looks a node up by path.
    #[must_use]
    pub fn find(&self, path: &Path) -> Option<NodeId> {
        self.chain(path)?.pop()
    }

    /// Whether this id still names a directory in the tree, rather than one the deleter has
    /// taken away. See [`Tree::remove`].
    ///
    /// # Panics
    ///
    /// If `id` did not come from this tree.
    #[must_use]
    pub fn is_attached(&self, id: NodeId) -> bool {
        self.nodes[id].attached
    }

    /// The scan root.
    #[must_use]
    pub fn root_path(&self) -> &Path {
        &self.root
    }

    /// Total measured bytes reclaimable anywhere under the root.
    #[must_use]
    pub fn reclaimable(&self) -> u64 {
        self.nodes[self.root()].reclaimable
    }

    /// How many claims in the whole tree have no size yet.
    #[must_use]
    pub fn unmeasured(&self) -> usize {
        self.nodes[self.root()].unmeasured
    }

    /// How many claims the whole tree holds.
    #[must_use]
    pub fn claims(&self) -> usize {
        self.nodes[self.root()].claims
    }

    /// The number of directories in the tree, the root included.
    ///
    /// What is still there: a claim the deleter has removed stops counting the moment it is
    /// reported, along with any ancestor it was the last thing under.
    #[must_use]
    pub fn len(&self) -> usize {
        self.live
    }

    /// How many [`NodeId`]s have ever been handed out.
    ///
    /// Every id below this has named a directory at some point and no id above it has, so a
    /// caller holding the value from a moment ago knows exactly which nodes appeared since:
    /// they are the ids in between. That is what lets the front end light a newly found row
    /// without the tree having to report arrivals, and it works because ids are minted in
    /// order and [`Tree::remove`] detaches rather than recycles.
    #[must_use]
    pub fn minted(&self) -> usize {
        self.nodes.len()
    }

    /// Whether there is nothing left to reclaim — either the walk found nothing, or
    /// everything it found has been deleted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.live == 1
    }

    /// Sorts every node's children, deepest-first order within each level.
    ///
    /// Per level, because a tree and a global sort are not compatible: children have to stay
    /// under their parent, so the only ordering a tree can express is a sibling ordering.
    ///
    /// Every order breaks its ties by name and every one of them is *total*, which matters
    /// more here than in a batch listing: the front end re-sorts as prices land, and two rows
    /// that compare equal under a sort that stopped at the key would swap places on an
    /// unrelated arrival — a row moving under the cursor for no reason a reader can see.
    pub fn sort_by(&mut self, sort: Sort) {
        for id in 0..self.nodes.len() {
            let mut children = std::mem::take(&mut self.nodes[id].children);
            children.sort_by(|&a, &b| {
                let (a, b) = if sort.reverse { (b, a) } else { (a, b) };
                let ordering = match sort.by {
                    Order::Size => self.nodes[b].reclaimable.cmp(&self.nodes[a].reclaimable),
                    Order::Path => std::cmp::Ordering::Equal,
                    // `None` is not "the beginning of time": it is a directory whose mtime
                    // could not be read, and putting it at the head of a list sorted by
                    // staleness would offer the one row nothing is known about as the best
                    // thing to delete. So unknown sorts last — and `S` does move it to the
                    // top, because reversing this sort reverses all of it, and the top of a
                    // newest-first list is where a row nobody should delete belongs anyway.
                    Order::Age => match (self.nodes[a].modified, self.nodes[b].modified) {
                        (Some(left), Some(right)) => left.cmp(&right),
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        (None, None) => std::cmp::Ordering::Equal,
                    },
                };
                ordering.then_with(|| self.nodes[a].name.cmp(&self.nodes[b].name))
            });
            self.nodes[id].children = children;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Order, Sort, Tree};
    use crate::fixture;
    use crate::size::Size;
    use crate::walk::Hit;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    /// A priced claim with no mtime — most of these tests are about the arithmetic.
    fn hit(path: &str, size: u64) -> Hit {
        Hit {
            modified: None,
            ..fixture::priced(path, size)
        }
    }

    /// A claim last touched `seconds` after an arbitrary epoch, so a test can talk about
    /// relative ages without a real clock.
    fn aged(path: &str, size: u64, seconds: u64) -> Hit {
        fixture::hit(path, Size::Measured(size), seconds)
    }

    fn names(tree: &Tree, at: &str) -> Vec<String> {
        tree.children(tree.find(Path::new(at)).unwrap())
            .iter()
            .map(|&id| tree.node(id).name.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn totals_are_correct_after_every_insert() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/a/node_modules", 100));
        assert_eq!(tree.reclaimable(), 100);
        tree.insert(hit("/scan/a/b/node_modules", 50));
        assert_eq!(tree.reclaimable(), 150);
        assert_eq!(
            tree.node(tree.find(Path::new("/scan/a")).unwrap())
                .reclaimable,
            150
        );
        assert_eq!(
            tree.node(tree.find(Path::new("/scan/a/b")).unwrap())
                .reclaimable,
            50
        );
    }

    #[test]
    fn a_hit_outside_the_root_is_refused_rather_than_absorbed() {
        let mut tree = Tree::new("/scan");
        assert!(tree.insert(hit("/elsewhere/node_modules", 100)).is_none());
        assert_eq!(tree.reclaimable(), 0);
        assert!(tree.is_empty());
    }

    #[test]
    fn an_intermediate_directory_is_created_once_however_many_hits_hang_off_it() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/repo/a/node_modules", 1));
        tree.insert(hit("/scan/repo/b/node_modules", 1));
        // root, repo, a, a/node_modules, b, b/node_modules
        assert_eq!(tree.len(), 6);
        assert_eq!(
            tree.children(tree.find(Path::new("/scan/repo")).unwrap())
                .len(),
            2
        );
    }

    #[test]
    fn every_ancestor_counts_the_claims_beneath_it() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/repo/a/node_modules", 1));
        tree.insert(hit("/scan/repo/b/node_modules", 1));
        tree.insert(hit("/scan/other/target", 1));
        assert_eq!(tree.claims(), 3);
        assert_eq!(
            tree.node(tree.find(Path::new("/scan/repo")).unwrap())
                .claims,
            2
        );
    }

    #[test]
    fn an_ancestors_age_is_the_newest_thing_under_it() {
        let mut tree = Tree::new("/scan");
        tree.insert(aged("/scan/repo/old/node_modules", 1, 100));
        tree.insert(aged("/scan/repo/new/node_modules", 1, 900));
        assert_eq!(
            tree.node(tree.find(Path::new("/scan/repo")).unwrap())
                .modified,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(900))
        );
    }

    #[test]
    fn removing_a_claim_takes_its_bytes_off_every_ancestor() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/repo/a/node_modules", 100));
        tree.insert(hit("/scan/repo/b/node_modules", 50));

        let surviving = tree.remove(Path::new("/scan/repo/a/node_modules")).unwrap();

        assert_eq!(tree.reclaimable(), 50);
        assert_eq!(tree.claims(), 1);
        assert_eq!(
            tree.node(tree.find(Path::new("/scan/repo")).unwrap())
                .reclaimable,
            50
        );
        // `a` held nothing else, so it went with its claim and `repo` is what is left.
        assert_eq!(tree.node(surviving).path, PathBuf::from("/scan/repo"));
        assert!(tree.find(Path::new("/scan/repo/a")).is_none());
        assert_eq!(names(&tree, "/scan/repo"), ["b"]);
    }

    #[test]
    fn a_directory_that_still_holds_something_survives_its_sibling_being_removed() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/repo/node_modules", 100));
        tree.insert(hit("/scan/repo/target", 50));

        let surviving = tree.remove(Path::new("/scan/repo/node_modules")).unwrap();

        assert_eq!(tree.node(surviving).path, PathBuf::from("/scan/repo"));
        assert_eq!(names(&tree, "/scan/repo"), ["target"]);
    }

    #[test]
    fn removing_the_last_claim_leaves_the_root_and_nothing_else() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/a/b/node_modules", 100));

        assert_eq!(
            tree.remove(Path::new("/scan/a/b/node_modules")),
            Some(tree.root())
        );

        assert!(tree.is_empty());
        assert_eq!(tree.len(), 1);
        assert_eq!(tree.reclaimable(), 0);
        assert!(tree.children(tree.root()).is_empty());
    }

    #[test]
    fn an_id_the_front_end_is_still_holding_says_it_is_gone_rather_than_naming_a_stranger() {
        let mut tree = Tree::new("/scan");
        let marked = tree.insert(hit("/scan/a/node_modules", 100)).unwrap();
        tree.insert(hit("/scan/b/node_modules", 50));

        tree.remove(Path::new("/scan/a/node_modules"));
        // The arena is not compacted, so the mark's id still resolves — to the directory it
        // always named, now detached, rather than to `b`'s claim.
        assert!(!tree.is_attached(marked));
        assert_eq!(
            tree.node(marked).path,
            PathBuf::from("/scan/a/node_modules")
        );
        assert!(tree.is_attached(tree.find(Path::new("/scan/b/node_modules")).unwrap()));
    }

    #[test]
    fn removing_the_newest_thing_under_a_directory_makes_it_older() {
        let mut tree = Tree::new("/scan");
        tree.insert(aged("/scan/repo/old/node_modules", 1, 100));
        tree.insert(aged("/scan/repo/new/node_modules", 1, 900));

        tree.remove(Path::new("/scan/repo/new/node_modules"));

        // A maximum cannot be subtracted, so this is the rollup that has to be re-asked
        // rather than adjusted. Left alone it would report a subtree as touched this morning
        // on the strength of a directory that no longer exists.
        assert_eq!(
            tree.node(tree.find(Path::new("/scan/repo")).unwrap())
                .modified,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(100))
        );
    }

    #[test]
    fn removing_something_that_is_not_a_claim_changes_nothing() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/repo/node_modules", 100));

        assert!(tree.remove(Path::new("/scan/repo")).is_none());
        assert!(tree.remove(Path::new("/scan/nowhere")).is_none());
        assert!(tree.remove(Path::new("/elsewhere/node_modules")).is_none());

        assert_eq!(tree.reclaimable(), 100);
        assert_eq!(tree.len(), 3);
    }

    #[test]
    fn a_price_still_lands_on_a_claim_beside_one_that_has_been_removed() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/a/node_modules", 0));
        let mut unpriced = hit("/scan/b/node_modules", 0);
        unpriced.size = Size::Unmeasured;
        tree.insert(unpriced);

        tree.remove(Path::new("/scan/a/node_modules"));
        tree.price(Path::new("/scan/b/node_modules"), Size::Measured(70));

        assert_eq!(tree.reclaimable(), 70);
        assert_eq!(tree.unmeasured(), 0);
    }

    #[test]
    fn each_order_sorts_within_its_own_level() {
        let mut tree = Tree::new("/scan");
        tree.insert(aged("/scan/small/node_modules", 1, 900));
        tree.insert(aged("/scan/large/node_modules", 10, 500));
        tree.insert(aged("/scan/medium/node_modules", 5, 100));

        tree.sort_by(Sort::by(Order::Size));
        assert_eq!(names(&tree, "/scan"), ["large", "medium", "small"]);

        tree.sort_by(Sort::by(Order::Path));
        assert_eq!(names(&tree, "/scan"), ["large", "medium", "small"]);

        tree.sort_by(Sort::by(Order::Age));
        assert_eq!(names(&tree, "/scan"), ["medium", "large", "small"]);

        tree.sort_by(Sort {
            by: Order::Size,
            reverse: true,
        });
        assert_eq!(names(&tree, "/scan"), ["small", "medium", "large"]);
    }

    #[test]
    fn a_directory_whose_age_is_unknown_is_never_the_top_candidate_for_deletion() {
        let mut tree = Tree::new("/scan");
        tree.insert(aged("/scan/dated/node_modules", 1, 100));
        tree.insert(hit("/scan/undated/node_modules", 1));

        // Stalest first, and "no idea" is not stale.
        tree.sort_by(Sort::by(Order::Age));
        assert_eq!(names(&tree, "/scan"), ["dated", "undated"]);
    }
}
