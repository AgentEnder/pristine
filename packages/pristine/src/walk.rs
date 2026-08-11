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

use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use ignore::{DirEntry, WalkBuilder, WalkState};

use crate::detect::Detector;
use crate::fallback::{DEFAULT_MIN_SIZE, Fallback, FallbackReport};
use crate::rules::{Kind, Rule, Ruleset};
use crate::size::{Measurer, Size, SizeMode};
use crate::tree::Tree;

/// What tier two says in place of a label.
///
/// Not a blank and not a guess: the fallback knows the directory is safe to remove and knows
/// nothing whatever about what put it there, so it says exactly that. See [`IgnoredClaim`].
pub const UNLABELLED: &str = "Gitignored, kind unknown";

/// Why a directory is reclaimable, and what is known about it.
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
    /// The rule that matched, carrying the ecosystem, the kind and any caveat.
    pub rule: Arc<Rule>,
    /// The project whose markers justified the claim.
    pub project_root: PathBuf,
}

/// A claim made by the tier-two gitignore fallback.
///
/// Nothing here says what the directory is, and that is the point rather than an omission: this
/// tier knows the directory is safe to remove and knows nothing whatever about what put it
/// there. The asymmetry against tier one is information — a named row is a directory whose cost
/// to lose is known, and an unnamed one is a leap.
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

    /// What this directory is: the ecosystem and the kind, or [`UNLABELLED`] when only git
    /// knows the directory at all.
    ///
    /// A fact rather than a hint, which is the whole reason it replaced the command that used
    /// to sit here. "`node_modules` is Node Dependencies" is checked; "`npm install` brings it
    /// back" was a guess about a package manager, on a machine nothing here had looked at.
    #[must_use]
    pub fn label(&self) -> Cow<'_, str> {
        match &self.claim {
            Claim::Rule(claim) => Cow::Owned(claim.rule.label()),
            Claim::Ignored(_) => Cow::Borrowed(UNLABELLED),
        }
    }

    /// What kind of artefact this is, or `None` when only git knows the directory at all.
    ///
    /// The half of a label a machine can act on, which is what the closed vocabulary bought:
    /// "show me every cache" is a question the front end can answer, and the `None` is not a
    /// gap to be filled in but the tier-two claim's own content — see [`IgnoredClaim`].
    #[must_use]
    pub fn kind(&self) -> Option<Kind> {
        self.rule().map(|rule| rule.kind)
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

/// What a walk reports, as it happens.
///
/// Two events rather than one, because a claim and its price are found at different times and
/// waiting for the second would throw away the first. See [`Walker::run`].
#[derive(Debug)]
pub enum Found {
    /// A directory was claimed. Published the moment the claim is judged, whatever the size
    /// mode: nothing here ever waits for a measurement.
    Claim(Hit),
    /// A pricing thread has gone into this claim and has not come back yet.
    ///
    /// The pool is bounded, so the number of these outstanding at any instant is the number
    /// of threads in it — which is what makes it worth reporting at all. A live view can show
    /// exactly which of its dashes are being worked on *now*, where before it could only show
    /// that some of them would be worked on eventually. Followed by exactly one
    /// [`Found::Priced`] for the same path, whatever the measurement turns out to be.
    ///
    /// A consumer that only wants totals ignores it, as the command line does.
    Pricing(PathBuf),
    /// A claim that was published without a size now has one.
    ///
    /// Arrives after the [`Found::Claim`] it belongs to — always, because the claim is
    /// published before the pricing pool is even told about it — and on a different thread.
    Priced(Priced),
}

/// A price for a claim that was published without one.
#[derive(Debug, Clone)]
pub struct Priced {
    /// The claimed directory, spelled exactly as its [`Hit`] spelled it.
    pub path: PathBuf,
    /// What the traversal found.
    pub size: Size,
}

/// One claim waiting for the pricing pool.
///
/// The metadata travels with the path because the walk has already paid for it, and measuring
/// starts from the claim's own block count.
struct Job {
    path: PathBuf,
    metadata: std::fs::Metadata,
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

    /// Runs the walk, calling `on_found` as each claim is found and again as each is priced.
    ///
    /// `on_found` is called concurrently, from the walker threads and from the pricing pool,
    /// and while the walk is still running — that is the point, since the TUI renders rows as
    /// they arrive. It must not block for long, or it becomes the walk's bottleneck.
    ///
    /// ## Why a claim and its price are two events
    ///
    /// Pricing a claim means walking the subtree the scan just pruned at, and that is an order
    /// of magnitude more work than finding it. Measured over one real `~/repos`, 10,599
    /// claims, under a full breakdown:
    ///
    /// | | last claim published | run complete |
    /// |---|---|---|
    /// | priced on the walker thread | 60.1 s | 60.1 s |
    /// | priced on the pool | **7.5 s** | 63.0 s |
    ///
    /// Those two left-hand numbers are the whole change. Measuring on the walker thread makes
    /// every claim's *publication* wait behind its own measurement, so the listing completes
    /// only when the last byte has been counted and a front end has nothing whatever to render
    /// for a minute. That is npkill's bargain, and not making it is what the pruning was for.
    ///
    /// So a claim is published the moment it is judged, carrying [`Size::Unmeasured`], and is
    /// then handed to a pool of pricing threads. Its size arrives afterwards as
    /// [`Found::Priced`], naming the same path, and a consumer updates the row in place.
    ///
    /// **`run` still does not return until the pool has drained**, so every number in the
    /// returned [`WalkOutcome`] is final. A consumer that only wants totals — the command
    /// line, today — need not care that any of this happened.
    ///
    /// The pool is one thread per walker thread. Oversubscribing it is the obvious next idea
    /// and it was measured, because the deleter oversubscribes for exactly this reason: at
    /// four times the threads the same scan takes **85.8 s** and does not publish its last
    /// claim until 30.7 s. Pricing is `readdir` and `lstat`, which is 97% kernel time and
    /// contends; `unlink` and `rmdir` wait on the disk and do not. The conclusion from the
    /// deleter does not carry over here.
    pub fn run<F>(&self, on_found: F) -> WalkOutcome
    where
        F: Fn(Found) + Send + Sync,
    {
        let fallback = self
            .fallback
            .then(|| Fallback::new(&self.root, self.min_size));
        let scan = Scan {
            root: self.root.as_path(),
            detector: self.ruleset.detector(),
            measurer: Measurer::new(self.size_mode.clone()).same_file_system(self.same_file_system),
            min_size: self.min_size,
            on_found,
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
        // No pool at all under the default mode, which is the hot path and queues nothing.
        // Threads that could only ever block on an empty queue are not free, and a pool with
        // no work in it is harder to reason about than no pool.
        let pricers = if self.size_mode == SizeMode::Skip {
            0
        } else {
            threads
        };

        let builder = self.builder(threads);

        // Deliberately unbounded, and the bound that matters is on the POOL rather than on the
        // queue. A bounded queue is backpressure, and backpressure here means stalling the
        // walk until pricing catches up — which is exactly the wait this exists to remove. Over
        // `~/repos` any bound below the 10,599 claims would stretch a 4.6 s scan out to the
        // 56 s the pricing takes, and the last rows would reach the screen last. What it costs
        // instead is one path and one `stat` per claim not yet priced, which is strictly less
        // than the `Hit` the consumer is already holding for that same claim.
        let (submit, queue) = std::sync::mpsc::channel::<Job>();
        let queue = Mutex::new(queue);

        std::thread::scope(|pool| {
            for _ in 0..pricers {
                pool.spawn(|| scan.price(&queue));
            }

            // An inner scope so that every sender is dropped before the pool is joined: the
            // walk's clones go when `ignore` joins its own threads, and this one goes at the
            // closing brace. A pricing thread ends when the last sender is gone, not before.
            {
                let submit = submit;
                builder.build_parallel().run(|| {
                    // Per-thread, because tier two's matchers mutate as they learn and sharing
                    // one would mean a lock on the hottest path in the scan.
                    let mut tier_two = fallback.as_ref().map(Fallback::thread);
                    let scan = &scan;
                    let submit = submit.clone();
                    Box::new(move |result| scan.visit(tier_two.as_mut(), &submit, result))
                });
            }
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

    /// The parallel walk itself, configured to be a plain traversal.
    ///
    /// Every ignore source is off, and that is tier one's requirement rather than an
    /// oversight: `node_modules`, `target` and `.venv` are gitignored in every repository that
    /// has a `.gitignore`, so a filtering walk would find almost nothing. `hidden(false)` is
    /// the same point — `.venv`, `.gradle`, `.nx` and `.build` all start with a dot. Tier two
    /// brings its own matcher and queries it per path instead.
    fn builder(&self, threads: usize) -> WalkBuilder {
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
        builder
    }

    /// Runs the walk and files every hit into a rollup tree, pricing included.
    ///
    /// The tree is correct at every moment — after each claim and after each late price — so a
    /// caller that wants to render while scanning can build the same thing itself around
    /// [`Walker::run`] and read the shared tree between updates.
    #[must_use]
    pub fn run_to_tree(&self) -> (Tree, WalkOutcome) {
        let tree = Mutex::new(Tree::new(&self.root));
        let stray = Mutex::new(Vec::new());

        let mut outcome = self.run(|found| match found {
            Found::Claim(hit) => {
                let path = hit.path.clone();
                if lock(&tree).insert(hit).is_none() {
                    lock(&stray).push(WalkError {
                        path: Some(path),
                        message: "claimed directory is not under the scan root".to_owned(),
                    });
                }
            }
            // Nothing to file: it says a thread is busy, which a finished tree has no way to
            // be interested in. The live view is the only consumer that is.
            Found::Pricing(_) => {}
            // A price for a row the tree does not hold, or holds priced already, would be
            // double-counted rather than absorbed — so `price` refuses it and it is reported,
            // on the same rule as a claim from outside the root.
            Found::Priced(priced) => {
                if lock(&tree).price(&priced.path, priced.size).is_none() {
                    lock(&stray).push(WalkError {
                        path: Some(priced.path),
                        message: "priced directory is not an unpriced claim in this tree"
                            .to_owned(),
                    });
                }
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
    on_found: F,
    errors: Mutex<Vec<WalkError>>,
    hits: AtomicUsize,
    fallback_hits: AtomicUsize,
    holding_a_checkout: AtomicUsize,
    reclaimed: AtomicU64,
    unmeasured: AtomicUsize,
}

impl<F> Scan<'_, F>
where
    F: Fn(Found) + Send + Sync,
{
    /// Judges one entry of the walk.
    fn visit(
        &self,
        tier_two: Option<&mut crate::fallback::Thread<'_>>,
        submit: &std::sync::mpsc::Sender<Job>,
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

        // Whether this claim's size costs a traversal, and so belongs to the pricing pool
        // rather than to this thread. Decided before the claim is published, because it
        // decides what size the claim is published with.
        let queued =
            matches!(claim, Claim::Rule(_)) && self.measurer.traverses(entry.path(), &metadata);

        let size = match &claim {
            // Nothing is measured here when the pool is taking it: the claim goes out
            // unpriced and the number follows. What is left for this branch is the claim
            // whose size is free — a symlink, one `lstat` the walk already did — and the
            // claim no mode asked to price, which stays `Unmeasured`.
            Claim::Rule(_) if queued => Size::Unmeasured,
            Claim::Rule(_) => {
                let measured = self.measurer.measure(entry.path(), &metadata);
                self.report_blind_spots(
                    measured.unreadable,
                    "unreadable, so this size is a lower bound",
                );
                measured.size
            }
            Claim::Ignored(_) => match self.survey(entry.path(), &metadata) {
                Some(size) => size,
                // Refused, and always by descending rather than pruning: a rule may still match
                // deeper, and an ignored directory holding a tracked file can still have
                // reclaimable subdirectories under it that do not.
                None => return WalkState::Continue,
            },
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
        let path = entry.into_path();
        let queued = queued.then(|| path.clone());
        (self.on_found)(Found::Claim(Hit {
            path,
            claim,
            size,
            modified: metadata.modified().ok(),
        }));

        // Queued only after the claim has been published, and that order is load-bearing: a
        // pricing thread is running already, so submitting first would let a `Priced` reach
        // the consumer for a row it has not been told about.
        if let Some(path) = queued {
            if let Err(returned) = submit.send(Job { path, metadata }) {
                // Unreachable while the pool's receiver is alive, which it is for the whole
                // walk. Reported rather than dropped: a claim queued and never priced would
                // otherwise be indistinguishable from one nobody asked to price.
                self.fail(
                    Some(returned.0.path),
                    "could not be queued for pricing".to_owned(),
                );
            }
        }

        // The whole thesis, in one line: what we have claimed, we do not enumerate.
        WalkState::Skip
    }

    /// The one pass tier two needs over a candidate, and the three ways it can refuse.
    ///
    /// Returns the size when the directory is claimable, and `None` when it is not — each
    /// refusal already reported to the user through the errors or the checkout count, because
    /// a directory somebody expected to see and did not is exactly what needs explaining.
    ///
    /// Unlike tier one this never goes near the pricing pool. The survey is not optional work:
    /// neither the size floor nor "holds no checkout" can be inferred, and the second is a
    /// negative, which is only proved by covering everything. By the time the tier can say
    /// "claim", it has already paid for the number.
    fn survey(&self, path: &Path, metadata: &std::fs::Metadata) -> Option<Size> {
        let surveyed = self.measurer.survey(path, metadata);
        let blind_spots = !surveyed.unreadable.is_empty() || !surveyed.not_crossed.is_empty();
        self.report_blind_spots(
            surveyed.unreadable,
            "unreadable, so this subtree could not be judged reclaimable",
        );
        self.report_blind_spots(
            surveyed.not_crossed,
            "on another filesystem, so this subtree could not be judged reclaimable",
        );
        if blind_spots {
            // Part of the subtree could not be read, so neither "holds no checkout" nor the
            // size is established — both are claims about the whole of it. Tier one can live
            // with a lower bound because a rule already vouched for the directory; here the
            // traversal *is* the evidence, and unjudgeable ground is left alone.
            return None;
        }
        if surveyed.nested_repo.is_some() {
            // Somebody's checkout lives in here, so this directory is not a single thing to be
            // removed. `git clean` descends past it rather than collapsing it, and so do we.
            self.holding_a_checkout.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        surveyed
            .size
            .bytes()
            .is_some_and(|bytes| bytes >= self.min_size)
            .then_some(surveyed.size)
    }

    /// One pricing thread: takes claims off the queue and measures them until the walk is
    /// finished with it.
    ///
    /// Ends when every sender is gone, which is what makes the pool self-terminating and is
    /// why [`Walker::run`] is careful about where the senders are dropped.
    fn price(&self, queue: &Mutex<std::sync::mpsc::Receiver<Job>>) {
        loop {
            // The lock covers the `recv` and nothing else. Held across the measurement it
            // would make the pool one thread wearing several hats — and the measurement is
            // the entire reason the pool exists.
            let job = lock(queue).recv();
            let Ok(job) = job else { return };

            // Announced before the traversal rather than after it, which is the only ordering
            // that makes the event mean anything: it says "a thread is in here", and a thread
            // that has already come out is not.
            (self.on_found)(Found::Pricing(job.path.clone()));
            let measured = self.measurer.measure(&job.path, &job.metadata);
            self.report_blind_spots(
                measured.unreadable,
                "unreadable, so this size is a lower bound",
            );
            if let Some(bytes) = measured.size.bytes() {
                self.reclaimed.fetch_add(bytes, Ordering::Relaxed);
                // The claim was counted as unpriced when it was published. It is not any
                // longer, and the outcome has to agree with the events the consumer saw.
                self.unmeasured.fetch_sub(1, Ordering::Relaxed);
            }
            (self.on_found)(Found::Priced(Priced {
                path: job.path,
                size: measured.size,
            }));
        }
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
