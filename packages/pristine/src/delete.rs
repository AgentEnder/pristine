//! The deleter: the half that cannot be undone.
//!
//! ## Why there is a plan
//!
//! Every safety check happens while the [`Plan`] is being built, and removal executes the
//! plan without re-deciding anything. That is what makes `--dry-run` honest rather than
//! approximate: the thing printed is the same object the deleter consumes, so a preview
//! cannot disagree with the run it previews.
//!
//! ## Resolving a path without following it
//!
//! Proving a target is under the scan root means resolving `..` and any symlinked ancestor,
//! which is what [`fs::canonicalize`] does — except that canonicalising the *target* would
//! also resolve the target itself, and a symlinked claim (Bazel's `bazel-*`) must be unlinked
//! as a link rather than followed to whatever it points at. So the parent is canonicalised
//! and the final component is joined back on. Nothing can hide in the ancestry, and the leaf
//! is left alone.
//!
//! ## The checks that fail toward "keep"
//!
//! Tier two's review (#588) found three bugs of the same shape: a check whose failure mode is
//! silence, so an unreadable or unseen subtree reads as a cleared one. The deleter inherits
//! that discipline. A directory it could not read is a failure, not an empty directory; a
//! subtree it refused to enter leaves every ancestor standing, because the `rmdir` is only
//! attempted when every child is known to be gone.
//!
//! ## The under-root check is not enough on its own, and why nothing is removed by name
//!
//! The plan's check is defence in depth against a malformed, stale or hostile path arriving
//! from a caller, a config file or a scan of a tree someone else can write to. But what it
//! proves, it proves about a *path*, and a path is a name that something else can re-point.
//! A check like that is worth what it is worth at the moment of the `unlink`, not at the
//! moment it ran — and in between sit a printed plan and a confirmation prompt.
//!
//! So the removal never re-walks a target by name. It opens the scan root once and then
//! **descends by descriptor**: every component is opened from its already-open parent with
//! `openat(fd, name, O_DIRECTORY | O_NOFOLLOW)`, and every removal is an `unlinkat` against
//! the descriptor of the directory that holds the entry. A component swapped for a symlink
//! fails the open with `ELOOP` rather than redirecting it, because the kernel resolves one
//! name against one held descriptor and there is no path left for anything to re-point.
//! "Under the root" becomes a property of how the syscall was issued.
//!
//! `cap-primitives` supplies those calls. It is the same machinery `cap-std` is built from,
//! and it is why this does not need `libc` and therefore does not need the `unsafe` this
//! crate forbids: the descent this module always wanted turns out to be reachable in safe
//! Rust. The root is opened once per batch rather than once per target, so the root itself
//! cannot be swapped mid-run either.
//!
//! One path still has to be resolved by name, and it cannot be avoided: the scan root has to
//! be opened from somewhere. That makes it the most dangerous name in the program rather than
//! an exempt one, because every descriptor descends from that handle — get it wrong and the
//! whole batch is misdirected, not one target. So [`open_root`] opens the root's final
//! component with `O_NOFOLLOW` from its own parent, and then checks the descriptor's
//! `(device, inode)` against the pair recorded while the plan was built. The second check is
//! the one that matters: a root renamed away and replaced by an ordinary directory on the same
//! filesystem offers no symlink to refuse and crosses no boundary, so nothing about the name
//! distinguishes it from the directory the planner validated. Only the inode does.
//!
//! ## Fan-out
//!
//! `unlink` and `rmdir` are latency-bound rather than CPU-bound, so the pool is deliberately
//! oversubscribed — the same conclusion the Node predecessor reached empirically. The unit of
//! work is one target, not one directory: a sweep, which is the mode this exists for, has
//! hundreds of targets, and per-target parallelism would need a join counter per directory to
//! know when its `rmdir` is safe. Removing a single target is therefore single-threaded, as
//! `rm -rf` is.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use cap_primitives::ambient_authority;
use cap_primitives::fs::{
    FollowSymlinks, Metadata, open_ambient_dir, open_dir_nofollow, read_base_dir, remove_dir,
    remove_file, stat,
};

use crate::size::{Size, Stat, allocated, device, identity, multiply_linked};
use crate::walk::Hit;

/// How far the pool is oversubscribed past the machine's parallelism, because the work is
/// waiting on the filesystem rather than on a core.
const OVERSUBSCRIPTION: usize = 4;

/// An upper bound on the pool, so a many-core machine does not spawn hundreds of threads to
/// contend for one device queue. Bounded rather than tuned: past this point the win has not
/// been measured, and the cost — a stack each — has.
const MAX_THREADS: usize = 64;

/// A directory offered for removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Where it is, as the caller knows it. Resolved when the plan is built.
    pub path: PathBuf,
    /// What the scan knew about its size, which on a default scan is nothing.
    pub size: Size,
}

impl Target {
    /// A target at `path`, with no size known.
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            size: Size::Unmeasured,
        }
    }
}

impl From<&Hit> for Target {
    fn from(hit: &Hit) -> Self {
        Self {
            path: hit.path.clone(),
            size: hit.size,
        }
    }
}

/// Why a directory was left where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// It did not resolve to somewhere under the scan root. Covers `..`, a symlinked
    /// ancestor, an absolute path from somewhere else, and the scan root itself.
    OutsideRoot,
    /// Another target in the same plan contains it, so removing that one removes this.
    AlreadyCovered(PathBuf),
    /// Touched more recently than the age floor allows.
    RecentlyUsed {
        /// How long ago it was touched, or `None` when the clock and the filesystem
        /// disagree about which came first.
        age: Option<Duration>,
    },
    /// On a different filesystem from the scan root, and `one_file_system` is on.
    OtherFileSystem,
    /// It holds a git checkout, so somewhere under it is work that may exist nowhere else.
    HoldsCheckout,
    /// It could not be read, so nothing about it could be proved.
    Unreadable(String),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideRoot => write!(f, "does not resolve to somewhere under the scan root"),
            Self::AlreadyCovered(by) => write!(f, "already covered by {}", by.display()),
            Self::RecentlyUsed { age: Some(age) } => {
                write!(f, "touched {} ago", humanise(*age))
            }
            Self::RecentlyUsed { age: None } => write!(f, "touched in the future"),
            Self::OtherFileSystem => write!(f, "on another filesystem"),
            Self::HoldsCheckout => write!(f, "holds a git checkout"),
            Self::Unreadable(why) => write!(f, "{why}"),
        }
    }
}

/// One directory that was left alone, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// The directory. For a mid-removal refusal this is the subtree that stopped it, which
    /// is more specific than the target it sits in.
    pub path: PathBuf,
    /// What stopped it.
    pub reason: Refusal,
}

/// A target that survived every check, with its path resolved.
#[derive(Debug, Clone)]
pub struct PlanTarget {
    /// The resolved path: no `..`, no symlinked ancestor, and proved to be under the root.
    /// This — never the requested path — is what the deleter unlinks.
    pub path: PathBuf,
    /// What the caller asked for, kept so a report can name what the user typed.
    pub requested: PathBuf,
    /// What the scan knew about its size.
    pub size: Size,
    /// Whether the target is itself a symlink, in which case removing it is one `unlink`.
    pub is_symlink: bool,
}

/// A resolved, checked list of directories to remove.
///
/// Building one performs every check in the safety model. The deleter re-derives nothing, so
/// what [`Plan`] says is what happens.
#[derive(Debug, Clone)]
pub struct Plan {
    root: PathBuf,
    /// Which directory the root's path named when the plan was built. The deleter has to
    /// resolve that path once, and this is what proves the descriptor it gets back is the
    /// same directory rather than whatever has taken the name since.
    root_identity: Option<(u64, u64)>,
    targets: Vec<PlanTarget>,
    kept: Vec<Refused>,
    boundary: u64,
    one_file_system: bool,
}

impl Plan {
    /// The canonical scan root. Nothing outside it is ever touched.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directories that will be removed.
    #[must_use]
    pub fn targets(&self) -> &[PlanTarget] {
        &self.targets
    }

    /// The directories that will not be, and why.
    #[must_use]
    pub fn kept(&self) -> &[Refused] {
        &self.kept
    }

    /// Whether there is anything to do.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// The bytes the plan can put a number on. Read it beside [`Plan::unpriced`]: a default
    /// scan measures nothing, so a plan over a 40 GB tree can honestly report zero here.
    #[must_use]
    pub fn measured_bytes(&self) -> u64 {
        self.targets
            .iter()
            .filter_map(|target| target.size.bytes())
            .sum()
    }

    /// How many targets carry no size, because the scan pruned at them rather than walking
    /// them to produce a number it was about to discard.
    #[must_use]
    pub fn unpriced(&self) -> usize {
        self.targets
            .iter()
            .filter(|target| target.size.bytes().is_none())
            .count()
    }
}

/// Builds a [`Plan`] under a fixed policy.
#[derive(Debug, Clone)]
pub struct Planner {
    root: PathBuf,
    one_file_system: bool,
    older_than: Option<Duration>,
}

impl Planner {
    /// A planner for `root`, with the safety model's defaults: one filesystem, no age floor.
    #[must_use]
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            one_file_system: true,
            older_than: None,
        }
    }

    /// Whether to refuse a target on a different filesystem from the root. On by default:
    /// crossing a mount is how a scan of one project reaches a network share or a backup
    /// volume that happens to be mounted inside it.
    #[must_use]
    pub fn one_file_system(mut self, one_file_system: bool) -> Self {
        self.one_file_system = one_file_system;
        self
    }

    /// Refuse anything touched more recently than this. Off by default — a floor that is on
    /// without being asked for silently keeps directories the user chose — but recommended,
    /// because a `node_modules` used this morning is not reclaimable in any useful sense.
    #[must_use]
    pub fn older_than(mut self, older_than: Option<Duration>) -> Self {
        self.older_than = older_than;
        self
    }

    /// Resolves and checks every target.
    ///
    /// Nothing here touches the filesystem beyond `stat` and `canonicalize`. A target that
    /// fails any check is moved to [`Plan::kept`] rather than dropped, because a directory
    /// the user selected and did not get is something they need to be told about.
    #[must_use]
    pub fn plan<I>(&self, targets: I) -> Plan
    where
        I: IntoIterator<Item = Target>,
    {
        let now = SystemTime::now();
        let ValidatedRoot {
            path: root,
            device: boundary,
            identity: root_identity,
        } = match canonical_root(&self.root) {
            Ok(resolved) => resolved,
            Err(err) => {
                // With no root there is nothing to prove anything against, so every target
                // is refused rather than judged against a path that does not exist.
                let why = format!("{}: {err}", self.root.display());
                return Plan {
                    root: self.root.clone(),
                    root_identity: None,
                    targets: Vec::new(),
                    kept: targets
                        .into_iter()
                        .map(|target| Refused {
                            path: target.path,
                            reason: Refusal::Unreadable(why.clone()),
                        })
                        .collect(),
                    boundary: 0,
                    one_file_system: self.one_file_system,
                };
            }
        };

        let mut accepted = Vec::new();
        let mut kept = Vec::new();
        for target in targets {
            match self.judge(&target, &root, boundary, now) {
                Ok(planned) => accepted.push(planned),
                Err(reason) => kept.push(Refused {
                    path: target.path,
                    reason,
                }),
            }
        }

        // Sorted, so the only possible ancestor of a target is the last one retained: no
        // retained target contains another, and a parent always sorts before its children.
        accepted.sort_by(|a, b| a.path.cmp(&b.path));
        let mut targets: Vec<PlanTarget> = Vec::with_capacity(accepted.len());
        for target in accepted {
            match targets.last() {
                // Removing the outer target removes the inner one. Keeping both would report
                // a failure for a directory that is gone because the plan worked.
                Some(outer) if target.path.starts_with(&outer.path) => kept.push(Refused {
                    path: target.requested,
                    reason: Refusal::AlreadyCovered(outer.path.clone()),
                }),
                _ => targets.push(target),
            }
        }

        Plan {
            root,
            root_identity,
            targets,
            kept,
            boundary,
            one_file_system: self.one_file_system,
        }
    }

    /// Every check one target has to pass, in the order that costs least.
    fn judge(
        &self,
        target: &Target,
        root: &Path,
        boundary: u64,
        now: SystemTime,
    ) -> Result<PlanTarget, Refusal> {
        let path = resolve(&target.path, root)?;
        let metadata = path
            .symlink_metadata()
            .map_err(|err| Refusal::Unreadable(err.to_string()))?;

        if crosses_boundary(self.one_file_system, boundary, &metadata) {
            return Err(Refusal::OtherFileSystem);
        }
        if let Some(floor) = self.older_than {
            let age = metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok());
            if age.is_none_or(|age| age < floor) {
                return Err(Refusal::RecentlyUsed { age });
            }
        }

        Ok(PlanTarget {
            requested: target.path.clone(),
            is_symlink: metadata.is_symlink(),
            size: target.size,
            path,
        })
    }
}

/// Somebody watching a removal happen. See [`Deleter::watching`].
type Watcher = Arc<dyn Fn(&Step) + Send + Sync>;

/// What a removal reports while it is happening.
///
/// Two events rather than one, for the same reason [`crate::Found`] has three: the bytes leave
/// the disk over seconds and the target finishes once, and those are different facts. A live
/// view needs both — a row cannot show its size falling toward zero if the only news it ever
/// gets is that the directory has already gone.
#[derive(Debug, Clone)]
pub enum Step {
    /// Bytes have left the disk and this target is still being swept.
    Freeing(Freeing),
    /// The sweep is finished with this target, in whole or in part.
    Finished(Removed),
}

/// How far into one target a sweep has got.
#[derive(Debug, Clone)]
pub struct Freeing {
    /// The target.
    pub path: PathBuf,
    /// Allocated bytes given back **so far**, counting a hard-linked file once — the same
    /// accounting [`Removed::bytes`] uses, because they are the same running total read at
    /// different moments.
    ///
    /// **Cumulative, never a delta.** Each report supersedes the last for this path, so a
    /// consumer that coalesces two of them loses nothing, and one that keeps the latest per
    /// target cannot double-count however the pool interleaves them. It is also what makes
    /// reconciling against the final [`Removal`] exact rather than approximate: the last word
    /// on a target is a total, not a correction.
    pub bytes: u64,
    /// Entries unlinked so far.
    pub entries: u64,
}

/// How many entries a sweep unlinks between progress reports.
///
/// A report per entry would be 24,001 channel messages for one `node_modules` and a `PathBuf`
/// clone for each. This is the granularity a 30fps view can actually use: a target big enough
/// to watch emits hundreds of these, and one small enough not to is over before it matters.
const REPORT_EVERY: u64 = 64;

/// …or this many bytes, whichever comes first.
///
/// Entries alone would leave a target that is sixteen very large files reporting nothing until
/// it finished, which is the exact failure this event exists to remove.
const REPORT_BYTES: u64 = 8 * 1024 * 1024;

/// Removes what a [`Plan`] says to remove, and nothing else.
#[derive(Clone, Default)]
pub struct Deleter {
    threads: Option<usize>,
    watching: Option<Watcher>,
}

impl fmt::Debug for Deleter {
    /// Hand-written because a closure has no `Debug`, and the only interesting thing about
    /// one here is whether anybody is listening.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deleter")
            .field("threads", &self.threads)
            .field("watching", &self.watching.is_some())
            .finish()
    }
}

impl Deleter {
    /// A deleter with the default pool.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many threads to remove with. Defaults to the machine's parallelism times
    /// [`OVERSUBSCRIPTION`], bounded by [`MAX_THREADS`] and by the number of targets.
    #[must_use]
    pub fn threads(mut self, threads: usize) -> Self {
        self.threads = Some(threads);
        self
    }

    /// Reports the removal as it happens, rather than only in the [`Removal`] at the end.
    ///
    /// The batch CLI has no use for this — it prints one report when the removal is over —
    /// but a live view does, twice over. [`Step::Finished`] is what drops a row: a row for a
    /// directory that is already gone is one a reader can still put a cursor on, and one they
    /// can watch a second delete keystroke land on. [`Step::Freeing`] is what lets that row
    /// **empty** first, on the bytes actually leaving the disk rather than on a timer — the
    /// difference between showing what is happening and animating over the fact that it
    /// already happened.
    ///
    /// Called from the pool, so `sink` is `Send + Sync` and may be called from several
    /// threads at once and in any order. A `Finished` sees exactly what [`Removal::removed`]
    /// will contain — same condition, same values — because both come from
    /// [`Sweep::reported`], and a `Freeing` is the same running total read earlier.
    #[must_use]
    pub fn watching(mut self, sink: impl Fn(&Step) + Send + Sync + 'static) -> Self {
        self.watching = Some(Arc::new(sink));
        self
    }

    /// Executes the plan.
    ///
    /// One target's failure costs that target and nothing else: everything is collected and
    /// reported, and the caller turns a non-empty [`Removal::failures`] into a non-zero exit.
    #[must_use]
    pub fn remove(&self, plan: &Plan) -> Removal {
        let mut removal = Removal {
            kept: plan.kept.clone(),
            ..Removal::default()
        };
        if plan.targets.is_empty() {
            return removal;
        }

        // The one path resolved by name in the whole removal, and the anchor for every
        // descriptor below it, so it is checked hardest — see [`open_root`]. Opened once per
        // batch rather than once per target, which also means the root cannot be swapped
        // between two targets.
        let root = match open_root(plan) {
            Ok(root) => root,
            Err(err) => {
                removal.failures.push(Failure {
                    path: plan.root.clone(),
                    message: err.to_string(),
                });
                return removal;
            }
        };

        let threads = self
            .threads
            .unwrap_or_else(default_threads)
            .clamp(1, plan.targets.len());
        let cursor = AtomicUsize::new(0);
        let collected = Mutex::new(Vec::new());

        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let at = cursor.fetch_add(1, Ordering::Relaxed);
                        let Some(target) = plan.targets.get(at) else {
                            break;
                        };
                        let sweep = Sweep::new(plan, &root, self.watching.as_ref()).run(target);
                        if let (Some(watching), Some(removed)) =
                            (self.watching.as_ref(), sweep.reported())
                        {
                            watching(&Step::Finished(removed));
                        }
                        mine.push(sweep);
                    }
                    lock(&collected).append(&mut mine);
                });
            }
        });

        for mut sweep in collected
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
        {
            removal.removed.extend(sweep.reported());
            removal.kept.append(&mut sweep.kept);
            removal.failures.append(&mut sweep.failures);
        }

        // The pool finishes in whatever order the filesystem allows, and a report a person
        // reads twice should not reorder itself between runs.
        removal.removed.sort_by(|a, b| a.path.cmp(&b.path));
        removal.kept.sort_by(|a, b| a.path.cmp(&b.path));
        removal.failures.sort_by(|a, b| a.path.cmp(&b.path));
        removal
    }
}

/// One target that was removed, in whole or in part.
#[derive(Debug, Clone)]
pub struct Removed {
    /// The target.
    pub path: PathBuf,
    /// Allocated bytes given back, counting a hard-linked file once.
    pub bytes: u64,
    /// Files, directories and links unlinked.
    pub entries: u64,
    /// Whether the target itself is gone. False when something inside it was refused or
    /// failed, which leaves it and everything above the refusal standing.
    pub complete: bool,
}

/// Something that went wrong. Collected rather than fatal.
#[derive(Debug, Clone)]
pub struct Failure {
    /// The path involved.
    pub path: PathBuf,
    /// What the filesystem said.
    pub message: String,
}

/// What a removal did.
#[derive(Debug, Clone, Default)]
pub struct Removal {
    /// Targets something was removed from.
    pub removed: Vec<Removed>,
    /// Directories left in place: the plan's refusals, plus every subtree a sweep declined
    /// to enter.
    pub kept: Vec<Refused>,
    /// Everything that failed.
    pub failures: Vec<Failure>,
}

impl Removal {
    /// Allocated bytes given back.
    #[must_use]
    pub fn bytes_freed(&self) -> u64 {
        self.removed.iter().map(|removed| removed.bytes).sum()
    }

    /// How many files, directories and links were unlinked.
    #[must_use]
    pub fn entries_removed(&self) -> u64 {
        self.removed.iter().map(|removed| removed.entries).sum()
    }

    /// Whether everything the plan asked for happened. A refusal is not a failure — it is
    /// the safety model working — so this asks only about [`Removal::failures`].
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty()
    }
}

/// One target's removal. Per-target rather than shared, so the pool contends for the
/// filesystem and not for a mutex.
///
/// Every method below takes the *descriptor* of the directory holding the entry it acts on,
/// plus the entry's name. The `path` alongside them is for reporting only — it is what the
/// user reads in a refusal, and it is never resolved.
struct Sweep<'a> {
    plan: &'a Plan,
    /// The scan root, opened once by [`Deleter::remove`] and shared across the pool. Every
    /// descriptor this sweep holds is descended from it.
    root: &'a fs::File,
    path: PathBuf,
    bytes: u64,
    entries: u64,
    complete: bool,
    /// The `(device, inode)` of every multiply-linked file already counted, so a hard-linked
    /// artefact is worth its blocks once — the same accounting [`crate::size`] uses, so a
    /// plan's estimate and the bytes actually freed are measured the same way.
    linked: HashSet<(u64, u64)>,
    kept: Vec<Refused>,
    failures: Vec<Failure>,
    /// Where progress goes while this sweep runs, and the totals already sent, so a report is
    /// a step forward rather than a repeat.
    watching: Option<&'a Watcher>,
    told_bytes: u64,
    told_entries: u64,
}

impl<'a> Sweep<'a> {
    fn new(plan: &'a Plan, root: &'a fs::File, watching: Option<&'a Watcher>) -> Self {
        Self {
            plan,
            root,
            path: PathBuf::new(),
            bytes: 0,
            entries: 0,
            complete: false,
            linked: HashSet::new(),
            kept: Vec::new(),
            failures: Vec::new(),
            watching,
            told_bytes: 0,
            told_entries: 0,
        }
    }

    fn run(mut self, target: &PlanTarget) -> Self {
        self.path.clone_from(&target.path);
        let Some((parent, name)) = self.parent_of(&target.path) else {
            return self;
        };
        self.complete = self.entry(&parent, &name, &target.path);
        self
    }

    /// What this sweep did, or `None` when nothing happened to the target.
    ///
    /// A record only for a target something actually happened to, so [`Removal::removed`]
    /// means what it says rather than "was considered". One function rather than the same
    /// condition written twice, because the other reader is [`Deleter::watching`] and a live
    /// view that dropped rows the final report then listed as untouched would be worse than
    /// having no progress at all.
    fn reported(&self) -> Option<Removed> {
        (self.entries > 0 || self.complete).then(|| Removed {
            path: self.path.clone(),
            bytes: self.bytes,
            entries: self.entries,
            complete: self.complete,
        })
    }

    /// Opens the directory that holds the target, by walking down from the root's descriptor
    /// one component at a time.
    ///
    /// Each step is `openat(fd, name, O_DIRECTORY | O_NOFOLLOW)` against the descriptor the
    /// previous step returned, so no part of the path is ever re-resolved from a name and a
    /// component swapped for a symlink is an `ELOOP` rather than a redirect. The final
    /// component is *not* opened: a claim may legitimately be a symlink — Bazel's `bazel-*` —
    /// and it has to be unlinked as a link rather than followed.
    fn parent_of(&mut self, target: &Path) -> Option<(fs::File, OsString)> {
        // Unreachable for a planned target, which the planner proved is under the root.
        // Refusing beats descending from a root this path has nothing to do with.
        let Ok(relative) = target.strip_prefix(&self.plan.root) else {
            self.failures.push(Failure {
                path: target.to_path_buf(),
                message: format!("is not under {}", self.plan.root.display()),
            });
            return None;
        };

        let mut names: Vec<&OsStr> = relative.components().map(Component::as_os_str).collect();
        let name = names.pop()?;
        let mut walked = self.plan.root.clone();
        // `dup`, so the loop can own each handle in turn without consuming the shared root.
        let mut dir = match self.root.try_clone() {
            Ok(dir) => dir,
            Err(err) => {
                self.failed(&walked, &err);
                return None;
            }
        };
        for component in names {
            walked.push(component);
            dir = match open_dir_nofollow(&dir, Path::new(component)) {
                Ok(next) => next,
                Err(err) => {
                    self.failed(&walked, &err);
                    return None;
                }
            };
        }
        Some((dir, name.to_owned()))
    }

    /// One filesystem entry, whatever kind it is, named relative to `parent`'s descriptor.
    ///
    /// The metadata is always `fstatat` with `AT_SYMLINK_NOFOLLOW`, so a symlink is a symlink
    /// here and never the thing it points at.
    fn entry(&mut self, parent: &fs::File, name: &OsStr, path: &Path) -> bool {
        let metadata = match stat(parent, Path::new(name), FollowSymlinks::No) {
            Ok(metadata) => metadata,
            Err(err) => {
                self.failed(path, &err);
                return false;
            }
        };
        if crosses_boundary(self.plan.one_file_system, self.plan.boundary, &metadata) {
            self.kept.push(Refused {
                path: path.to_path_buf(),
                reason: Refusal::OtherFileSystem,
            });
            return false;
        }
        if metadata.is_dir() {
            self.directory(parent, name, path, &metadata)
        } else {
            self.unlink(parent, name, path, &metadata)
        }
    }

    fn directory(
        &mut self,
        parent: &fs::File,
        name: &OsStr,
        path: &Path,
        metadata: &Metadata,
    ) -> bool {
        // The same `O_NOFOLLOW` open as the descent. If the directory just seen by `fstatat`
        // has become a symlink in the meantime, this fails rather than following it — which
        // is the whole reason the traversal is written against descriptors.
        let dir = match open_dir_nofollow(parent, Path::new(name)) {
            Ok(dir) => dir,
            Err(err) => {
                self.failed(path, &err);
                return false;
            }
        };
        let listing = match read_base_dir(&dir) {
            Ok(listing) => listing,
            Err(err) => {
                // Not an empty directory. A cleaner that treats "I could not look" as "there
                // was nothing there" removes the directory and everything it never saw.
                self.failed(path, &err);
                return false;
            }
        };

        let mut children = Vec::new();
        let mut complete = true;
        for child in listing {
            match child {
                Ok(child) => children.push(child.file_name()),
                // `readdir` gave up part-way through a directory it had already opened, so
                // the listing is short by an unknown amount. Anything below is unaccounted
                // for, which is exactly the state in which nothing may be removed.
                Err(err) => {
                    self.failed(path, &err);
                    complete = false;
                }
            }
        }

        // Before anything in this directory is touched: a checkout under here may hold work
        // that exists nowhere else, and half-removing it is worse than not starting.
        if children.iter().any(|child| child == ".git") {
            self.kept.push(Refused {
                path: path.to_path_buf(),
                reason: Refusal::HoldsCheckout,
            });
            return false;
        }

        for child in children {
            complete &= self.entry(&dir, &child, &path.join(&child));
        }

        // Only once every child is known to be gone. An `rmdir` attempted over a refusal
        // would fail anyway, but reporting that as a failure would call the safety model a
        // fault; and a directory left short by a `readdir` error must not be retried blind.
        if !complete {
            return false;
        }
        // `unlinkat(parent_fd, name, AT_REMOVEDIR)`, so what is removed is the entry we just
        // walked and not whatever the name resolves to now.
        match remove_dir(parent, Path::new(name)) {
            Ok(()) => {
                self.count(metadata);
                true
            }
            Err(err) => {
                self.failed(path, &err);
                false
            }
        }
    }

    /// Unlinks a file or a symlink. A symlink is removed as a link: what it points at is
    /// somewhere else, is very likely outside the root, and is not ours.
    fn unlink(
        &mut self,
        parent: &fs::File,
        name: &OsStr,
        path: &Path,
        metadata: &Metadata,
    ) -> bool {
        match remove_file(parent, Path::new(name)) {
            Ok(()) => {
                self.count(metadata);
                true
            }
            Err(err) => {
                self.failed(path, &err);
                false
            }
        }
    }

    fn count(&mut self, metadata: &Metadata) {
        self.entries += 1;
        if let Some(identity) = multiply_linked(metadata) {
            if !self.linked.insert(identity) {
                self.tell();
                return;
            }
        }
        self.bytes += allocated(metadata);
        self.tell();
    }

    /// Says how far this sweep has got, when it has got far enough to be worth saying.
    ///
    /// The one place a removal speaks while it is still running, and it is deliberately here
    /// — inside the single function that accounts for a freed entry — rather than at the
    /// traversal's branches. A report emitted anywhere else would be a second opinion about
    /// how much has gone, and the whole value of the event is that it is the *same* running
    /// total the final [`Removed`] carries, read earlier.
    fn tell(&mut self) {
        let Some(watching) = self.watching else {
            return;
        };
        if self.entries - self.told_entries < REPORT_EVERY
            && self.bytes - self.told_bytes < REPORT_BYTES
        {
            return;
        }
        self.told_entries = self.entries;
        self.told_bytes = self.bytes;
        watching(&Step::Freeing(Freeing {
            path: self.path.clone(),
            bytes: self.bytes,
            entries: self.entries,
        }));
    }

    fn failed(&mut self, path: &Path, err: &impl fmt::Display) {
        self.failures.push(Failure {
            path: path.to_path_buf(),
            message: err.to_string(),
        });
    }
}

/// Asks a yes/no question whose answer defaults to **no**.
///
/// Only `y` or `yes` mean yes. Everything else does not, and that includes end of input: a
/// pipe with nothing in it is not consent, so a script that means to delete has to say so
/// with a flag rather than by being silent.
///
/// # Errors
///
/// If the prompt cannot be written or the answer cannot be read.
pub fn confirm(
    question: &str,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<bool> {
    write!(output, "{question} [y/N] ")?;
    output.flush()?;
    let mut answer = String::new();
    if input.read_line(&mut answer)? == 0 {
        return Ok(false);
    }
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// The scan root as the planner proved it: where it really is, which device everything under
/// it has to sit on, and — the part a path cannot carry — which directory it actually is.
struct ValidatedRoot {
    path: PathBuf,
    device: u64,
    identity: Option<(u64, u64)>,
}

/// The canonical root, the device it lives on, and the inode it is.
fn canonical_root(root: &Path) -> io::Result<ValidatedRoot> {
    let canonical = fs::canonicalize(root)?;
    let metadata = canonical.symlink_metadata()?;
    Ok(ValidatedRoot {
        device: device(&metadata),
        identity: identity(&metadata),
        path: canonical,
    })
}

/// Opens the scan root, and proves the descriptor is the directory the planner validated.
///
/// This is the one path still resolved by name, and therefore the one place a name decides
/// which directory a whole batch acts on: every other descriptor descends from this one, so
/// getting it wrong misdirects everything rather than one target. Two guards, because they
/// catch different attacks.
///
/// The final component is opened with `O_NOFOLLOW` from its own parent, so a root replaced by
/// a symlink fails here rather than quietly anchoring the sweep somewhere else.
///
/// Then the descriptor's `(device, inode)` is compared with the pair recorded when the plan was
/// built. That is the load-bearing one: a root renamed away and replaced by an ordinary
/// directory offers no symlink to refuse, and if the replacement is on the same filesystem the
/// boundary check passes too. Nothing about the *name* tells the two apart — only the inode.
fn open_root(plan: &Plan) -> io::Result<fs::File> {
    let opened = match (plan.root.parent(), plan.root.file_name()) {
        (Some(parent), Some(name)) => {
            let parent = open_ambient_dir(parent, ambient_authority())?;
            open_dir_nofollow(&parent, Path::new(name))?
        }
        // `/` has no parent to be opened from, and cannot itself be a symlink.
        _ => open_ambient_dir(&plan.root, ambient_authority())?,
    };
    // `fstat` on the descriptor rather than a stat on the path, so what is checked is the
    // directory now held open and not whatever the name resolves to a moment later.
    if identity(&opened.metadata()?) != plan.root_identity {
        return Err(io::Error::other(
            "the scan root is no longer the directory the plan was built against",
        ));
    }
    Ok(opened)
}

/// Resolves `path` and proves it is under `root`, without resolving the final component.
///
/// The parent is canonicalised, so `..` and every symlinked ancestor are gone before the
/// comparison. The leaf is joined back on unresolved, because a symlinked target must be
/// unlinked as a link and canonicalising it would name what it points at instead.
fn resolve(path: &Path, root: &Path) -> Result<PathBuf, Refusal> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        // A path with no parent or no final component is `/` or `..`. Neither is a target.
        return Err(Refusal::OutsideRoot);
    };
    // A parent that will not resolve is refused either way; saying which kind of refusal it
    // was is the difference between "you pointed outside the tree" and "it is already gone".
    let parent = fs::canonicalize(parent).map_err(|err| Refusal::Unreadable(err.to_string()))?;
    let resolved = parent.join(name);
    // `starts_with` compares whole components, so `/scan-backup` does not start with
    // `/scan`. The inequality is what keeps the root itself off every plan.
    if resolved == root || !resolved.starts_with(root) {
        return Err(Refusal::OutsideRoot);
    }
    Ok(resolved)
}

/// Whether something with this metadata sits off the filesystem the plan is confined to.
///
/// One expression, called from both the plan and the sweep, so "a mount is not crossed" is
/// one decision rather than two that can drift apart. A mount is where a scan of one project
/// reaches a network share, a Time Machine volume or another user's disk.
///
/// Generic because the plan stats by path and the sweep stats by descriptor, which produce
/// two different metadata types for the same `st_dev`.
fn crosses_boundary(one_file_system: bool, boundary: u64, metadata: &impl Stat) -> bool {
    one_file_system && device(metadata) != boundary
}

fn default_threads() -> usize {
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    cores.saturating_mul(OVERSUBSCRIPTION).min(MAX_THREADS)
}

/// A duration in the units a person reads, rounded down to the coarsest that fits.
fn humanise(duration: Duration) -> String {
    const HOUR: u64 = 60 * 60;
    const DAY: u64 = 24 * HOUR;
    let seconds = duration.as_secs();
    let (value, unit) = match seconds {
        0..HOUR => (seconds / 60, "minute"),
        HOUR..DAY => (seconds / HOUR, "hour"),
        _ => (seconds / DAY, "day"),
    };
    format!("{value} {unit}{}", if value == 1 { "" } else { "s" })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{confirm, humanise};
    use std::time::Duration;

    /// Answers `input` and returns both the decision and what the user was shown.
    fn ask(input: &str) -> (bool, String) {
        let mut output = Vec::new();
        let answered = confirm("Remove 12 directories?", &mut input.as_bytes(), &mut output)
            .expect("a byte slice cannot fail to be read");
        (answered, String::from_utf8(output).expect("ASCII prompt"))
    }

    #[test]
    fn the_confirmation_defaults_to_no() {
        // Bare enter, and the prompt has to say which way that goes.
        assert!(!ask("\n").0);
        assert!(ask("\n").1.ends_with("[y/N] "));
    }

    #[test]
    fn end_of_input_is_not_consent() {
        // A script piping nothing at an irreversible prompt means it did not expect one.
        assert!(!ask("").0);
    }

    #[test]
    fn only_yes_means_yes() {
        for yes in ["y", "Y", "yes", "YES", " yes \n"] {
            assert!(ask(yes).0, "`{yes}` was read as no");
        }
        for no in ["n", "no", "\n", "  ", "sure", "yep", "yes please", "1"] {
            assert!(!ask(no).0, "`{no}` was read as yes");
        }
    }

    #[test]
    fn an_age_is_reported_in_the_coarsest_unit_that_fits() {
        assert_eq!(humanise(Duration::from_secs(90)), "1 minute");
        assert_eq!(humanise(Duration::from_secs(2 * 60 * 60)), "2 hours");
        assert_eq!(humanise(Duration::from_secs(36 * 60 * 60)), "1 day");
        assert_eq!(humanise(Duration::from_secs(90 * 24 * 60 * 60)), "90 days");
    }
}
