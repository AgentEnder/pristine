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

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

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
}

/// How much work a scan may do to size what it claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SizeMode {
    /// Record claims without enumerating them. The default, and the performance thesis.
    #[default]
    Skip,
    /// Sum each claim's subtree. What "show me a breakdown" costs.
    Breakdown,
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

/// Measures directories under a fixed policy.
#[derive(Debug, Clone, Copy)]
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
        if self.mode == SizeMode::Skip {
            return Measurement::default();
        }

        let mut bytes = allocated(metadata);
        let mut unreadable = Vec::new();
        let boundary = device(metadata);
        // Multiply-linked files, so a hard-linked artefact is counted once per claim rather
        // than once per link. Only populated by files that actually carry more than one
        // link, which on an ordinary tree is none of them.
        let mut linked = HashSet::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(current) = stack.pop() {
            let Ok(entries) = fs::read_dir(&current) else {
                unreadable.push(current);
                continue;
            };
            for entry in entries {
                let Ok(entry) = entry else { continue };
                let path = entry.path();
                // `symlink_metadata`, never `metadata`: following a link would count bytes
                // that live somewhere else and, if it pointed upward, would not terminate.
                let Ok(metadata) = entry.path().symlink_metadata() else {
                    unreadable.push(path);
                    continue;
                };
                if self.same_file_system && device(&metadata) != boundary {
                    continue;
                }
                if let Some(identity) = multiply_linked(&metadata) {
                    if !linked.insert(identity) {
                        continue;
                    }
                }
                bytes += allocated(&metadata);
                if metadata.is_dir() {
                    stack.push(path);
                }
            }
        }
        Measurement {
            size: Size::Measured(bytes),
            unreadable,
        }
    }
}

/// Bytes actually allocated on disk, which is what deleting gives back.
#[cfg(unix)]
fn allocated(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.blocks() * 512
}

#[cfg(not(unix))]
fn allocated(metadata: &fs::Metadata) -> u64 {
    metadata.len()
}

#[cfg(unix)]
fn device(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.dev()
}

#[cfg(not(unix))]
fn device(_metadata: &fs::Metadata) -> u64 {
    0
}

/// The `(device, inode)` identity of a file with more than one hard link, or `None` when it
/// has exactly one and cannot be double-counted.
///
/// Deduplicating here makes a claim's total agree with `du`, which is the number the user
/// will check it against. It still overstates a pnpm `node_modules`, whose links point into
/// a store *outside* the claim: deleting the tree frees only the links. Answering that would
/// mean proving no link lives elsewhere, which costs a scan of the whole filesystem.
#[cfg(unix)]
fn multiply_linked(metadata: &fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    (metadata.nlink() > 1 && !metadata.is_dir()).then(|| (metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn multiply_linked(_metadata: &fs::Metadata) -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::{Measurer, Size, SizeMode};
    use std::fs;
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
