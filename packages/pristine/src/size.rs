//! How many bytes a claimed directory is worth, and why a normal scan does not ask.
//!
//! ## The default is not to measure
//!
//! Prune-on-match is the whole performance thesis, and a recursive measurement would undo it:
//! sizing `node_modules` means enumerating the tens of thousands of inodes the scan just
//! declined to walk. So a normal scan records the claim and reports [`Size::Unmeasured`]. The
//! traversal happens only under [`SizeMode::Breakdown`], which is what "the user asked for a
//! breakdown" compiles down to.
//!
//! ## Why not the directory's own block accounting
//!
//! The concept doc asked for "sizes from the directory's own block accounting where the
//! platform offers it". No platform pristine targets offers a *recursive* one. A directory
//! inode's block count on APFS, ext4, btrfs and ZFS alike describes the directory's own entry
//! table, not the tree beneath it — which is why `du` walks. Reporting it as the claim's size
//! would call a 40 GB `node_modules` about 48 KB, so the honest answer is `Unmeasured` rather
//! than a number that is wrong by six orders of magnitude.
//!
//! A claim that is a *symlink* — Bazel's `bazel-*` — is different: `lstat` is the complete
//! answer for a link in constant time, so those are measured even in the default mode.
//!
//! When a breakdown is asked for, the subtree goes through the tight `read_dir` + `lstat`
//! loop below rather than back through the scan that found it: no ignore stack, no rule
//! evaluation, no path bookkeeping, one pass, on the walker thread that found the claim.
//! Bytes are *allocated* blocks rather than apparent length, because allocated is what
//! deleting gives back.
//!
//! ## The one thing a default scan does have to look at
//!
//! Tier two cannot claim a directory without walking it. Its size floor cannot be inferred, and
//! neither can "holds no git repository" — which is a negative, and a negative is only proved
//! by covering everything. So [`Measurer::survey`] walks in every mode, and tier-two claims
//! carry a real size even on a default scan while tier-one claims do not.
//!
//! That is a smaller dent in the performance thesis than it sounds. For a candidate that is
//! *claimed*, the survey replaces work the walk would have done anyway — the walker would have
//! descended into all of it — with the same tight loop and no ignore stack or rule evaluation
//! per entry. The cost is in the candidates that are refused, which get surveyed and then
//! walked. Over `~/repos`: 2.9 s with tier two off, 4.1 s with it on, for 75 tier-two claims
//! that arrive priced.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::{fs, io};

/// What is known about a claim's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Size {
    /// Not measured, because the scan pruned here instead of enumerating the subtree. Not a
    /// failure and not a zero: ask for a breakdown to turn it into a number.
    #[default]
    Unmeasured,
    /// Allocated bytes, summed over everything beneath the claim.
    Measured(u64),
}

impl Size {
    /// The byte count, or `None` when nothing was measured.
    #[must_use]
    pub fn bytes(self) -> Option<u64> {
        match self {
            Self::Unmeasured => None,
            Self::Measured(bytes) => Some(bytes),
        }
    }

    /// What a person reads: a size, or a dash when nothing has looked.
    ///
    /// Not zero and not an error. Measuring a tier-one claim means enumerating the subtree the
    /// scan deliberately pruned at, so "no number yet" is the ordinary state of a claim rather
    /// than a fault — and a row of dashes has to be legible as *unpriced* rather than as
    /// empty, in the listing and in the tree alike.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Measured(bytes) => human(bytes),
            Self::Unmeasured => UNPRICED.to_owned(),
        }
    }
}

/// What an unpriced row shows instead of a number.
pub const UNPRICED: &str = "—";

/// Bytes in the units a person reads, binary because that is what the sizes are.
#[must_use]
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    #[expect(
        clippy::cast_precision_loss,
        reason = "a display rounded to one decimal place has none to lose"
    )]
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// How much work a scan may do to size what it claims.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SizeMode {
    /// Record claims without enumerating them. The default, and the performance thesis.
    #[default]
    Skip,
    /// Sum each claim's subtree. What "show me a breakdown" costs — an order of magnitude over
    /// the scan it prices (4.6 s to 55.8 s over one real `~/repos`), which is why nothing does
    /// it unasked.
    Breakdown,
    /// Sum the subtree of every claim the named path touches, and nothing else.
    ///
    /// The whole-tree breakdown is the only honest answer to "how much do I get back from all
    /// of this", and it is also the one nobody wants to wait for twice. Scoping it is what
    /// makes the number reachable at a price the user chooses: pay for the one subtree in
    /// question and leave the rest reading `Unmeasured`, which it already was.
    ///
    /// It is also what `--breakdown-under` hands the tree: a reader who wants one subtree
    /// priced and the rest left alone gets exactly that, and every other row keeps its dash.
    ///
    /// The scope has to be spelled the way the walk spells its hits — the scan root's own
    /// prefix and all — because the comparison is by path. Anchoring it is the caller's job;
    /// see the command line's `anchor`.
    BreakdownUnder(PathBuf),
}

impl SizeMode {
    /// Whether a claim at `dir` is one this mode pays to measure.
    ///
    /// A scope *containing* the claim is the obvious case. A scope *inside* it counts too, and
    /// that is not a courtesy: a claim is the smallest thing that can be priced, since the walk
    /// pruned there and nothing below it was ever enumerated. Reading "under" strictly would
    /// mean `--breakdown-under repo/node_modules/.pnpm` prices nothing at all and says so with
    /// a straight face — a confident empty answer, which is the failure shape this crate keeps
    /// meeting in other clothes.
    fn prices(&self, dir: &Path) -> bool {
        match self {
            Self::Skip => false,
            Self::Breakdown => true,
            Self::BreakdownUnder(scope) => dir.starts_with(scope) || scope.starts_with(dir),
        }
    }
}

/// The result of measuring one directory.
#[derive(Debug, Clone, Default)]
pub struct Measurement {
    /// What is known about the size.
    pub size: Size,
    /// Entries that could not be read, so the total is a lower bound. Reported rather than
    /// swallowed: a number that silently excludes an unreadable half of the tree is worse
    /// than one labelled incomplete.
    pub unreadable: Vec<PathBuf>,
}

/// Everything one pass over a tier-two candidate found.
#[derive(Debug, Clone, Default)]
pub struct Survey {
    /// The total. [`Size::Unmeasured`] only when the survey gave up early, which it does only
    /// once `nested_repo` is set and the candidate is dead anyway.
    pub size: Size,
    /// A git repository living inside the candidate, if there is one. Its presence is what
    /// stops the directory above it being removed wholesale.
    pub nested_repo: Option<PathBuf>,
    /// Entries that could not be read. Not merely a caveat on the total here: a survey that
    /// could not see all of the subtree has not established `nested_repo` either, so the
    /// caller has no grounds to claim the directory at all.
    pub unreadable: Vec<PathBuf>,
    /// Subtrees on another filesystem, which the survey does not enter.
    ///
    /// Reported for the same reason as `unreadable` and not silently skipped, which is what an
    /// earlier version did. A mount point inside a candidate hides everything under it,
    /// including a checkout — so "holds no repository", which is a claim about the whole
    /// subtree, is not established when one is present.
    pub not_crossed: Vec<PathBuf>,
}

/// Measures directories under a fixed policy.
#[derive(Debug, Clone)]
pub struct Measurer {
    mode: SizeMode,
    same_file_system: bool,
}

impl Measurer {
    /// A measurer with the given mode, staying on one filesystem.
    #[must_use]
    pub fn new(mode: SizeMode) -> Self {
        Self {
            mode,
            same_file_system: true,
        }
    }

    /// Whether to descend across a mount point. Off by default, matching the safety model:
    /// what the deleter will not cross, the measurer must not count.
    #[must_use]
    pub fn same_file_system(mut self, same_file_system: bool) -> Self {
        self.same_file_system = same_file_system;
        self
    }

    /// Whether measuring `dir` means traversing it.
    ///
    /// This is what decides whether a claim goes to the pricing pool instead of being sized on
    /// the walker thread that found it. Two things are false here and both matter: a claim
    /// this mode does not price at all, and a *symlink*, whose one `lstat` the walk has
    /// already done. Queueing either would trade a constant-time answer for a thread handoff
    /// and delay the claim's own publication for nothing.
    #[must_use]
    pub fn traverses(&self, dir: &Path, metadata: &fs::Metadata) -> bool {
        metadata.is_dir() && self.mode.prices(dir)
    }

    /// Measures `dir`, whose metadata the caller already has from the walk.
    ///
    /// Returns without touching the filesystem under [`SizeMode::Skip`], which is the point.
    #[must_use]
    pub fn measure(&self, dir: &Path, metadata: &fs::Metadata) -> Measurement {
        // A symlinked claim is worth its own inode and nothing more: the bytes are wherever
        // it points, which is outside the tree and not ours to delete. One `lstat` — already
        // done — is the complete answer, so it needs no traversal and no opt-in.
        if !metadata.is_dir() {
            return Measurement {
                size: Size::Measured(allocated(metadata)),
                unreadable: Vec::new(),
            };
        }
        if !self.mode.prices(dir) {
            return Measurement::default();
        }

        let walked = self.walk(dir, metadata, false);
        Measurement {
            size: Size::Measured(walked.bytes),
            unreadable: walked.unreadable,
        }
    }

    /// One pass over a tier-two candidate, answering both questions the tier has left: how big
    /// it is, and whether it holds a git repository.
    ///
    /// This walks whatever the mode is, because neither answer can be inferred, and it walks
    /// the *whole* subtree. An earlier version stopped as soon as it had enough bytes to clear
    /// the floor, which is much cheaper — and useless here, because "holds no repository" is a
    /// negative and a negative is only proved by covering everything. The consolation is that
    /// this pass is still cheaper than what the walker would have done had tier two not
    /// claimed the directory at all: a tight `read_dir` + `lstat` loop with no ignore stack and
    /// no rule evaluation, and then a prune.
    ///
    /// Because it always covers everything, a tier-two claim arrives with a real size even on a
    /// default scan. Tier one's claims stay [`Size::Unmeasured`]: nothing forces the walk to
    /// look inside those.
    #[must_use]
    pub fn survey(&self, dir: &Path, metadata: &fs::Metadata) -> Survey {
        // A link is worth its own inode and nothing more, and one `lstat` — already done — is
        // the whole truth about it.
        if !metadata.is_dir() {
            return Survey {
                size: Size::Measured(allocated(metadata)),
                nested_repo: None,
                unreadable: Vec::new(),
                not_crossed: Vec::new(),
            };
        }
        let walked = self.walk(dir, metadata, true);
        Survey {
            size: if walked.nested_repo.is_some() {
                Size::Unmeasured
            } else {
                Size::Measured(walked.bytes)
            },
            nested_repo: walked.nested_repo,
            unreadable: walked.unreadable,
            not_crossed: walked.not_crossed,
        }
    }

    /// The one traversal, summing allocated blocks below `dir`.
    ///
    /// With `watch_for_repos` it also stops the moment it finds a `.git`, because whatever
    /// asked for that has no further use for the total.
    fn walk(&self, dir: &Path, metadata: &fs::Metadata, watch_for_repos: bool) -> Walked {
        let mut pass = Pass {
            bytes: allocated(metadata),
            boundary: device(metadata),
            watch_for_repos,
            ..Pass::default()
        };
        pass.stack.push(dir.to_path_buf());

        while let Some(current) = pass.stack.pop() {
            match fs::read_dir(&current) {
                Ok(entries) => {
                    if let Some(repo) = self.absorb(&current, entries, &mut pass) {
                        return pass.stopped_at(repo);
                    }
                }
                Err(_) => pass.unreadable.push(current),
            }
        }
        pass.finished()
    }

    /// Folds one directory's entries into `pass`, returning the directory when a `.git` among
    /// them ends the walk.
    ///
    /// Split out of [`Measurer::walk`] so its error branch can be driven by a hand-made
    /// iterator. `readdir` failing part-way through a directory it had already opened is not
    /// something a test can arrange on a real filesystem, and it is the branch that most needs
    /// one.
    fn absorb<I>(&self, current: &Path, entries: I, pass: &mut Pass) -> Option<PathBuf>
    where
        I: IntoIterator<Item = io::Result<fs::DirEntry>>,
    {
        for entry in entries {
            // `readdir` gave up part-way through a directory it had opened, so this listing
            // is short by an unknown amount. An earlier version skipped the entry, which left
            // the survey looking complete and let a caller claim a directory nobody had
            // finished reading.
            let Ok(entry) = entry else {
                pass.unreadable.push(current.to_path_buf());
                continue;
            };
            let path = entry.path();
            // A `.git` marks a checkout, and it counts whether it is a directory or the
            // file a linked work tree and a submodule use.
            if pass.watch_for_repos && entry.file_name() == OsStr::new(".git") {
                return Some(current.to_path_buf());
            }
            // `symlink_metadata`, never `metadata`: following a link would count bytes
            // that live somewhere else and, if it pointed upward, would not terminate.
            let Ok(metadata) = path.symlink_metadata() else {
                pass.unreadable.push(path);
                continue;
            };
            if self.same_file_system && device(&metadata) != pass.boundary {
                // A mount point. `measure` may pass over one silently, because there it
                // only makes a size a lower bound. A survey may not: everything under the
                // mount is unseen, including a `.git`, so passing over it silently would
                // let "holds no repository" be asserted about ground nobody looked at.
                if pass.watch_for_repos && metadata.is_dir() {
                    pass.not_crossed.push(path);
                }
                continue;
            }
            if let Some(identity) = multiply_linked(&metadata) {
                if !pass.linked.insert(identity) {
                    continue;
                }
            }
            pass.bytes += allocated(&metadata);
            if metadata.is_dir() {
                pass.stack.push(path);
            }
        }
        None
    }
}

/// The running state of one traversal.
#[derive(Debug, Default)]
struct Pass {
    bytes: u64,
    boundary: u64,
    watch_for_repos: bool,
    unreadable: Vec<PathBuf>,
    not_crossed: Vec<PathBuf>,
    /// Multiply-linked files, so a hard-linked artefact is counted once per claim rather than
    /// once per link. Only populated by files that actually carry more than one link, which on
    /// an ordinary tree is none of them.
    linked: HashSet<(u64, u64)>,
    stack: Vec<PathBuf>,
}

impl Pass {
    fn stopped_at(self, nested_repo: PathBuf) -> Walked {
        Walked {
            bytes: self.bytes,
            nested_repo: Some(nested_repo),
            unreadable: self.unreadable,
            not_crossed: self.not_crossed,
        }
    }

    fn finished(self) -> Walked {
        Walked {
            bytes: self.bytes,
            nested_repo: None,
            unreadable: self.unreadable,
            not_crossed: self.not_crossed,
        }
    }
}

/// What one traversal came back with.
struct Walked {
    bytes: u64,
    /// The directory holding the `.git` that stopped the walk, when one did. `bytes` is then a
    /// lower bound rather than a total.
    nested_repo: Option<PathBuf>,
    unreadable: Vec<PathBuf>,
    not_crossed: Vec<PathBuf>,
}

/// The stat fields the byte accounting needs, from whichever `stat` produced them.
///
/// There are two, and they are not interchangeable types. The measurer walks by *path* and so
/// holds [`std::fs::Metadata`]; the deleter walks by *descriptor* and so holds
/// `cap_primitives`' metadata, which is what an `fstatat` against an open directory returns.
/// The rules below — allocated blocks, a hard link counted once — have to be the same for
/// both, or a plan's estimate and the bytes it reports freeing would be measured differently.
#[cfg(unix)]
pub(crate) trait Stat {
    fn is_dir(&self) -> bool;
    fn dev(&self) -> u64;
    fn ino(&self) -> u64;
    fn nlink(&self) -> u64;
    fn blocks(&self) -> u64;
}

#[cfg(unix)]
impl Stat for fs::Metadata {
    fn is_dir(&self) -> bool {
        Self::is_dir(self)
    }
    fn dev(&self) -> u64 {
        std::os::unix::fs::MetadataExt::dev(self)
    }
    fn ino(&self) -> u64 {
        std::os::unix::fs::MetadataExt::ino(self)
    }
    fn nlink(&self) -> u64 {
        std::os::unix::fs::MetadataExt::nlink(self)
    }
    fn blocks(&self) -> u64 {
        std::os::unix::fs::MetadataExt::blocks(self)
    }
}

#[cfg(unix)]
impl Stat for cap_primitives::fs::Metadata {
    fn is_dir(&self) -> bool {
        Self::is_dir(self)
    }
    fn dev(&self) -> u64 {
        cap_primitives::fs::MetadataExt::dev(self)
    }
    fn ino(&self) -> u64 {
        cap_primitives::fs::MetadataExt::ino(self)
    }
    fn nlink(&self) -> u64 {
        cap_primitives::fs::MetadataExt::nlink(self)
    }
    fn blocks(&self) -> u64 {
        cap_primitives::fs::MetadataExt::blocks(self)
    }
}

/// The same two sources, where there are no block or link counts to be had.
#[cfg(not(unix))]
pub(crate) trait Stat {
    fn is_dir(&self) -> bool;
    fn apparent_len(&self) -> u64;
}

#[cfg(not(unix))]
impl Stat for fs::Metadata {
    fn is_dir(&self) -> bool {
        Self::is_dir(self)
    }
    fn apparent_len(&self) -> u64 {
        self.len()
    }
}

#[cfg(not(unix))]
impl Stat for cap_primitives::fs::Metadata {
    fn is_dir(&self) -> bool {
        Self::is_dir(self)
    }
    fn apparent_len(&self) -> u64 {
        self.len()
    }
}

/// Bytes actually allocated on disk, which is what deleting gives back.
#[cfg(unix)]
pub(crate) fn allocated(stat: &impl Stat) -> u64 {
    stat.blocks() * 512
}

#[cfg(not(unix))]
pub(crate) fn allocated(stat: &impl Stat) -> u64 {
    stat.apparent_len()
}

#[cfg(unix)]
pub(crate) fn device(stat: &impl Stat) -> u64 {
    stat.dev()
}

#[cfg(not(unix))]
pub(crate) fn device(_stat: &impl Stat) -> u64 {
    0
}

/// What names a directory for as long as it exists, which a *path* does not.
///
/// The deleter records this for the scan root while the plan is built and checks it against the
/// descriptor it later opens, because the root is the one name that still has to be resolved
/// and a renamed-away root can be replaced by something that answers to the same name on the
/// same device.
///
/// `None` off unix, where there is no stable pair to be had. The whole `st_dev` family of
/// checks is equally inert there — see [`device`] — and the crate claims macOS and Linux.
// The `Option` is not redundant: it is `None` in the `not(unix)` arm below, and a caller has
// to be able to tell "this platform cannot answer" from an answer.
#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn identity(stat: &impl Stat) -> Option<(u64, u64)> {
    Some((stat.dev(), stat.ino()))
}

#[cfg(not(unix))]
pub(crate) fn identity(_stat: &impl Stat) -> Option<(u64, u64)> {
    None
}

/// The `(device, inode)` identity of a file with more than one hard link, or `None` when it
/// has exactly one and cannot be double-counted.
///
/// Deduplicating here makes a claim's total agree with `du`, which is the number the user
/// will check it against. It still overstates a pnpm `node_modules`, whose links point into
/// a store *outside* the claim: deleting the tree frees only the links. Answering that would
/// mean proving no link lives elsewhere, which costs a scan of the whole filesystem.
#[cfg(unix)]
pub(crate) fn multiply_linked(stat: &impl Stat) -> Option<(u64, u64)> {
    (stat.nlink() > 1 && !stat.is_dir()).then(|| (stat.dev(), stat.ino()))
}

#[cfg(not(unix))]
pub(crate) fn multiply_linked(_stat: &impl Stat) -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::{Measurer, Size, SizeMode};
    use std::path::Path;
    use std::{fs, io};
    use tempfile::TempDir;

    fn write(dir: &TempDir, name: &str, bytes: usize) {
        let path = dir.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    #[test]
    fn a_breakdown_sums_the_whole_subtree() {
        let tmp = TempDir::new().unwrap();
        write(&tmp, "a/one.bin", 64 * 1024);
        write(&tmp, "a/b/two.bin", 64 * 1024);

        let metadata = tmp.path().symlink_metadata().unwrap();
        let measured = Measurer::new(SizeMode::Breakdown).measure(tmp.path(), &metadata);

        assert!(measured.size.bytes().unwrap() >= 128 * 1024, "{measured:?}");
        assert!(measured.unreadable.is_empty());
    }

    #[test]
    fn the_default_mode_reports_unmeasured_without_reading_anything() {
        let tmp = TempDir::new().unwrap();
        write(&tmp, "a/big.bin", 4 * 1024 * 1024);
        // Unreadable, so any traversal would have to report it. Silence is the proof.
        let sealed = tmp.path().join("sealed");
        fs::create_dir(&sealed).unwrap();
        seal(&sealed);

        let metadata = tmp.path().symlink_metadata().unwrap();
        let measured = Measurer::new(SizeMode::Skip).measure(tmp.path(), &metadata);
        unseal(&sealed);

        assert_eq!(measured.size, Size::Unmeasured);
        assert!(measured.unreadable.is_empty(), "{measured:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_breakdown_reports_what_it_could_not_read() {
        let tmp = TempDir::new().unwrap();
        let sealed = tmp.path().join("sealed");
        fs::create_dir(&sealed).unwrap();
        seal(&sealed);
        if fs::read_dir(&sealed).is_ok() {
            unseal(&sealed);
            return; // running as root, where permissions prove nothing
        }

        let metadata = tmp.path().symlink_metadata().unwrap();
        let measured = Measurer::new(SizeMode::Breakdown).measure(tmp.path(), &metadata);
        unseal(&sealed);

        assert_eq!(measured.unreadable, [sealed]);
    }

    #[cfg(unix)]
    #[test]
    fn a_hard_linked_file_is_counted_once() {
        let tmp = TempDir::new().unwrap();
        write(&tmp, "a/artifact.bin", 512 * 1024);
        let metadata = tmp.path().symlink_metadata().unwrap();
        let measurer = Measurer::new(SizeMode::Breakdown);
        let once = measurer.measure(tmp.path(), &metadata).size;

        fs::hard_link(
            tmp.path().join("a/artifact.bin"),
            tmp.path().join("a/copy.bin"),
        )
        .unwrap();
        let twice = measurer.measure(tmp.path(), &metadata).size;

        assert_eq!(once, twice, "the second link added its blocks again");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_tree_is_worth_its_own_inode_only() {
        let tmp = TempDir::new().unwrap();
        write(&tmp, "elsewhere/big.bin", 4 * 1024 * 1024);
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(tmp.path().join("elsewhere"), &link).unwrap();

        let metadata = link.symlink_metadata().unwrap();
        // Even the default mode measures a link: one `lstat` is the whole truth about it.
        let measured = Measurer::new(SizeMode::Skip).measure(&link, &metadata);

        assert!(measured.size.bytes().unwrap() < 1024 * 1024, "{measured:?}");
    }

    #[test]
    fn a_scoped_breakdown_prices_what_is_under_the_scope_and_leaves_the_rest_alone() {
        let tmp = TempDir::new().unwrap();
        write(&tmp, "wanted/big.bin", 256 * 1024);
        write(&tmp, "elsewhere/big.bin", 256 * 1024);
        let wanted = tmp.path().join("wanted");
        let elsewhere = tmp.path().join("elsewhere");

        let measurer = Measurer::new(SizeMode::BreakdownUnder(wanted.clone()));

        let priced = measurer.measure(&wanted, &wanted.symlink_metadata().unwrap());
        assert!(priced.size.bytes().unwrap() >= 256 * 1024, "{priced:?}");
        // The whole point of the scope: everything outside it costs nothing, so a user can
        // price one subtree without paying for the tree.
        let untouched = measurer.measure(&elsewhere, &elsewhere.symlink_metadata().unwrap());
        assert_eq!(untouched.size, Size::Unmeasured);
    }

    #[test]
    fn a_scope_inside_a_claim_prices_that_claim_rather_than_nothing() {
        // Drilling into `node_modules/.pnpm` and being told the whole scan is unpriced is the
        // failure this codebase keeps finding in other clothes: a confident empty answer. A
        // claim is the smallest thing that can be priced, so a scope that lands inside one is
        // a request to price it.
        let tmp = TempDir::new().unwrap();
        write(&tmp, "claim/deep/big.bin", 256 * 1024);
        let claim = tmp.path().join("claim");

        let measurer = Measurer::new(SizeMode::BreakdownUnder(claim.join("deep")));
        let priced = measurer.measure(&claim, &claim.symlink_metadata().unwrap());

        assert!(priced.size.bytes().unwrap() >= 256 * 1024, "{priced:?}");
    }

    #[test]
    fn a_survey_prices_the_whole_subtree_whatever_the_mode() {
        let tmp = TempDir::new().unwrap();
        write(&tmp, "a/one.bin", 256 * 1024);
        write(&tmp, "a/b/two.bin", 256 * 1024);

        let metadata = tmp.path().symlink_metadata().unwrap();
        for mode in [SizeMode::Skip, SizeMode::Breakdown] {
            let surveyed = Measurer::new(mode.clone()).survey(tmp.path(), &metadata);
            assert!(surveyed.nested_repo.is_none());
            assert!(
                surveyed.size.bytes().unwrap() >= 512 * 1024,
                "{mode:?}: {surveyed:?}"
            );
        }
    }

    #[test]
    fn a_survey_stops_at_the_first_checkout_it_finds() {
        let tmp = TempDir::new().unwrap();
        let checkout = tmp.path().join("deep/checkout");
        fs::create_dir_all(checkout.join(".git")).unwrap();
        write(&tmp, "deep/checkout/src/main.rs", 1024);

        let metadata = tmp.path().symlink_metadata().unwrap();
        let surveyed = Measurer::new(SizeMode::Breakdown).survey(tmp.path(), &metadata);

        assert_eq!(surveyed.nested_repo.as_deref(), Some(checkout.as_path()));
        // The total is meaningless once the answer is "not removable", and reporting a lower
        // bound as if it were a size would be worse than reporting nothing.
        assert_eq!(surveyed.size, Size::Unmeasured);
    }

    #[test]
    fn a_directory_that_stops_listing_part_way_through_is_reported_as_unread() {
        // `readdir` can fail after the directory was opened, and the listing is then short by
        // an unknown amount. Skipping the entry — which an earlier version did — left the
        // survey looking complete, so a caller would go on to claim a directory nobody had
        // finished reading. No filesystem can be talked into this on demand, hence the
        // hand-made iterator.
        let tmp = TempDir::new().unwrap();
        let mut pass = super::Pass {
            watch_for_repos: true,
            ..super::Pass::default()
        };
        let entries = vec![Err(io::Error::other("readdir gave up"))];

        let stopped = Measurer::new(SizeMode::Skip).absorb(tmp.path(), entries, &mut pass);

        assert!(stopped.is_none());
        assert_eq!(pass.unreadable, [tmp.path().to_path_buf()]);
    }

    #[test]
    fn a_survey_reports_a_subtree_it_will_not_cross_rather_than_passing_over_it() {
        let tmp = TempDir::new().unwrap();
        // A checkout two levels down, behind what will look like a mount point.
        fs::create_dir_all(tmp.path().join("mounted/checkout/.git")).unwrap();

        // `survey` takes the caller's metadata for `dir`, and the boundary it will not cross
        // comes from that. Handing it metadata from another device is therefore the same
        // situation the walker meets when a mount point sits inside a candidate — without
        // needing a real mount, which no portable test can arrange.
        let here = tmp.path().symlink_metadata().unwrap();
        let elsewhere = Path::new("/dev").symlink_metadata().unwrap();
        if super::device(&here) == super::device(&elsewhere) {
            return; // one filesystem on this machine, so there is no boundary to prove
        }

        let surveyed = Measurer::new(SizeMode::Skip).survey(tmp.path(), &elsewhere);

        // The checkout is on the far side, so the survey genuinely did not see it. Saying so is
        // the whole point: silence here would let "holds no repository" be asserted about
        // ground nobody looked at, and the caller would claim the directory.
        assert!(surveyed.nested_repo.is_none());
        assert_eq!(surveyed.not_crossed, [tmp.path().join("mounted")]);
    }

    #[test]
    fn a_dot_git_file_marks_a_checkout_just_as_a_directory_does() {
        // A linked work tree and a submodule both keep a `.git` *file* naming the real gitdir.
        let tmp = TempDir::new().unwrap();
        write(&tmp, "worktree/.git", 64);

        let metadata = tmp.path().symlink_metadata().unwrap();
        let surveyed = Measurer::new(SizeMode::Skip).survey(tmp.path(), &metadata);

        assert_eq!(
            surveyed.nested_repo.as_deref(),
            Some(tmp.path().join("worktree").as_path())
        );
    }

    /// Makes a directory unreadable, so that any traversal of it has to report a failure.
    #[cfg(unix)]
    fn seal(dir: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o000)).unwrap();
    }

    /// Puts the permissions back, so the temporary directory can still be cleaned up.
    #[cfg(unix)]
    fn unseal(dir: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(not(unix))]
    fn seal(_dir: &std::path::Path) {}

    #[cfg(not(unix))]
    fn unseal(_dir: &std::path::Path) {}
}
