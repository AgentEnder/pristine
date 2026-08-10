//! How many bytes a claimed directory is actually worth.
//!
//! ## Why the default traverses, and what the design doc says
//!
//! The concept doc asks for "sizes from the directory's own block accounting where the
//! platform offers it; a full walk only when the user asks for a breakdown". No platform
//! pristine targets offers it. A directory inode's block count on APFS, ext4, btrfs and ZFS
//! alike describes the directory *entry table*, not the tree beneath it — that is why `du`
//! walks — so taking it literally would report a 40 GB `node_modules` as about 48 KB and
//! make the rollup tree's headline number meaningless. [`SizeMode::DirectoryOnly`] exposes
//! that reading for anyone who wants instant, honest-but-useless numbers; the default is
//! [`SizeMode::Recursive`].
//!
//! The performance thesis survives intact, because it was never really about *whether* the
//! subtree is visited but about *how*. npkill sizes `node_modules` by running it through the
//! same scan that found it. pristine prunes there and hands the subtree to the tight
//! `read_dir` + `lstat` loop below: no ignore stack, no rule evaluation, no path
//! bookkeeping, one pass, and each claim measured on the walker thread that found it, so the
//! measurements are already spread across the pool.
//!
//! Bytes are *allocated* bytes (`st_blocks * 512`) rather than apparent length, because
//! allocated is what deleting the tree gives back.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// How much work a measurement is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SizeMode {
    /// Sum the allocated blocks of everything beneath the directory. One metadata-only pass.
    #[default]
    Recursive,
    /// The directory's own inode allocation, with no traversal at all. Constant time, and
    /// describes the directory entry table rather than its contents.
    DirectoryOnly,
}

/// The result of measuring one directory.
#[derive(Debug, Clone, Default)]
pub struct Measurement {
    /// Allocated bytes.
    pub bytes: u64,
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
    #[must_use]
    pub fn measure(&self, dir: &Path, metadata: &fs::Metadata) -> Measurement {
        let mut measurement = Measurement {
            bytes: allocated(metadata),
            unreadable: Vec::new(),
        };
        // A symlinked claim (Bazel's `bazel-*`) is worth its own inode and nothing more: the
        // bytes are in the output base, which is outside the tree and not ours to delete.
        if self.mode == SizeMode::DirectoryOnly || !metadata.is_dir() {
            return measurement;
        }

        let boundary = device(metadata);
        // Multiply-linked files, so a hard-linked artefact is counted once per claim rather
        // than once per link. Only populated by files that actually carry more than one
        // link, which on an ordinary tree is none of them.
        let mut linked = HashSet::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(current) = stack.pop() {
            let Ok(entries) = fs::read_dir(&current) else {
                measurement.unreadable.push(current);
                continue;
            };
            for entry in entries {
                let Ok(entry) = entry else { continue };
                let path = entry.path();
                // `symlink_metadata`, never `metadata`: following a link would count bytes
                // that live somewhere else and, if it pointed upward, would not terminate.
                let Ok(metadata) = entry.path().symlink_metadata() else {
                    measurement.unreadable.push(path);
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
                measurement.bytes += allocated(&metadata);
                if metadata.is_dir() {
                    stack.push(path);
                }
            }
        }
        measurement
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
    use super::{Measurer, SizeMode};
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &TempDir, name: &str, bytes: usize) {
        let path = dir.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    #[test]
    fn a_recursive_measurement_sums_the_whole_subtree() {
        let tmp = TempDir::new().unwrap();
        write(&tmp, "a/one.bin", 64 * 1024);
        write(&tmp, "a/b/two.bin", 64 * 1024);

        let metadata = tmp.path().symlink_metadata().unwrap();
        let measured = Measurer::new(SizeMode::Recursive).measure(tmp.path(), &metadata);

        assert!(measured.bytes >= 128 * 1024, "got {}", measured.bytes);
        assert!(measured.unreadable.is_empty());
    }

    #[test]
    fn a_directory_only_measurement_ignores_the_contents() {
        let tmp = TempDir::new().unwrap();
        write(&tmp, "a/big.bin", 4 * 1024 * 1024);

        let metadata = tmp.path().symlink_metadata().unwrap();
        let measured = Measurer::new(SizeMode::DirectoryOnly).measure(tmp.path(), &metadata);

        assert!(measured.bytes < 1024 * 1024, "got {}", measured.bytes);
    }

    #[cfg(unix)]
    #[test]
    fn a_hard_linked_file_is_counted_once() {
        let tmp = TempDir::new().unwrap();
        write(&tmp, "a/artifact.bin", 512 * 1024);
        let metadata = tmp.path().symlink_metadata().unwrap();
        let measurer = Measurer::new(SizeMode::Recursive);
        let once = measurer.measure(tmp.path(), &metadata).bytes;

        fs::hard_link(
            tmp.path().join("a/artifact.bin"),
            tmp.path().join("a/copy.bin"),
        )
        .unwrap();
        let twice = measurer.measure(tmp.path(), &metadata).bytes;

        assert_eq!(
            once, twice,
            "the second link added its target's blocks again"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_tree_is_worth_its_own_inode_only() {
        let tmp = TempDir::new().unwrap();
        write(&tmp, "elsewhere/big.bin", 4 * 1024 * 1024);
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(tmp.path().join("elsewhere"), &link).unwrap();

        let metadata = link.symlink_metadata().unwrap();
        let measured = Measurer::new(SizeMode::Recursive).measure(&link, &metadata);

        assert!(
            measured.bytes < 1024 * 1024,
            "the link was followed: {}",
            measured.bytes
        );
    }
}
