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
//! ## What the under-root check is and is not
//!
//! It is defence in depth against a malformed, stale or hostile path arriving from a caller,
//! a config file or a scan of a tree someone else can write to. It is **not** a defence
//! against an attacker racing the removal: every check here is a `stat` followed later by an
//! `unlink`, and between the two a path component can be swapped. Closing that needs an
//! `openat`-based descent holding a directory descriptor, which needs `libc` and therefore
//! `unsafe`, which this crate forbids. Saying so is better than implying a guarantee that is
//! not here; on the disk of the person running the cleaner, the window is not the threat.
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
use std::fmt;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use crate::size::{Size, allocated, device, multiply_linked};
use crate::walk::Hit;

/// How far the pool is oversubscribed past the machine's parallelism, because the work is
/// waiting on the filesystem rather than on a core.
const OVERSUBSCRIPTION: usize = 4;

/// An upper bound on the pool, so a many-core machine does not spawn hundreds of threads to
/// contend for one device queue. Bounded rather than tuned: past this point the win has not
/// been measured, and the cost — a stack each — has.
const MAX_THREADS: usize = 64;

/// A directory offered for removal.
#[derive(Debug, Clone)]
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
        let (root, boundary) = match canonical_root(&self.root) {
            Ok(resolved) => resolved,
            Err(err) => {
                // With no root there is nothing to prove anything against, so every target
                // is refused rather than judged against a path that does not exist.
                let why = format!("{}: {err}", self.root.display());
                return Plan {
                    root: self.root.clone(),
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

/// Removes what a [`Plan`] says to remove, and nothing else.
#[derive(Debug, Clone, Default)]
pub struct Deleter {
    threads: Option<usize>,
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
                        mine.push(Sweep::new(plan).run(target));
                    }
                    lock(&collected).append(&mut mine);
                });
            }
        });

        for mut sweep in collected
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
        {
            // A record only for a target something actually happened to, so `removed` means
            // what it says rather than "was considered".
            if sweep.entries > 0 || sweep.complete {
                removal.removed.push(Removed {
                    path: sweep.path,
                    bytes: sweep.bytes,
                    entries: sweep.entries,
                    complete: sweep.complete,
                });
            }
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
struct Sweep<'a> {
    plan: &'a Plan,
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
}

impl<'a> Sweep<'a> {
    fn new(plan: &'a Plan) -> Self {
        Self {
            plan,
            path: PathBuf::new(),
            bytes: 0,
            entries: 0,
            complete: false,
            linked: HashSet::new(),
            kept: Vec::new(),
            failures: Vec::new(),
        }
    }

    fn run(mut self, target: &PlanTarget) -> Self {
        self.path.clone_from(&target.path);
        self.complete = match target.path.symlink_metadata() {
            Ok(metadata) => self.entry(&target.path, &metadata),
            Err(err) => {
                self.failed(&target.path, &err);
                false
            }
        };
        self
    }

    /// One filesystem entry, whatever kind it is. The metadata is always from
    /// `symlink_metadata`, so a symlink is a symlink here and never the thing it points at.
    fn entry(&mut self, path: &Path, metadata: &fs::Metadata) -> bool {
        if crosses_boundary(self.plan.one_file_system, self.plan.boundary, metadata) {
            self.kept.push(Refused {
                path: path.to_path_buf(),
                reason: Refusal::OtherFileSystem,
            });
            return false;
        }
        if metadata.is_dir() {
            self.directory(path, metadata)
        } else {
            self.unlink(path, metadata)
        }
    }

    fn directory(&mut self, path: &Path, metadata: &fs::Metadata) -> bool {
        let listing = match fs::read_dir(path) {
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
                Ok(child) => children.push(child),
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
        if children.iter().any(|child| child.file_name() == ".git") {
            self.kept.push(Refused {
                path: path.to_path_buf(),
                reason: Refusal::HoldsCheckout,
            });
            return false;
        }

        for child in children {
            let child = child.path();
            match child.symlink_metadata() {
                Ok(metadata) => complete &= self.entry(&child, &metadata),
                Err(err) => {
                    self.failed(&child, &err);
                    complete = false;
                }
            }
        }

        // Only once every child is known to be gone. An `rmdir` attempted over a refusal
        // would fail anyway, but reporting that as a failure would call the safety model a
        // fault; and a directory left short by a `readdir` error must not be retried blind.
        if !complete {
            return false;
        }
        match fs::remove_dir(path) {
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
    fn unlink(&mut self, path: &Path, metadata: &fs::Metadata) -> bool {
        match fs::remove_file(path) {
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

    fn count(&mut self, metadata: &fs::Metadata) {
        self.entries += 1;
        if let Some(identity) = multiply_linked(metadata) {
            if !self.linked.insert(identity) {
                return;
            }
        }
        self.bytes += allocated(metadata);
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

/// The canonical root and the device it lives on.
fn canonical_root(root: &Path) -> io::Result<(PathBuf, u64)> {
    let canonical = fs::canonicalize(root)?;
    let metadata = canonical.symlink_metadata()?;
    Ok((canonical, device(&metadata)))
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
fn crosses_boundary(one_file_system: bool, boundary: u64, metadata: &fs::Metadata) -> bool {
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
