//! The parallel walker: one pass over a tree, pruning at every directory it claims.
//!
//! ## Prune on match
//!
//! When a rule claims a directory the walker records it and returns [`WalkState::Skip`]. That
//! single decision is the performance thesis. npkill walks *into* `node_modules` to size it,
//! enumerating tens of thousands of inodes through its full scan pipeline to produce one
//! number the user is about to discard by deleting the tree. Here the scan stops at the
//! boundary and the subtree, if it is measured at all, is handed to the tight loop in
//! [`crate::size`].
//!
//! ## Why every ignore file is switched off
//!
//! [`ignore`] is here for two things: the parallel walk, and the gitignore stack that tier
//! two needs. Tier one must not use the second. `node_modules`, `target` and `.venv` are
//! gitignored in every repo that has a `.gitignore`, so a walk with the default filtering on
//! would find almost nothing — and `hidden(false)` matters for the same reason, since
//! `.venv`, `.gradle`, `.nx` and `.build` all start with a dot. Tier two therefore brings its
//! own matcher, and asks it per path rather than letting it steer the walk. See
//! [`crate::fallback`].
//!
//! ## The two tiers, in order
//!
//! Tier one is asked first at every directory, and it prunes. That ordering *is* tier two's
//! fourth condition, "no tier-one rule already claimed it": there is no separate check for it
//! anywhere, and there does not need to be.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use ignore::{DirEntry, WalkBuilder, WalkState};

use crate::detect::Detector;
use crate::fallback::{DEFAULT_MIN_SIZE, Fallback, FallbackReport};
use crate::rules::{Rule, Ruleset};
use crate::size::{Measurer, Size, SizeMode};
use crate::tree::Tree;

/// Why a directory is reclaimable, and what is known about getting it back.
#[derive(Debug, Clone)]
pub enum Claim {
    /// Tier one: a marker-anchored rule recognised the project and named this directory as its
    /// output.
    Rule(RuleClaim),
    /// Tier two: nothing in the ruleset knows this directory, but git does.
    Ignored(IgnoredClaim),
}

/// A claim made by the curated ruleset.
#[derive(Debug, Clone)]
pub struct RuleClaim {
    /// The rule that matched, carrying its ecosystem label and any caveat.
    pub rule: Arc<Rule>,
    /// The project whose markers justified the claim.
    pub project_root: PathBuf,
    /// The concrete command that brings this directory back, resolved for this project — so
    /// `pnpm install` rather than the rule's "npm ci / pnpm install / yarn" when a
    /// `pnpm-lock.yaml` is what the project actually has.
    pub regenerate: String,
}

/// A claim made by the tier-two gitignore fallback.
///
/// There is no regeneration command here, and that is the point rather than an omission: this
/// tier knows the directory is safe to remove and knows nothing whatever about what put it
/// there. The asymmetry against tier one is information — it tells the user which deletions are
/// cheap and which are a leap.
#[derive(Debug, Clone)]
pub struct IgnoredClaim {
    /// The git work tree whose ignore stack and index justified the claim.
    pub work_tree: PathBuf,
}

/// One reclaimable directory.
#[derive(Debug, Clone)]
pub struct Hit {
    /// The directory itself.
    pub path: PathBuf,
    /// Which tier claimed it, and everything that tier knows.
    pub claim: Claim,
    /// What is known about the size. For a tier-one claim, [`Size::Unmeasured`] unless a
    /// breakdown was asked for, because measuring means enumerating the subtree the scan
    /// deliberately pruned at. A tier-two claim always carries a real number: it could not have
    /// been claimed without a full pass over it, so there was nothing left to save.
    pub size: Size,
    /// The directory's own mtime. The best single proxy for "do I still need this".
    pub modified: Option<SystemTime>,
}

impl Hit {
    /// How long ago the directory was last touched, or `None` if the clock disagrees with
    /// the filesystem.
    #[must_use]
    pub fn age(&self, now: SystemTime) -> Option<std::time::Duration> {
        now.duration_since(self.modified?).ok()
    }

    /// The command that brings this directory back, when anything knows one.
    ///
    /// `None` for a tier-two hit. See [`IgnoredClaim`] for why that gap is worth carrying all
    /// the way to the user rather than papering over.
    #[must_use]
    pub fn regenerate(&self) -> Option<&str> {
        match &self.claim {
            Claim::Rule(claim) => Some(&claim.regenerate),
            Claim::Ignored(_) => None,
        }
    }

    /// The rule that claimed this directory, or `None` when no rule did.
    #[must_use]
    pub fn rule(&self) -> Option<&Rule> {
        match &self.claim {
            Claim::Rule(claim) => Some(&claim.rule),
            Claim::Ignored(_) => None,
        }
    }
}

/// Something the walk could not read. Collected rather than fatal: one unreadable directory
/// must not cost the user the rest of the scan.
#[derive(Debug)]
pub struct WalkError {
    /// The path involved, when the error names one.
    pub path: Option<PathBuf>,
    /// What went wrong.
    pub message: String,
}

/// What a walk found.
#[derive(Debug, Default)]
pub struct WalkOutcome {
    /// How many directories were claimed, across both tiers.
    pub hits: usize,
    /// The total size of the claims that were measured. Zero on a default scan, which
    /// measures nothing — read it alongside `unmeasured` rather than on its own.
    pub reclaimable_bytes: u64,
    /// How many claims were recorded without being measured, because the scan pruned there.
    pub unmeasured: usize,
    /// What tier two managed. Read it: a scan of a directory outside any git work tree finds
    /// nothing through this tier and *cannot*, and the report is what tells the two apart.
    pub fallback: FallbackReport,
    /// Everything that could not be read.
    pub errors: Vec<WalkError>,
}

/// A configured scan of one tree.
#[derive(Debug, Clone)]
pub struct Walker {
    root: PathBuf,
    ruleset: Arc<Ruleset>,
    threads: Option<usize>,
    max_depth: Option<usize>,
    follow_links: bool,
    same_file_system: bool,
    size_mode: SizeMode,
    fallback: bool,
    min_size: u64,
}

impl Walker {
    /// A walk of `root` under `ruleset`, with the defaults the safety model asks for:
    /// symlinks are not followed and mount points are not crossed.
    ///
    /// The tier-two gitignore fallback is on, at the default floor. It is safe on by default
    /// because it never claims a directory holding a tracked file, and it is inert outside a
    /// git work tree.
    #[must_use]
    pub fn new(root: impl AsRef<Path>, ruleset: Arc<Ruleset>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            ruleset,
            threads: None,
            max_depth: None,
            follow_links: false,
            same_file_system: true,
            size_mode: SizeMode::default(),
            fallback: true,
            min_size: DEFAULT_MIN_SIZE,
        }
    }

    /// How many threads to walk with. Defaults to the machine's parallelism.
    #[must_use]
    pub fn threads(mut self, threads: usize) -> Self {
        self.threads = Some(threads);
        self
    }

    /// How deep to descend below the root, unbounded by default.
    #[must_use]
    pub fn max_depth(mut self, max_depth: Option<usize>) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// Whether to follow symlinks. Off by default: a followed link leaves the root, and the
    /// deleter will not remove anything it cannot prove is under it.
    #[must_use]
    pub fn follow_links(mut self, follow_links: bool) -> Self {
        self.follow_links = follow_links;
        self
    }

    /// Whether to stay on one filesystem. On by default.
    #[must_use]
    pub fn same_file_system(mut self, same_file_system: bool) -> Self {
        self.same_file_system = same_file_system;
        self
    }

    /// How hard to work for each claim's size.
    #[must_use]
    pub fn size_mode(mut self, size_mode: SizeMode) -> Self {
        self.size_mode = size_mode;
        self
    }

    /// Whether to run the tier-two gitignore fallback. On by default.
    #[must_use]
    pub fn fallback(mut self, fallback: bool) -> Self {
        self.fallback = fallback;
        self
    }

    /// The size floor a tier-two claim must clear, [`DEFAULT_MIN_SIZE`] by default.
    ///
    /// It applies to tier two only. A rule that names a directory has already said the
    /// directory is output, and an empty `node_modules` is still a `node_modules`.
    #[must_use]
    pub fn min_size(mut self, min_size: u64) -> Self {
        self.min_size = min_size;
        self
    }

    /// Runs the walk, calling `on_hit` from a walker thread as each claim is found.
    ///
    /// `on_hit` is called concurrently from several threads and while the walk is still
    /// running — that is the point, since the TUI renders rows as they arrive. It must not
    /// block for long, or it becomes the walk's bottleneck.
    pub fn run<F>(&self, on_hit: F) -> WalkOutcome
    where
        F: Fn(Hit) + Send + Sync,
    {
        let fallback = self
            .fallback
            .then(|| Fallback::new(&self.root, self.min_size));
        let scan = Scan {
            root: self.root.as_path(),
            detector: self.ruleset.detector(),
            measurer: Measurer::new(self.size_mode).same_file_system(self.same_file_system),
            min_size: self.min_size,
            on_hit,
            errors: Mutex::new(Vec::new()),
            hits: AtomicUsize::new(0),
            fallback_hits: AtomicUsize::new(0),
            holding_a_checkout: AtomicUsize::new(0),
            reclaimed: AtomicU64::new(0),
            unmeasured: AtomicUsize::new(0),
        };

        let threads = self.threads.unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
        });

        let mut builder = WalkBuilder::new(self.root.as_path());
        builder
            .hidden(false)
            .parents(false)
            .ignore(false)
            .git_global(false)
            .git_ignore(false)
            .git_exclude(false)
            .follow_links(self.follow_links)
            .same_file_system(self.same_file_system)
            .threads(threads)
            .max_depth(self.max_depth)
            // Git's object store is large, never reclaimable, and full of names that would
            // waste marker probes.
            .filter_entry(|entry| entry.file_name() != OsStr::new(".git"));

        builder.build_parallel().run(|| {
            // Per-thread, because tier two's matchers mutate as they learn and sharing one
            // would mean a lock on the hottest path in the scan.
            let mut tier_two = fallback.as_ref().map(Fallback::thread);
            let scan = &scan;
            Box::new(move |result| scan.visit(tier_two.as_mut(), result))
        });

        let mut errors = std::mem::take(&mut *lock(&scan.errors));
        let fallback_hits = scan.fallback_hits.load(Ordering::Relaxed);
        let fallback = match &fallback {
            Some(fallback) => {
                let (report, mut inert) = fallback.finish(
                    fallback_hits,
                    scan.holding_a_checkout.load(Ordering::Relaxed),
                );
                errors.append(&mut inert);
                report
            }
            None => FallbackReport {
                min_size: self.min_size,
                ..FallbackReport::default()
            },
        };

        WalkOutcome {
            hits: scan.hits.load(Ordering::Relaxed),
            reclaimable_bytes: scan.reclaimed.load(Ordering::Relaxed),
            unmeasured: scan.unmeasured.load(Ordering::Relaxed),
            fallback,
            errors,
        }
    }

    /// Runs the walk and files every hit into a rollup tree.
    ///
    /// The tree is correct at every moment, so a caller that wants to render while scanning
    /// can build the same thing itself around [`Walker::run`] and read the shared tree
    /// between updates.
    #[must_use]
    pub fn run_to_tree(&self) -> (Tree, WalkOutcome) {
        let tree = Mutex::new(Tree::new(&self.root));
        let stray = Mutex::new(Vec::new());

        let mut outcome = self.run(|hit| {
            let path = hit.path.clone();
            if lock(&tree).insert(hit).is_none() {
                lock(&stray).push(WalkError {
                    path: Some(path),
                    message: "claimed directory is not under the scan root".to_owned(),
                });
            }
        });

        outcome.errors.append(&mut lock(&stray));
        let tree = tree.into_inner().unwrap_or_else(PoisonError::into_inner);
        (tree, outcome)
    }
}

/// Everything one walk shares across its threads. Split out so the per-thread visitor closure
/// can capture a single reference rather than a dozen.
struct Scan<'a, F> {
    root: &'a Path,
    detector: &'a Detector,
    measurer: Measurer,
    /// The floor a tier-two claim has to clear. Tier one is exempt: a rule that names a
    /// directory has already said it is output.
    min_size: u64,
    on_hit: F,
    errors: Mutex<Vec<WalkError>>,
    hits: AtomicUsize,
    fallback_hits: AtomicUsize,
    holding_a_checkout: AtomicUsize,
    reclaimed: AtomicU64,
    unmeasured: AtomicUsize,
}

impl<F> Scan<'_, F>
where
    F: Fn(Hit) + Send + Sync,
{
    /// Judges one entry of the walk.
    fn visit(
        &self,
        tier_two: Option<&mut crate::fallback::Thread<'_>>,
        result: Result<DirEntry, ignore::Error>,
    ) -> WalkState {
        let entry = match result {
            Ok(entry) => entry,
            Err(err) => {
                self.fail(error_path(&err), err.to_string());
                return WalkState::Continue;
            }
        };

        // The root itself is never a claim: there would be no parent inside the scan
        // to carry the markers, and pruning it would end the walk.
        if entry.depth() == 0 {
            return WalkState::Continue;
        }
        let Some(file_type) = entry.file_type() else {
            return WalkState::Continue;
        };
        // Symlinks stay in the running for Bazel's `bazel-*`, which are links.
        if !file_type.is_dir() && !file_type.is_symlink() {
            return WalkState::Continue;
        }

        // Tier one first, and tier one prunes. That is what enforces tier two's fourth
        // condition, and it is why a gitignored `node_modules` keeps its `pnpm install`
        // instead of becoming an anonymous pile of bytes.
        let claim = if let Some(rule) = self.detector.detect(entry.path(), self.root, entry.depth())
        {
            Claim::Rule(rule)
        } else {
            // Tier two judges directories only. A symlink holds nothing, and its bytes live
            // outside the tree.
            let judged = tier_two
                .filter(|_| file_type.is_dir())
                .and_then(|tier_two| tier_two.judge(entry.path()));
            match judged {
                Some(ignored) => Claim::Ignored(ignored),
                None => return WalkState::Continue,
            }
        };

        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(err) => {
                self.fail(Some(entry.path().to_path_buf()), err.to_string());
                return WalkState::Skip;
            }
        };

        let size = match &claim {
            Claim::Rule(_) => {
                let measured = self.measurer.measure(entry.path(), &metadata);
                self.report_blind_spots(
                    measured.unreadable,
                    "unreadable, so this size is a lower bound",
                );
                measured.size
            }
            Claim::Ignored(_) => {
                let surveyed = self.measurer.survey(entry.path(), &metadata);
                let blind_spots =
                    !surveyed.unreadable.is_empty() || !surveyed.not_crossed.is_empty();
                self.report_blind_spots(
                    surveyed.unreadable,
                    "unreadable, so this subtree could not be judged reclaimable",
                );
                self.report_blind_spots(
                    surveyed.not_crossed,
                    "on another filesystem, so this subtree could not be judged reclaimable",
                );
                if blind_spots {
                    // Part of the subtree could not be read, so neither "holds no checkout" nor
                    // the size is established — both are claims about the whole of it. Tier one
                    // can live with a lower bound because a rule already vouched for the
                    // directory; here the traversal *is* the evidence, and unjudgeable ground
                    // is left alone. The paths went into the errors above, so the user can see
                    // why a directory they expected did not appear.
                    return WalkState::Continue;
                }
                if surveyed.nested_repo.is_some() {
                    // Somebody's checkout lives in here, so this directory is not a single
                    // thing to be removed. `git clean` descends past it rather than collapsing
                    // it, and so do we — subdirectories with no checkout under them are still
                    // claimable on their own.
                    self.holding_a_checkout.fetch_add(1, Ordering::Relaxed);
                    return WalkState::Continue;
                }
                if surveyed
                    .size
                    .bytes()
                    .is_none_or(|bytes| bytes < self.min_size)
                {
                    // Below the floor, so not a claim — but keep descending. A rule may still
                    // match deeper, and an ignored directory that holds a tracked file can
                    // still have reclaimable subdirectories under it that do not.
                    return WalkState::Continue;
                }
                surveyed.size
            }
        };

        self.hits.fetch_add(1, Ordering::Relaxed);
        if matches!(claim, Claim::Ignored(_)) {
            self.fallback_hits.fetch_add(1, Ordering::Relaxed);
        }
        match size.bytes() {
            Some(bytes) => {
                self.reclaimed.fetch_add(bytes, Ordering::Relaxed);
            }
            None => {
                self.unmeasured.fetch_add(1, Ordering::Relaxed);
            }
        }
        (self.on_hit)(Hit {
            path: entry.into_path(),
            claim,
            size,
            modified: metadata.modified().ok(),
        });

        // The whole thesis, in one line: what we have claimed, we do not enumerate.
        WalkState::Skip
    }

    fn fail(&self, path: Option<PathBuf>, message: String) {
        lock(&self.errors).push(WalkError { path, message });
    }

    /// Reports the corners of a subtree a traversal did not see. The message differs by tier
    /// and is the point of the report: a tier-one claim survives a blind spot with a size that
    /// is a lower bound, and a tier-two claim does not survive one at all.
    fn report_blind_spots(&self, paths: Vec<PathBuf>, message: &str) {
        if paths.is_empty() {
            return;
        }
        let mut errors = lock(&self.errors);
        for path in paths {
            errors.push(WalkError {
                path: Some(path),
                message: message.to_owned(),
            });
        }
    }
}

/// Digs the path out of a walk error. `ignore` wraps the underlying failure in `WithPath` and
/// `WithDepth` layers rather than exposing an accessor, so unwrap them by hand.
fn error_path(err: &ignore::Error) -> Option<PathBuf> {
    match err {
        ignore::Error::WithPath { path, .. } => Some(path.clone()),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            error_path(err)
        }
        ignore::Error::Loop { child, .. } => Some(child.clone()),
        _ => None,
    }
}

/// Locking helper. A poisoned mutex here means a panic in `on_hit`, which has already been
/// reported to whoever wrote it; losing the errors collected so far on top of that would
/// help nobody.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
