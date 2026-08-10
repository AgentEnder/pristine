//! The rollup tree: the filesystem tree pruned to paths that lead to something reclaimable,
//! with each node carrying the bytes recoverable beneath it.
//!
//! A node's number is not "how big is this directory" — that is `dua`'s question — but "how
//! much would I get back by emptying this subtree". A source directory with nothing
//! reclaimable under it never appears.
//!
//! The rollup is a post-order sum, accumulated on the way down rather than in a second pass.
//! It can be, because the only nodes carrying weight are the claims, claims are always leaves
//! (the walk prunes there), and nothing is ever removed. So totals are correct after every
//! insert, which is what lets the TUI render a partial tree while the scan is still running.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

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
    /// Measured reclaimable bytes in this subtree, this node included.
    pub reclaimable: u64,
    /// How many claims in this subtree were recorded but not measured, because the scan
    /// pruned at them. A node with `reclaimable == 0` and `unmeasured > 0` is not empty; it
    /// is unpriced, and a breakdown is what puts a number on it.
    pub unmeasured: usize,
    /// Set when this node is itself a claimed directory, in which case it has no children.
    pub hit: Option<Hit>,
    /// Children, in insertion order until [`Tree::sort_by_reclaimable`] is called.
    pub children: Vec<NodeId>,
}

/// The pruned filesystem tree produced by a walk.
#[derive(Debug)]
pub struct Tree {
    nodes: Vec<Node>,
    /// `(parent, name) -> child`, so inserting into a directory with thousands of children
    /// stays linear in the number of hits rather than quadratic.
    index: HashMap<(NodeId, OsString), NodeId>,
    root: PathBuf,
}

impl Tree {
    /// An empty tree rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let node = Node {
            name: root.as_os_str().to_os_string(),
            path: root.clone(),
            reclaimable: 0,
            unmeasured: 0,
            hit: None,
            children: Vec::new(),
        };
        Self {
            nodes: vec![node],
            index: HashMap::new(),
            root,
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

        let mut parent = self.root();
        let mut path = self.root.clone();
        self.nodes[parent].reclaimable += bytes;
        self.nodes[parent].unmeasured += unmeasured;

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
                    reclaimable: 0,
                    unmeasured: 0,
                    hit: None,
                    children: Vec::new(),
                });
                self.nodes[parent].children.push(id);
                self.index.insert((parent, name), id);
                id
            };
            self.nodes[id].reclaimable += bytes;
            self.nodes[id].unmeasured += unmeasured;
            parent = id;
        }

        self.nodes[parent].hit = Some(hit);
        Some(parent)
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
        let relative = path.strip_prefix(&self.root).ok()?;
        let mut current = self.root();
        for component in relative.components() {
            current = *self
                .index
                .get(&(current, component.as_os_str().to_os_string()))?;
        }
        Some(current)
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

    /// The number of nodes, the root included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the walk found nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.len() == 1
    }

    /// Sorts every node's children by reclaimable bytes, largest first, ties broken by name.
    ///
    /// Per level, because a tree and a global sort are not compatible: children have to stay
    /// under their parent, so the only ordering a tree can express is a sibling ordering.
    pub fn sort_by_reclaimable(&mut self) {
        for id in 0..self.nodes.len() {
            let mut children = std::mem::take(&mut self.nodes[id].children);
            children.sort_by(|&a, &b| {
                self.nodes[b]
                    .reclaimable
                    .cmp(&self.nodes[a].reclaimable)
                    .then_with(|| self.nodes[a].name.cmp(&self.nodes[b].name))
            });
            self.nodes[id].children = children;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Tree;
    use crate::rules::Ruleset;
    use crate::size::Size;
    use crate::walk::{Claim, Hit, RuleClaim};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn hit(path: &str, size: u64) -> Hit {
        let ruleset = Ruleset::builtin().unwrap();
        let rule = Arc::clone(&ruleset.rules()[0]);
        Hit {
            path: PathBuf::from(path),
            claim: Claim::Rule(RuleClaim {
                project_root: PathBuf::from("/scan"),
                regenerate: rule.regenerate.clone(),
                rule,
            }),
            size: Size::Measured(size),
            modified: None,
        }
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
}
