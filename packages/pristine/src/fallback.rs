//! Tier two: the gitignore fallback, for the ecosystems nobody wrote a rule for.
//!
//! This is the differentiator from `kondo`, whose coverage is exactly its ruleset and which is
//! therefore blind to anything outside it. Inside a git work tree a directory is reclaimable by
//! *inference* when all four of these hold:
//!
//! 1. it is ignored, per the whole gitignore stack — nested files, negations, `info/exclude`,
//!    global excludes — and not merely per the root `.gitignore`;
//! 2. it contains no tracked file at any depth;
//! 3. it clears a size floor, 10 MiB by default;
//! 4. no tier-one rule already claimed it.
//!
//! Condition two is the safety property, it is exactly the guarantee `git clean` enforces, and
//! it is the reason this tier can be on by default without being reckless. Condition four falls
//! out of evaluation order in [`crate::walk`]: tier one is asked first, and it prunes.
//!
//! ## The fifth condition, which `git clean` also enforces
//!
//! A directory holding a git checkout at any depth is not claimed either. This is not in the
//! four above and it is not optional: `git clean -ndX` in a repository whose ignored
//! `.sandboxes/` holds live work trees prints `Would skip repository …` and then lists the
//! siblings one by one, rather than collapsing the directory into a single removal. Anything
//! that collapses it is offering to delete somebody's uncommitted work, and that shape —
//! checkouts parked under an ignored directory — is common rather than exotic. So a candidate
//! holding a checkout is refused and descended into, which is exactly what git does with it,
//! and its subdirectories that hold no checkout are claimed on their own.
//!
//! For the same reason a candidate with an unreadable corner is refused. "Holds no checkout" is
//! a claim about the whole subtree, and a traversal that could not see all of it has not made
//! that claim. Tier one survives an unreadable corner with a size that is a lower bound, because
//! a rule vouched for the directory; here the traversal *is* the evidence.
//!
//! ## Outside a work tree this tier is inert
//!
//! Deliberately, and it is reported rather than left to look like an empty result. With no
//! repository there is no ignore file that means anything, and the only signal left would be
//! the directory's name — which is precisely how a cleaner deletes somebody's source. `build/`
//! is a CMake project's hand-written source as often as it is output, and no amount of wanting
//! a broader tier makes a name into evidence.
//!
//! ## Why a query matcher rather than a filtering walk
//!
//! Tier one switches every ignore file off, because `node_modules`, `target` and `.venv` are
//! gitignored in every repository that has a `.gitignore` and a filtering walk would find
//! almost nothing. So tier two cannot ride on the walk's own filtering and instead asks
//! [`IncrementalIgnore`], which answers the same question for one path at a time and caches per
//! directory. One matcher per work tree per walker thread: the matchers hold mutable caches, so
//! sharing one would mean a lock on the hottest path in the scan.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use ignore::{IncrementalIgnore, WalkBuilder};

use crate::git::{self, WorkTree};
use crate::walk::{IgnoredClaim, WalkError};

/// The default size floor: below this a directory is not worth a row, and tier two would
/// otherwise report every scrap of ignored cache on the disk.
pub const DEFAULT_MIN_SIZE: u64 = 10 * 1024 * 1024;

/// What tier two was able to do, reported alongside what it found.
///
/// Inertness is a result, not an absence of one. A user who scanned a directory that is not in
/// a git work tree has to be able to tell "there was nothing reclaimable here" from "this tier
/// had nothing to work with", and the two look identical without this.
#[derive(Debug, Clone, Default)]
pub struct FallbackReport {
    /// Whether tier two ran at all.
    pub enabled: bool,
    /// The floor a directory had to clear, in bytes.
    pub min_size: u64,
    /// Git work trees whose ignore stack and index tier two consulted.
    pub work_trees: usize,
    /// Directories tier two passed over because they lie outside any git work tree.
    pub outside_work_tree: usize,
    /// Directories tier two would otherwise have claimed, but which hold a git checkout. Worth
    /// surfacing: this is where the reclaimable bytes a scan declined to offer went.
    pub holding_a_checkout: usize,
    /// Directories tier two claimed.
    pub hits: usize,
}

impl FallbackReport {
    /// Whether tier two ran and had nothing to work with, because no part of the scan was in a
    /// git work tree.
    ///
    /// A tier switched off is not inert; it was not asked.
    #[must_use]
    pub fn is_inert(&self) -> bool {
        self.enabled && self.work_trees == 0
    }
}

/// The state tier two shares across walker threads: the work trees it has opened, which cost a
/// subprocess each and must not be opened twice.
#[derive(Debug)]
pub(crate) struct Fallback {
    scan_root: PathBuf,
    /// The work tree containing the scan root, resolved once before the walk. Scanning a
    /// subdirectory of a checkout is ordinary, and the repository above it still has an
    /// opinion, so the search for it is the one search allowed to leave the scan root.
    root_work_tree: Option<PathBuf>,
    min_size: u64,
    /// Keyed by work tree root. The `OnceLock` is what makes two threads arriving at the same
    /// repository run `git ls-files` once between them rather than once each.
    trees: Mutex<HashMap<PathBuf, Arc<Opened>>>,
    outside_work_tree: AtomicUsize,
}

type Opened = OnceLock<Result<Arc<WorkTree>, Arc<str>>>;

impl Fallback {
    /// Prepares tier two for a scan of `scan_root`.
    pub(crate) fn new(scan_root: &Path, min_size: u64) -> Self {
        Self {
            scan_root: scan_root.to_path_buf(),
            root_work_tree: git::discover(scan_root),
            min_size,
            trees: Mutex::new(HashMap::new()),
            outside_work_tree: AtomicUsize::new(0),
        }
    }

    /// Per-thread state for one walker thread.
    pub(crate) fn thread(&self) -> Thread<'_> {
        Thread {
            shared: self,
            recent: None,
            matchers: HashMap::new(),
            trees: HashMap::new(),
        }
    }

    /// What tier two managed, and everything it could not consult.
    pub(crate) fn finish(
        &self,
        hits: usize,
        holding_a_checkout: usize,
    ) -> (FallbackReport, Vec<WalkError>) {
        let trees = lock(&self.trees);
        let mut work_trees = 0;
        let mut errors = Vec::new();
        for (root, opened) in trees.iter() {
            match opened.get() {
                Some(Ok(_)) => work_trees += 1,
                // A repository that would not answer is inert ground too, and inertness that
                // is not reported reads as "there was nothing here".
                Some(Err(message)) => errors.push(WalkError {
                    path: Some(root.clone()),
                    message: message.to_string(),
                }),
                None => {}
            }
        }
        let report = FallbackReport {
            enabled: true,
            min_size: self.min_size,
            work_trees,
            outside_work_tree: self.outside_work_tree.load(Ordering::Relaxed),
            holding_a_checkout,
            hits,
        };
        (report, errors)
    }
}

/// One walker thread's view of tier two.
///
/// Everything here is a cache. The matchers have to be per-thread because they mutate as they
/// learn, and the rest is per-thread because a shared map would mean taking a lock for every
/// directory in the scan.
#[derive(Debug)]
pub(crate) struct Thread<'a> {
    shared: &'a Fallback,
    /// The last directory whose work tree this thread resolved, and the answer. One slot is
    /// enough: the walk hands a thread the entries of one directory at a time, so consecutive
    /// questions almost always share a parent. It keeps the search for a work tree to a single
    /// `stat` per directory without a per-directory cache to pay for.
    recent: Option<(PathBuf, Option<PathBuf>)>,
    matchers: HashMap<PathBuf, IncrementalIgnore>,
    trees: HashMap<PathBuf, Option<Arc<WorkTree>>>,
}

impl Thread<'_> {
    /// Whether tier two claims `dir`, judged on everything that can be answered without
    /// touching the subtree. The walker applies the size floor afterwards, because that — and
    /// the nested-checkout rule alongside it — is what costs a traversal.
    pub(crate) fn judge(&mut self, dir: &Path) -> Option<IgnoredClaim> {
        let Some(work_tree) = self.work_tree_of(dir) else {
            self.shared
                .outside_work_tree
                .fetch_add(1, Ordering::Relaxed);
            return None;
        };
        // A repository that would not answer leaves this ground unjudgeable, and unjudgeable
        // means untouched.
        let tracked = self.tree(&work_tree)?;

        let relative = dir.strip_prefix(&work_tree).ok()?.to_path_buf();
        let matcher = self.matcher(&work_tree);
        if !matcher.matched(&relative, true).is_ignore() {
            return None;
        }

        // The safety property, and the one condition that costs somebody their work if it is
        // approximated rather than checked.
        if tracked.holds_tracked_path(dir) {
            return None;
        }

        Some(IgnoredClaim { work_tree })
    }

    /// The work tree containing `dir`, or `None` when it is not in one.
    fn work_tree_of(&mut self, dir: &Path) -> Option<PathBuf> {
        // `dir` itself may be a checkout inside another one, and git's rule is that the nearest
        // repository wins. This is the one `stat` tier two pays per directory.
        if git::is_work_tree_root(dir) {
            return Some(dir.to_path_buf());
        }
        let parent = dir.parent()?;
        if let Some((cached, answer)) = &self.recent
            && cached == parent
        {
            return answer.clone();
        }
        let answer = self.search_up(parent);
        self.recent = Some((parent.to_path_buf(), answer.clone()));
        answer
    }

    /// Walks up from `dir` for a work tree root, stopping at the scan root — above which the
    /// answer was resolved once, before the walk started.
    fn search_up(&self, dir: &Path) -> Option<PathBuf> {
        let mut cursor = Some(dir);
        while let Some(candidate) = cursor {
            if candidate == self.shared.scan_root || !candidate.starts_with(&self.shared.scan_root)
            {
                return self.shared.root_work_tree.clone();
            }
            if git::is_work_tree_root(candidate) {
                return Some(candidate.to_path_buf());
            }
            cursor = candidate.parent();
        }
        self.shared.root_work_tree.clone()
    }

    /// This thread's ignore matcher for `work_tree`, built on first use.
    fn matcher(&mut self, work_tree: &Path) -> &mut IncrementalIgnore {
        self.matchers
            .entry(work_tree.to_path_buf())
            .or_insert_with(|| build_matcher(work_tree))
    }

    /// The index of `work_tree`, opened once for the whole scan however many threads ask.
    fn tree(&mut self, work_tree: &Path) -> Option<Arc<WorkTree>> {
        if let Some(cached) = self.trees.get(work_tree) {
            return cached.clone();
        }
        let opened = {
            let mut trees = lock(&self.shared.trees);
            Arc::clone(trees.entry(work_tree.to_path_buf()).or_default())
        };
        let tree = opened
            .get_or_init(|| {
                WorkTree::open(work_tree)
                    .map(Arc::new)
                    .map_err(|err| Arc::from(err.to_string().as_str()))
            })
            .as_ref()
            .ok()
            .map(Arc::clone);
        self.trees.insert(work_tree.to_path_buf(), tree.clone());
        tree
    }
}

/// An ignore matcher for one work tree, configured to be git and nothing but git.
///
/// Every option here is a deliberate narrowing. `.ignore` and `.rgignore` files are ripgrep's,
/// not git's, and honouring them would mean claiming directories git has no opinion about.
/// `hidden` is ripgrep's "skip dotfiles", which would silently make every `.something`
/// reclaimable. `parents` would read `.gitignore` files above the work tree root, which git
/// does not do — and which, in a checkout inside another checkout, would let the outer
/// repository's rules mark the inner one's source as reclaimable.
fn build_matcher(work_tree: &Path) -> IncrementalIgnore {
    let mut builder = WalkBuilder::new(work_tree);
    builder
        .hidden(false)
        .parents(false)
        .ignore(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true);
    // One matcher per root the builder was given, and `WalkBuilder::new` takes exactly one, so
    // this cannot be empty.
    let mut matchers = builder.build_matchers();
    debug_assert_eq!(matchers.len(), 1, "one root in, one matcher out");
    matchers.remove(0)
}

/// Locking helper. A poisoned mutex here means a panic elsewhere in the walk, which has already
/// been reported; losing the work trees opened so far on top of that would help nobody.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::FallbackReport;

    #[test]
    fn a_tier_that_was_never_asked_is_not_inert() {
        let off = FallbackReport::default();
        assert!(!off.enabled);
        assert!(!off.is_inert());
    }

    #[test]
    fn a_tier_that_ran_and_found_no_work_tree_is_inert() {
        let report = FallbackReport {
            enabled: true,
            work_trees: 0,
            ..FallbackReport::default()
        };
        assert!(report.is_inert());
    }

    #[test]
    fn a_tier_that_found_a_work_tree_and_nothing_in_it_is_not_inert() {
        let report = FallbackReport {
            enabled: true,
            work_trees: 3,
            hits: 0,
            ..FallbackReport::default()
        };
        assert!(!report.is_inert());
    }
}
