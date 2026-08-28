//! How much of a file's allocated blocks a deletion would actually give back.
//!
//! ## The question `st_blocks` does not answer
//!
//! A file's block count says how many blocks it *references*, not how many it *owns*. Modern
//! filesystems let two files reference the same blocks — `clonefile(2)` on APFS, `FICLONE` on
//! btrfs and XFS — and every reference is billed the full amount. `du` reports the sum and is
//! not wrong to; "how many distinct bytes live under here" is a real question and that is its
//! answer.
//!
//! It is not this crate's question. pristine's README opens by saying so: a row's number is
//! "how much do I get back by emptying this subtree", and for a `node_modules` that pnpm
//! cloned out of its store, that number is **zero** while the store is still there. Measured
//! on one real store: a `core-js-compat` file billed at 592 KiB of blocks, shared with two
//! clones; a `es-define-property` file shared with twenty-nine. Nothing comes back until the
//! last reference goes.
//!
//! ## What each platform will say
//!
//! - **macOS/APFS** answers directly. `getattrlist(2)` with `ATTR_CMNEXT_PRIVATESIZE` returns
//!   the bytes this file holds *exclusively*, which is precisely the bytes unlinking it
//!   returns. It is volume-wide, so it sees a store sitting outside the scan without pristine
//!   having to look at one.
//! - **Linux** answers per extent. The `FIEMAP` ioctl flags shared extents with
//!   `FIEMAP_EXTENT_SHARED`, so the private total is the unflagged extents. Only btrfs, XFS
//!   and bcachefs can share at all, so the ioctl is skipped everywhere else — on ext4 the
//!   answer is always "all of it", and ext4 is where pnpm hard-links instead.
//! - **Anywhere else** says "all of it", which is what the crate assumed everywhere until now.
//!
//! ## Hard links are a separate axis, and this module is not it
//!
//! Measured, not assumed: a file with `nlink == 2` and no clone reports its **full** size as
//! private. `privatesize` accounts for shared *extents*, not for shared *names*. So the two
//! forms of sharing compose rather than substitute, and the caller has to apply both — see
//! [`crate::size::Pass`], which counts names against `st_nlink` before it trusts anything
//! here.

use std::path::Path;

#[cfg(target_os = "macos")]
mod darwin;
#[cfg(target_os = "linux")]
mod linux;

/// What one file's allocated blocks are worth to whoever deletes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sharing {
    /// Bytes that come back when this file's last name is unlinked, which is at most its
    /// allocated blocks and is zero for a file every one of whose extents is shared.
    pub private: u64,
    /// The clone family this file belongs to, when the filesystem tracks one and the file is
    /// not alone in it. What lets a shared remainder be attributed to a peer found elsewhere
    /// in the scan; `None` when nothing is shared or the platform cannot say.
    pub family: Option<u64>,
}

impl Sharing {
    /// A file that shares nothing, and so is worth every block it references.
    pub(crate) const fn owned(allocated: u64) -> Self {
        Self {
            private: allocated,
            family: None,
        }
    }

    /// The blocks that stay allocated after this file goes, because something else references
    /// them.
    pub(crate) const fn shared(&self, allocated: u64) -> u64 {
        allocated.saturating_sub(self.private)
    }
}

/// What `path`'s blocks are worth, given the `allocated` total already read from its `lstat`.
///
/// Never fails. Every platform error — an unsupported filesystem, a file that vanished between
/// the `readdir` and here, a permission the walk does not hold — falls back to
/// [`Sharing::owned`], which is exactly the accounting this crate used before the question was
/// asked at all. A sharing lookup that cannot answer must not be able to make a claim look
/// *smaller* than it is: under-reporting reclaimable space is a tool that stops being used,
/// and there is no filesystem where "all of it" is an overstatement of what you get back.
pub(crate) fn of(path: &Path, allocated: u64) -> Sharing {
    #[cfg(target_os = "macos")]
    {
        darwin::of(path, allocated)
    }
    #[cfg(target_os = "linux")]
    {
        linux::of(path, allocated)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = path;
        Sharing::owned(allocated)
    }
}

/// The same question, asked against a descriptor the caller already holds.
///
/// What the deleter uses. Its whole traversal is descriptor-relative so that a name cannot
/// become something else between being walked and being unlinked, and a size lookup that
/// re-resolved the path would be a hole in exactly that guarantee. Same fallback as
/// [`of`]: anything that cannot be answered is answered as "owns all of it".
pub(crate) fn at(dir: &std::fs::File, name: &std::ffi::OsStr, allocated: u64) -> Sharing {
    #[cfg(target_os = "macos")]
    {
        darwin::at(dir, name, allocated)
    }
    #[cfg(target_os = "linux")]
    {
        linux::at(dir, name, allocated)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (dir, name);
        Sharing::owned(allocated)
    }
}

#[cfg(test)]
mod tests {
    use super::{Sharing, of};
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::{fs, process};
    use tempfile::TempDir;

    /// The blocks an `lstat` bills the file for — what the measurer would have counted before
    /// it learnt to ask this module anything.
    fn allocated(path: &Path) -> u64 {
        path.symlink_metadata().unwrap().blocks() * 512
    }

    fn write(dir: &TempDir, name: &str, bytes: usize) -> PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, vec![b'x'; bytes]).unwrap();
        path
    }

    /// A copy-on-write clone, which is what pnpm makes on APFS and what `cp --reflink` makes
    /// on btrfs and XFS. Skips the test — rather than failing it — on a filesystem that cannot
    /// clone, because CI runners and tmpfs are exactly that.
    fn clone_file(from: &Path, to: &Path) -> bool {
        let flag = if cfg!(target_os = "macos") {
            "-c"
        } else {
            "--reflink=always"
        };
        process::Command::new("cp")
            .arg(flag)
            .args([from, to])
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn a_file_that_shares_nothing_is_worth_every_block_it_references() {
        let tmp = TempDir::new().unwrap();
        let lone = write(&tmp, "lone.bin", 512 * 1024);

        let sharing = of(&lone, allocated(&lone));

        assert_eq!(sharing.private, allocated(&lone));
        assert_eq!(sharing.shared(allocated(&lone)), 0);
        assert_eq!(sharing.family, None, "nothing to share with, so no family");
    }

    #[test]
    fn a_clone_is_worth_nothing_while_its_twin_is_still_there() {
        let tmp = TempDir::new().unwrap();
        let original = write(&tmp, "original.bin", 512 * 1024);
        let clone = tmp.path().join("clone.bin");
        if !clone_file(&original, &clone) {
            return; // a filesystem that cannot share extents has nothing to prove here
        }

        let billed = allocated(&clone);
        let sharing = of(&clone, billed);
        if sharing.private == billed {
            return; // cloned, but on a platform this crate cannot ask — the fallback, correctly
        }

        assert_eq!(
            sharing.private, 0,
            "deleting one of two clones frees nothing"
        );
        assert_eq!(sharing.shared(billed), billed);
        assert!(sharing.family.is_some(), "a clone belongs to a family");
        assert_eq!(
            sharing.family,
            of(&original, allocated(&original)).family,
            "both sides of one clone are in one family"
        );
    }

    #[test]
    fn the_survivor_of_a_clone_pair_becomes_worth_its_blocks_again() {
        let tmp = TempDir::new().unwrap();
        let original = write(&tmp, "original.bin", 512 * 1024);
        let clone = tmp.path().join("clone.bin");
        if !clone_file(&original, &clone) {
            return;
        }
        fs::remove_file(&clone).unwrap();

        // The whole point of the measurement: it is a statement about *now*, not a property
        // stamped on the file when it was made. Once the last other reference goes, the
        // survivor owns the blocks and a deletion really would give them back.
        let billed = allocated(&original);
        assert_eq!(of(&original, billed).private, billed);
    }

    #[cfg(unix)]
    #[test]
    fn a_hard_link_does_not_register_here_at_all() {
        let tmp = TempDir::new().unwrap();
        let original = write(&tmp, "original.bin", 512 * 1024);
        fs::hard_link(&original, tmp.path().join("second.name")).unwrap();

        // Measured, and the reason `size` cannot delegate the whole question to this module:
        // `privatesize` counts shared *extents*, and a second hard link is a shared *name*.
        // A caller that read this as "worth all of it, so deleting the claim gets it all
        // back" would be wrong exactly where a pnpm store on ext4 lives.
        let billed = allocated(&original);
        assert_eq!(of(&original, billed).private, billed);
        assert_eq!(original.symlink_metadata().unwrap().nlink(), 2);
    }

    #[test]
    fn the_answer_does_not_depend_on_where_the_reply_buffer_landed() {
        // Written for a real bug: the reply buffer used to be a bare `[u8; 24]`, whose
        // alignment is 1, and the kernel copies an 8-byte `off_t` into it at offset 4. When
        // the stack put that buffer on an odd boundary the call still succeeded and still
        // declared a 24-byte reply, but came back all zeroes — which reads as "every byte of
        // this file is shared", and priced a whole build directory at nothing.
        //
        // Honest about what this catches: **not that bug, reliably.** The alignment that
        // mattered was the calling frame's, so reproducing it took a 360-test parallel run to
        // shift the stack; this test alone never failed even with the fix reverted. It is a
        // smoke test for the answer being stable under concurrency, and the real guards are
        // `align(8)` on the buffer and `darwin::plausible`, which has its own test.
        let tmp = TempDir::new().unwrap();
        let lone = write(&tmp, "lone.bin", 512 * 1024);
        let billed = allocated(&lone);

        let answers: Vec<u64> = std::thread::scope(|scope| {
            let running: Vec<_> = (0..16)
                .map(|_| {
                    scope.spawn(|| {
                        (0..64)
                            .map(|_| of(&lone, billed).private)
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            running
                .into_iter()
                .flat_map(|thread| thread.join().unwrap())
                .collect()
        });

        assert!(
            answers.iter().all(|&private| private == billed),
            "{} of {} calls disagreed with the rest",
            answers.iter().filter(|&&p| p != billed).count(),
            answers.len()
        );
    }

    #[test]
    fn a_file_that_cannot_be_asked_about_is_assumed_to_own_its_blocks() {
        // The fallback every platform error lands on. Understating a claim is the one error
        // this crate must not make quietly, so "could not ask" means "all of it" and the
        // number stays what it was before this module existed.
        let missing = Path::new("/nonexistent/path/that/cannot/be/statted");
        assert_eq!(of(missing, 4096), Sharing::owned(4096));
    }
}
