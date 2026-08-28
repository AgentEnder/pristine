//! APFS, which answers the question directly.
//!
//! `ATTR_CMNEXT_PRIVATESIZE` is the bytes a file holds exclusively. Verified against the real
//! filesystem rather than inferred from the header, because two of the four results are not
//! what the names suggest:
//!
//! | file | `st_blocks` | `privatesize` | `clone_refcnt` |
//! |---|---|---|---|
//! | 10 MiB, alone | 10 MiB | 10 MiB | 1 |
//! | 10 MiB, plus a **hard link** | 10 MiB | **10 MiB** | **1** |
//! | 10 MiB, plus a **clone** | 10 MiB | **0** | 2 |
//! | …after 3 MiB of one side is rewritten | 10 MiB | **3 MiB** | **1** |
//!
//! Row two is why [`super`] insists hard links are a separate axis: `privatesize` does not
//! count names. Row four is why `clone_refcnt` is read but never trusted for arithmetic — a
//! partial rewrite drops it to 1 on both sides while 7 MiB is still shared, and only
//! `privatesize` stays right. The refcount is used for one thing: deciding whether there is a
//! clone family worth naming at all.

use std::ffi::CString;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::Sharing;

/// `struct attrlist`, as `<sys/attr.h>` lays it out.
#[repr(C)]
struct AttrList {
    bitmapcount: u16,
    reserved: u16,
    commonattr: u32,
    volattr: u32,
    dirattr: u32,
    fileattr: u32,
    forkattr: u32,
}

const ATTR_BIT_MAP_COUNT: u16 = 5;
const ATTR_CMNEXT_PRIVATESIZE: u32 = 0x0000_0008;
const ATTR_CMNEXT_CLONEID: u32 = 0x0000_0100;
const ATTR_CMNEXT_CLONE_REFCNT: u32 = 0x0000_1000;
const FSOPT_NOFOLLOW: u32 = 0x0000_0001;
const FSOPT_ATTR_CMN_EXTENDED: u32 = 0x0000_0020;

/// The reply is packed, with no padding between fields and none after the leading length:
/// `[u32 length][i64 privatesize][u64 cloneid][u32 clone_refcnt]`. Confirmed by asking for all
/// three at once and reading back `length == 24`, so these offsets are measured rather than
/// assumed from the field types.
const REPLY_BYTES: usize = 24;
const PRIVATESIZE_AT: usize = 4;
const CLONEID_AT: usize = 12;
const REFCNT_AT: usize = 20;

/// The reply buffer, and the `align(8)` is not decoration.
///
/// A bare `[u8; 24]` has alignment 1, and the kernel copies an 8-byte `off_t` into it at
/// offset 4. Where that landed depended on the stack frame, so the call succeeded, reported
/// `length == 24`, and returned a buffer that was **all zeroes past the length** — a file
/// reported as owning nothing while also reporting no other reference to it, which is not a
/// state APFS has. It reproduced only under a parallel test run, because that is what varied
/// the stack layout; the equivalent C never showed it, because a C compiler aligns a stack
/// array generously and hid the bug for free.
///
/// A zeroed reply is the worst possible failure here — it reads as "every byte of this is
/// shared", which is silently the *understating* direction. Hence the alignment, and hence
/// [`plausible`] as a second line of defence.
#[repr(C, align(8))]
struct Reply([u8; REPLY_BYTES]);

pub(super) fn of(path: &Path, allocated: u64) -> Sharing {
    // A path with an interior NUL never named a file, so there is nothing to ask about.
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return Sharing::owned(allocated);
    };
    interpret(ask(None, &path), allocated)
}

pub(super) fn at(dir: &std::fs::File, name: &std::ffi::OsStr, allocated: u64) -> Sharing {
    let Ok(name) = CString::new(name.as_bytes()) else {
        return Sharing::owned(allocated);
    };
    interpret(ask(Some(dir.as_raw_fd()), &name), allocated)
}

/// Turns a reply into an answer, or falls back to owning everything when there is none.
fn interpret(reply: Option<Reply>, allocated: u64) -> Sharing {
    let Some(reply) = reply else {
        return Sharing::owned(allocated);
    };

    let private = u64::from_le_bytes(read(&reply, PRIVATESIZE_AT));
    let cloneid = u64::from_le_bytes(read(&reply, CLONEID_AT));
    let refcnt = u32::from_le_bytes(read(&reply, REFCNT_AT));
    if !plausible(private, refcnt, allocated) {
        return Sharing::owned(allocated);
    }

    Sharing {
        // Clamped, because the two numbers come from different accountings: `privatesize` is
        // in logical bytes and `allocated` is blocks. They agree on an ordinary file and
        // diverge on a compressed or sparse one, where the logical figure is the larger — and
        // a private total above the allocated total would credit a deletion with blocks the
        // file never referenced.
        private: private.min(allocated),
        // One is this file and no one else, which is not a family. See the module note on why
        // this count is read but never added up.
        family: (refcnt > 1).then_some(cloneid),
    }
}

/// The one `getattrlist` call, and the only unsafe in the crate on this platform.
///
/// Returns `None` for every failure — a filesystem that does not implement the extended
/// common attributes, a file unlinked since the `readdir`, a permission the walk does not
/// hold. [`super::of`] turns that into "owns all of it", which is the pre-existing accounting.
#[allow(
    unsafe_code,
    reason = "getattrlist(2) is the only route to APFS clone accounting; see sharing::mod"
)]
fn ask(dir: Option<RawFd>, path: &CString) -> Option<Reply> {
    let mut request = AttrList {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: ATTR_CMNEXT_PRIVATESIZE | ATTR_CMNEXT_CLONEID | ATTR_CMNEXT_CLONE_REFCNT,
    };
    let mut reply = Reply([0_u8; REPLY_BYTES]);

    let options = FSOPT_NOFOLLOW | FSOPT_ATTR_CMN_EXTENDED;
    // SAFETY: `path` is a NUL-terminated C string that outlives the call, and where a
    // descriptor is given it is borrowed from a live `File` for the same span. `request` is a
    // fully initialised `attrlist` matching the kernel's layout, and `reply` is a local
    // buffer, 8-aligned for the `off_t` the kernel writes at offset 4, whose true size is
    // what is passed as `bufsize` — so nothing can be written past it.
    let outcome = unsafe {
        match dir {
            // `getattrlistat` against the descriptor the deleter already holds, never a path
            // resolved afresh. The whole traversal there is descriptor-relative on purpose,
            // and a name re-resolved to ask about its size is a name that could have become
            // something else in between.
            Some(dir) => libc::getattrlistat(
                dir,
                path.as_ptr(),
                std::ptr::from_mut(&mut request).cast::<libc::c_void>(),
                std::ptr::from_mut(&mut reply).cast::<libc::c_void>(),
                REPLY_BYTES,
                // Wider here than on `getattrlist`, which is the header's choice and not
                // ours; the flags are the same flags.
                u64::from(options),
            ),
            None => libc::getattrlist(
                path.as_ptr(),
                std::ptr::from_mut(&mut request).cast::<libc::c_void>(),
                std::ptr::from_mut(&mut reply).cast::<libc::c_void>(),
                REPLY_BYTES,
                options,
            ),
        }
    };
    if outcome != 0 {
        return None;
    }

    // A kernel that answered with fewer attributes than were asked for has not filled the
    // offsets below, and reading them would be reading zeroes as facts.
    let length = u32::from_le_bytes(read(&reply, 0));
    (length as usize == REPLY_BYTES).then_some(reply)
}

/// `N` bytes at `at`, which the callers only ever ask for inside a reply they have measured.
fn read<const N: usize>(reply: &Reply, at: usize) -> [u8; N] {
    let mut field = [0_u8; N];
    field.copy_from_slice(&reply.0[at..at + N]);
    field
}

/// Whether a reply is one APFS could actually have meant.
///
/// "Owns none of its blocks" and "nothing else references it" are each ordinary answers and
/// together they are a contradiction: blocks shared with nobody are blocks you own. Treating
/// the pair as a failure costs a correct answer for no file that exists, and catches a whole
/// class of half-written reply — the shape the alignment bug above produced — in the one
/// direction that would otherwise make a claim look worthless.
const fn plausible(private: u64, refcnt: u32, allocated: u64) -> bool {
    private > 0 || refcnt > 1 || allocated == 0
}

#[cfg(test)]
mod tests {
    use super::plausible;

    #[test]
    fn a_file_owning_nothing_that_nothing_else_references_is_not_an_answer() {
        // The shape a half-written reply takes, and the one this guard exists for: zeroes
        // everywhere. Read literally it says a 4 KiB file shares all 4 KiB with nobody.
        assert!(!plausible(0, 0, 4096));
        assert!(!plausible(0, 1, 4096));
    }

    #[test]
    fn owning_nothing_is_an_answer_when_something_else_holds_the_blocks() {
        // The pnpm case, and the whole point of the feature. It must survive the guard.
        assert!(plausible(0, 2, 4096));
        assert!(plausible(0, 29, 4096));
    }

    #[test]
    fn an_empty_file_owns_nothing_and_is_telling_the_truth() {
        // Zero blocks, zero private, no clone family. Every field is zero and all of it is
        // correct, which is why the guard tests the allocated total rather than the reply.
        assert!(plausible(0, 1, 0));
    }

    #[test]
    fn owning_some_of_itself_is_always_an_answer() {
        // Including the partial-rewrite case, where a file owns 3 MiB, still shares 7 MiB,
        // and APFS reports a refcount of 1 for both sides. Measured, not assumed — see the
        // table in the module note.
        assert!(plausible(3 * 1024 * 1024, 1, 10 * 1024 * 1024));
    }
}
