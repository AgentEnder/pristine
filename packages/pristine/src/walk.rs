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
//! two (#588) will need. Tier one must not use the second. `node_modules`, `target` and
//! `.venv` are gitignored in every repo that has a `.gitignore`, so a walk with the default
//! filtering on would find almost nothing — and `hidden(false)` matters for the same reason,
//! since `.venv`, `.gradle`, `.nx` and `.build` all start with a dot.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use ignore::{WalkBuilder, WalkState};

use crate::rules::{Rule, Ruleset};
use crate::size::{Measurer, Size, SizeMode};
use crate::tree::Tree;

/// One reclaimable directory.
#[derive(Debug, Clone)]
pub struct Hit {
    /// The directory itself.
    pub path: PathBuf,
    /// The project whose markers justified the claim.
    pub project_root: PathBuf,
    /// The rule that matched, carrying its ecosystem label and any caveat.
    pub rule: Arc<Rule>,
    /// The concrete command that brings this directory back, resolved for this project — so
    /// `pnpm install` rather than the rule's "npm ci / pnpm install / yarn" when a
    /// `pnpm-lock.yaml` is what the project actually has.
    pub regenerate: String,
    /// What is known about the size. [`Size::Unmeasured`] unless a breakdown was asked for,
    /// because measuring means enumerating the subtree the scan deliberately pruned at.
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
    /// How many directories were claimed.
    pub hits: usize,
    /// The total size of the claims that were measured. Zero on a default scan, which
    /// measures nothing — read it alongside `unmeasured` rather than on its own.
    pub reclaimable_bytes: u64,
    /// How many claims were recorded without being measured, because the scan pruned there.
    pub unmeasured: usize,
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
}

impl Walker {
    /// A walk of `root` under `ruleset`, with the defaults the safety model asks for:
    /// symlinks are not followed and mount points are not crossed.
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

    /// Runs the walk, calling `on_hit` from a walker thread as each claim is found.
    ///
    /// `on_hit` is called concurrently from several threads and while the walk is still
    /// running — that is the point, since the TUI renders rows as they arrive. It must not
    /// block for long, or it becomes the walk's bottleneck.
    pub fn run<F>(&self, on_hit: F) -> WalkOutcome
    where
        F: Fn(Hit) + Send + Sync,
    {
        let errors = Mutex::new(Vec::new());
        let hits = AtomicUsize::new(0);
        let reclaimed = AtomicU64::new(0);
        let unmeasured = AtomicUsize::new(0);
        let measurer = Measurer::new(self.size_mode).same_file_system(self.same_file_system);
        let detector = self.ruleset.detector();
        let root = self.root.as_path();

        let threads = self.threads.unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
        });

        let mut builder = WalkBuilder::new(root);
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
            Box::new(|result| {
                let entry = match result {
                    Ok(entry) => entry,
                    Err(err) => {
                        lock(&errors).push(WalkError {
                            path: error_path(&err),
                            message: err.to_string(),
                        });
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

                let Some(detection) = detector.detect(entry.path(), root, entry.depth()) else {
                    return WalkState::Continue;
                };

                let metadata = match entry.metadata() {
                    Ok(metadata) => metadata,
                    Err(err) => {
                        lock(&errors).push(WalkError {
                            path: Some(entry.path().to_path_buf()),
                            message: err.to_string(),
                        });
                        return WalkState::Skip;
                    }
                };

                let sized = measurer.measure(entry.path(), &metadata);
                if !sized.unreadable.is_empty() {
                    let mut collected = lock(&errors);
                    for path in sized.unreadable {
                        collected.push(WalkError {
                            path: Some(path),
                            message: "unreadable, so this size is a lower bound".to_owned(),
                        });
                    }
                }

                hits.fetch_add(1, Ordering::Relaxed);
                match sized.size.bytes() {
                    Some(bytes) => {
                        reclaimed.fetch_add(bytes, Ordering::Relaxed);
                    }
                    None => {
                        unmeasured.fetch_add(1, Ordering::Relaxed);
                    }
                }
                on_hit(Hit {
                    path: entry.into_path(),
                    project_root: detection.project_root,
                    rule: detection.rule,
                    regenerate: detection.regenerate,
                    size: sized.size,
                    modified: metadata.modified().ok(),
                });

                // The whole thesis, in one line: what we have claimed, we do not enumerate.
                WalkState::Skip
            })
        });

        WalkOutcome {
            hits: hits.load(Ordering::Relaxed),
            reclaimable_bytes: reclaimed.load(Ordering::Relaxed),
            unmeasured: unmeasured.load(Ordering::Relaxed),
            errors: std::mem::take(&mut lock(&errors)),
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
