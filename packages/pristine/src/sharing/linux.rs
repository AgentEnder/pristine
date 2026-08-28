//! btrfs, XFS and bcachefs, which answer the question one extent at a time.
//!
//! There is no `privatesize` here. `FIEMAP` maps a file's logical bytes onto physical extents
//! and flags the ones the filesystem knows are shared, so the private total is what the
//! unflagged extents add up to — the same number APFS hands over whole.
//!
//! ## Why most Linux machines never reach the ioctl
//!
//! Only a filesystem that can share extents can have any. ext4 cannot, and ext4 is where pnpm
//! falls back to **hard links** — a form of sharing this module deliberately does not see and
//! [`crate::size`] handles from `st_nlink` on every platform. So the filesystem is checked
//! first and the ioctl is skipped unless it could return something, which keeps the cost off
//! the machines that would only ever have learnt "all of it".
//!
//! That check is also what keeps this affordable at all: `FIEMAP` needs the file *open*, where
//! `getattrlist` needs only its name. One `open`/`ioctl`/`close` per file is a far worse trade
//! than one extra `stat`, and it is not one an ext4 user should pay to be told nothing.

use std::ffi::{CString, OsStr};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use super::Sharing;

/// `_IOWR('f', 11, struct fiemap)`.
const FS_IOC_FIEMAP: libc::c_ulong = 0xC020_660B;
/// This is the last extent in the file. Without it the map is partial.
const FIEMAP_EXTENT_LAST: u32 = 0x0001;
/// The filesystem does not know where these bytes are — not yet allocated, or unreadable.
const FIEMAP_EXTENT_UNKNOWN: u32 = 0x0002;
/// Written but not yet on disk, so it has no physical home to be shared out of.
const FIEMAP_EXTENT_DELALLOC: u32 = 0x0004;
/// Something else references these blocks. The whole reason for this file.
const FIEMAP_EXTENT_SHARED: u32 = 0x2000;

/// Filesystems that can share an extent between two files. Anywhere else the answer is
/// "all of it" without asking.
const BTRFS: i64 = 0x9123_683E;
const XFS: i64 = 0x5846_5342;
const BCACHEFS: i64 = 0xca45_1a4e_u32 as i64;

/// Extents per ioctl. One round trip covers an ordinary file whole; a heavily fragmented one
/// costs a few more rather than one per extent.
const BATCH: usize = 64;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Extent {
    logical: u64,
    physical: u64,
    length: u64,
    reserved64: [u64; 2],
    flags: u32,
    reserved: [u32; 3],
}

#[repr(C)]
struct Fiemap {
    start: u64,
    length: u64,
    flags: u32,
    mapped_extents: u32,
    extent_count: u32,
    reserved: u32,
    extents: [Extent; BATCH],
}

pub(super) fn of(path: &Path, allocated: u64) -> Sharing {
    // `O_NOFOLLOW` because the walk has already `lstat`ed this name and a symlink is worth its
    // own inode, never its target's. `O_NONBLOCK` because a name that turns out to be a fifo
    // or a device must not park the walker on an open that never returns.
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path);
    opened.map_or_else(
        |_| Sharing::owned(allocated),
        |file| measure(&file, allocated),
    )
}

pub(super) fn at(dir: &File, name: &OsStr, allocated: u64) -> Sharing {
    let Ok(name) = CString::new(name.as_bytes()) else {
        return Sharing::owned(allocated);
    };
    open_at(dir, &name).map_or_else(
        || Sharing::owned(allocated),
        |file| measure(&file, allocated),
    )
}

/// The same `openat` the deleter's own traversal uses, against the descriptor it already
/// holds — never a path resolved afresh, which is a name that could have become something
/// else since it was walked.
#[allow(
    unsafe_code,
    reason = "openat(2) against a borrowed descriptor; see sharing::mod"
)]
fn open_at(dir: &File, name: &CString) -> Option<File> {
    // SAFETY: `dir` owns a live descriptor for the call and `name` is NUL-terminated.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return None;
    }
    // SAFETY: `openat` returned a descriptor it no longer owns, so taking it is sound and
    // closing it is now this `File`'s job.
    Some(File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
}

/// What an opened file is worth.
fn measure(file: &File, allocated: u64) -> Sharing {
    if !can_share(file) {
        return Sharing::owned(allocated);
    }
    map(file).map_or_else(
        || Sharing::owned(allocated),
        |mapped| Sharing {
            // Extent lengths are logical and `allocated` is blocks; on a compressed btrfs file the
            // logical total is the larger, and a private figure above what the file is billed for
            // would credit a deletion with blocks it never referenced.
            private: mapped.private.min(allocated),
            family: mapped.family,
        },
    )
}

/// What one file's extents added up to.
struct Mapped {
    private: u64,
    /// The physical address of the first shared extent, standing in for APFS's clone id: two
    /// files sharing an extent share its address, which is what makes them nameable as peers.
    family: Option<u64>,
}

/// Whether this file's filesystem can share extents at all.
#[allow(
    unsafe_code,
    reason = "fstatfs(2) has no safe wrapper; see sharing::mod for why the crate needs one"
)]
fn can_share(file: &File) -> bool {
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `file` owns a live descriptor for the whole call, and `stat` is a correctly
    // sized and aligned `statfs` the kernel either fills completely or leaves alone on error.
    let outcome = unsafe { libc::fstatfs(file.as_raw_fd(), stat.as_mut_ptr()) };
    if outcome != 0 {
        return false;
    }
    // SAFETY: `fstatfs` returned 0, so it filled the buffer.
    let kind = i64::from(unsafe { stat.assume_init() }.f_type);
    matches!(kind, BTRFS | XFS | BCACHEFS)
}

/// Walks the file's extents, or `None` when the map came back incomplete.
///
/// Incomplete means unusable rather than approximate. A partial map's private total is a
/// *lower* bound, and the one direction this crate must never round in is the one that makes a
/// claim look cheaper than it is — so a map that never reached its last extent is discarded
/// and the caller falls back to "owns all of it".
#[allow(
    unsafe_code,
    reason = "FS_IOC_FIEMAP is an ioctl; see sharing::mod for why the crate needs one"
)]
fn map(file: &File) -> Option<Mapped> {
    let mut mapped = Mapped {
        private: 0,
        family: None,
    };
    let mut start = 0_u64;

    loop {
        let mut request = Fiemap {
            start,
            length: u64::MAX,
            flags: 0,
            mapped_extents: 0,
            extent_count: BATCH as u32,
            reserved: 0,
            extents: [Extent::default(); BATCH],
        };
        // SAFETY: `file` owns a live descriptor for the whole call, and `request` is a
        // `fiemap` header immediately followed by exactly the `extent_count` extents it
        // declares, which is the layout the ioctl writes into.
        let outcome = unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                FS_IOC_FIEMAP,
                std::ptr::from_mut(&mut request),
            )
        };
        if outcome != 0 {
            return None;
        }

        let count = (request.mapped_extents as usize).min(BATCH);
        if count == 0 {
            // A hole runs to the end of the file, so there is nothing further to map and
            // nothing further to charge for.
            return Some(mapped);
        }

        for extent in &request.extents[..count] {
            if extent.flags & FIEMAP_EXTENT_SHARED == 0 {
                mapped.private += extent.length;
            } else if mapped.family.is_none() {
                mapped.family = Some(extent.physical);
            }
            // Bytes with no physical home yet cannot be shared out of one, whatever the
            // shared flag says. Counting them as private is the safe reading: it can only
            // make a claim look bigger, never smaller.
            if extent.flags & (FIEMAP_EXTENT_UNKNOWN | FIEMAP_EXTENT_DELALLOC) != 0
                && extent.flags & FIEMAP_EXTENT_SHARED != 0
            {
                mapped.private += extent.length;
            }
        }

        let last = request.extents[count - 1];
        if last.flags & FIEMAP_EXTENT_LAST != 0 {
            return Some(mapped);
        }
        let next = last.logical.checked_add(last.length)?;
        // A filesystem that answered without advancing would loop here forever, and a walker
        // thread that never returns is worse than an unmeasured claim.
        if next <= start {
            return None;
        }
        start = next;
    }
}
