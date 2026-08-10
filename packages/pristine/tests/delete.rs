//! What the deleter promises, stated as fixtures on a real filesystem.
//!
//! This is the irreversible half, so the tests that matter are the ones where the answer is
//! "no". Every one of them describes a way a directory could be removed that should not be,
//! and each is written so that removing the check makes it fail rather than making it slower.

// `allow-unwrap-in-tests` in clippy.toml only reaches code inside a `#[test]` function, and
// the fixture helpers below sit outside one. An unwrap in a fixture is an assertion.
#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use pristine::{Deleter, Plan, Planner, Refusal, Target};
use tempfile::TempDir;

/// Creates `path` and every parent, then writes `bytes` bytes of filler into it.
fn write(path: &Path, bytes: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![b'x'; bytes]).unwrap();
}

fn touch(path: &Path) {
    write(path, 0);
}

fn mkdir(path: &Path) {
    fs::create_dir_all(path).unwrap();
}

/// A temporary tree, and its path with every symlink already resolved.
///
/// On macOS `/var` is a symlink to `/private/var`, so `TempDir::path()` is not canonical. The
/// deleter reports RESOLVED paths — that is the whole point of the under-root check — so a
/// test comparing against the unresolved spelling compares two names for one directory.
fn fixture() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    (tmp, base)
}

/// A plan over `root` with the safety model's defaults.
fn plan_for(root: &Path, targets: &[PathBuf]) -> Plan {
    Planner::new(root).plan(targets.iter().map(Target::at))
}

/// The reason each refused path was left alone, in path order.
fn refusals(plan: &Plan) -> Vec<(PathBuf, Refusal)> {
    let mut kept: Vec<_> = plan
        .kept()
        .iter()
        .map(|refused| (refused.path.clone(), refused.reason.clone()))
        .collect();
    kept.sort_by(|a, b| a.0.cmp(&b.0));
    kept
}

/// The resolved target paths, in path order.
fn targets(plan: &Plan) -> Vec<PathBuf> {
    let mut paths: Vec<_> = plan
        .targets()
        .iter()
        .map(|target| target.path.clone())
        .collect();
    paths.sort();
    paths
}

/// Makes a directory unreadable, so any traversal of it has to report a failure. Returns
/// false when the process can read it anyway, which means it is running as root.
#[cfg(unix)]
fn seal(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o000)).unwrap();
    fs::read_dir(dir).is_err()
}

/// Puts the permissions back, so the temporary directory can still be cleaned up.
#[cfg(unix)]
fn unseal(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
}

// ---------------------------------------------------------------------------------------
// 1. Every target is resolved and proved to be under the scan root before any unlink.
// ---------------------------------------------------------------------------------------

#[test]
fn a_target_outside_the_scan_root_is_refused() {
    let (_tmp, base) = fixture();
    let root = base.join("root");
    let inside = root.join("app/node_modules");
    let outside = base.join("elsewhere/node_modules");
    touch(&inside.join("left-pad/index.js"));
    touch(&outside.join("index.js"));

    let plan = plan_for(&root, &[inside.clone(), outside.clone()]);

    assert_eq!(targets(&plan), [inside]);
    assert_eq!(refusals(&plan), [(outside, Refusal::OutsideRoot)]);
}

#[test]
fn a_target_that_climbs_out_of_the_root_with_dot_dot_is_refused() {
    let (_tmp, base) = fixture();
    let root = base.join("root");
    mkdir(&root);
    touch(&base.join("elsewhere/keep.txt"));

    let escaped = root.join("../elsewhere");
    let plan = plan_for(&root, std::slice::from_ref(&escaped));

    assert!(targets(&plan).is_empty());
    assert_eq!(refusals(&plan), [(escaped, Refusal::OutsideRoot)]);
    assert!(Deleter::new().remove(&plan).removed.is_empty());
    assert!(base.join("elsewhere/keep.txt").exists());
}

#[cfg(unix)]
#[test]
fn a_target_reached_through_a_symlinked_parent_is_judged_where_it_really_lives() {
    let (_tmp, base) = fixture();
    let root = base.join("root");
    mkdir(&root);
    touch(&base.join("elsewhere/target/keep.txt"));
    // A link inside the root pointing out of it. Read textually the target is under the
    // root; resolved it is not, and resolved is what counts.
    std::os::unix::fs::symlink(base.join("elsewhere"), root.join("outside")).unwrap();

    let escaped = root.join("outside/target");
    let plan = plan_for(&root, std::slice::from_ref(&escaped));

    assert!(targets(&plan).is_empty(), "{:?}", targets(&plan));
    assert_eq!(refusals(&plan), [(escaped, Refusal::OutsideRoot)]);
    assert!(Deleter::new().remove(&plan).removed.is_empty());
    assert!(base.join("elsewhere/target/keep.txt").exists());
}

#[test]
fn the_scan_root_itself_is_never_a_target() {
    let (_tmp, base) = fixture();
    let root = base.join("root");
    touch(&root.join("keep.txt"));

    let plan = plan_for(&root, std::slice::from_ref(&root));

    assert!(targets(&plan).is_empty());
    assert_eq!(refusals(&plan), [(root.clone(), Refusal::OutsideRoot)]);
    assert!(Deleter::new().remove(&plan).removed.is_empty());
    assert!(root.join("keep.txt").exists());
}

#[test]
fn a_target_inside_another_target_is_dropped_rather_than_failing_later() {
    let (_tmp, base) = fixture();
    let outer = base.join("app/node_modules");
    let inner = outer.join("dep/target");
    touch(&inner.join("build.o"));

    let plan = plan_for(&base, &[outer.clone(), inner.clone()]);

    // Removing the outer one makes the inner one vanish. Keeping both would report a
    // failure for a directory that is gone precisely because the plan worked.
    assert_eq!(
        refusals(&plan),
        [(inner, Refusal::AlreadyCovered(outer.clone()))]
    );
    assert_eq!(targets(&plan), [outer]);
}

#[test]
fn a_target_that_is_no_longer_there_is_reported_rather_than_silently_dropped() {
    let (_tmp, base) = fixture();
    let gone = base.join("app/node_modules");

    let plan = plan_for(&base, std::slice::from_ref(&gone));

    assert!(targets(&plan).is_empty());
    assert!(
        matches!(
            refusals(&plan).as_slice(),
            [(path, Refusal::Unreadable(_))] if path == &gone
        ),
        "{:?}",
        refusals(&plan)
    );
}

// ---------------------------------------------------------------------------------------
// 3. A filesystem boundary is not crossed unless the flag says to.
// ---------------------------------------------------------------------------------------

/// A path under `/` that is on a different filesystem, if this machine has one mounted.
///
/// `/dev` is devfs on macOS and devtmpfs on Linux, so normally it is one. A stripped
/// container may not mount anything, and then there is nothing here to prove.
#[cfg(unix)]
fn on_another_filesystem() -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let root = Path::new("/").symlink_metadata().ok()?.dev();
    ["/dev/null", "/dev", "/proc/self", "/sys"]
        .into_iter()
        .map(PathBuf::from)
        .find(|candidate| {
            candidate
                .symlink_metadata()
                .is_ok_and(|metadata| metadata.dev() != root)
        })
}

#[cfg(unix)]
#[test]
fn a_mount_point_is_refused_and_only_the_flag_lets_it_through() {
    let Some(elsewhere) = on_another_filesystem() else {
        return; // nothing else is mounted, so the boundary cannot be observed here
    };
    // Rooted at `/` so the target is genuinely under the root and the ONLY thing that can
    // stop it is the mount boundary. Building a plan reads metadata and nothing else — no
    // deleter is constructed here, which is why pointing one at `/` is safe.
    let plan = Planner::new("/").plan([Target::at(&elsewhere)]);
    assert!(plan.targets().is_empty(), "{:?}", targets(&plan));
    assert_eq!(
        refusals(&plan),
        [(elsewhere.clone(), Refusal::OtherFileSystem)]
    );

    let crossed = Planner::new("/")
        .one_file_system(false)
        .plan([Target::at(&elsewhere)]);
    assert_eq!(targets(&crossed), [elsewhere]);
    assert!(crossed.kept().is_empty(), "{:?}", crossed.kept());
}

// ---------------------------------------------------------------------------------------
// 2. Symlinks are never followed out of the root.
// ---------------------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn a_symlink_inside_a_target_is_unlinked_as_a_link_and_never_walked() {
    let (_tmp, base) = fixture();
    let outside = base.join("precious");
    touch(&outside.join("thesis.md"));
    let target = base.join("app/node_modules");
    touch(&target.join("dep/index.js"));
    std::os::unix::fs::symlink(&outside, target.join("dep/escape")).unwrap();

    let removal = Deleter::new().remove(&plan_for(&base, std::slice::from_ref(&target)));

    assert!(removal.failures.is_empty(), "{:?}", removal.failures);
    assert!(!target.exists(), "the target survived");
    assert!(
        outside.join("thesis.md").exists(),
        "the deleter walked through a symlink and out of the root"
    );
}

#[cfg(unix)]
#[test]
fn a_target_that_is_itself_a_symlink_is_unlinked_without_touching_what_it_points_at() {
    let (_tmp, base) = fixture();
    let outside = base.join("precious");
    touch(&outside.join("thesis.md"));
    // Bazel's `bazel-*` claims are exactly this shape: the claim is a link, and the bytes
    // are somewhere else and not ours.
    let target = base.join("repo/bazel-out");
    mkdir(target.parent().unwrap());
    std::os::unix::fs::symlink(&outside, &target).unwrap();

    let removal = Deleter::new().remove(&plan_for(&base, std::slice::from_ref(&target)));

    assert!(removal.failures.is_empty(), "{:?}", removal.failures);
    assert!(target.symlink_metadata().is_err(), "the link survived");
    assert!(outside.join("thesis.md").exists(), "the link was followed");
}

// ---------------------------------------------------------------------------------------
// 4. Nested git repositories are refused and reported, never swept up.
// ---------------------------------------------------------------------------------------

#[test]
fn a_checkout_inside_a_target_stops_the_removal_and_is_reported() {
    let (_tmp, base) = fixture();
    let target = base.join("ignored");
    touch(&target.join("junk/scratch.o"));
    let checkout = target.join("work/repo");
    touch(&checkout.join(".git/HEAD"));
    touch(&checkout.join("uncommitted.rs"));

    let removal = Deleter::new().remove(&plan_for(&base, std::slice::from_ref(&target)));

    assert!(removal.failures.is_empty(), "{:?}", removal.failures);
    assert!(
        checkout.join("uncommitted.rs").exists(),
        "uncommitted work was deleted"
    );
    assert_eq!(
        removal
            .kept
            .iter()
            .map(|refused| (refused.path.clone(), refused.reason.clone()))
            .collect::<Vec<_>>(),
        [(checkout, Refusal::HoldsCheckout)]
    );
    // Everything the checkout did not cover still goes, and the target itself stays because
    // it is no longer empty.
    assert!(!target.join("junk").exists());
    assert!(target.exists());
    assert!(removal.removed.iter().all(|removed| !removed.complete));
}

#[test]
fn a_git_file_marks_a_checkout_just_as_a_git_directory_does() {
    let (_tmp, base) = fixture();
    let target = base.join("ignored");
    let worktree = target.join("linked");
    // A linked work tree and a submodule both carry `.git` as a FILE holding a gitdir
    // pointer. Testing only for a directory would sweep both up.
    write(&worktree.join(".git"), 32);
    touch(&worktree.join("uncommitted.rs"));

    let removal = Deleter::new().remove(&plan_for(&base, &[target]));

    assert!(worktree.join("uncommitted.rs").exists());
    assert_eq!(removal.kept.len(), 1, "{:?}", removal.kept);
    assert_eq!(removal.kept[0].reason, Refusal::HoldsCheckout);
}

#[test]
fn a_target_that_is_itself_a_checkout_is_left_whole() {
    let (_tmp, base) = fixture();
    let target = base.join("vendored");
    touch(&target.join(".git/HEAD"));
    touch(&target.join("uncommitted.rs"));

    let removal = Deleter::new().remove(&plan_for(&base, std::slice::from_ref(&target)));

    assert!(target.join("uncommitted.rs").exists());
    assert!(target.join(".git/HEAD").exists());
    assert_eq!(removal.kept.len(), 1, "{:?}", removal.kept);
    assert_eq!(removal.kept[0].reason, Refusal::HoldsCheckout);
    assert!(removal.removed.is_empty(), "{:?}", removal.removed);
}

// ---------------------------------------------------------------------------------------
// 7. `--older-than` excludes directories that were touched recently.
// ---------------------------------------------------------------------------------------

#[test]
fn a_recently_touched_target_is_refused_when_an_age_floor_is_set() {
    let (_tmp, base) = fixture();
    let fresh = base.join("fresh/node_modules");
    let stale = base.join("stale/node_modules");
    touch(&fresh.join("index.js"));
    touch(&stale.join("index.js"));
    let a_year_ago = SystemTime::now() - Duration::from_secs(365 * 24 * 60 * 60);
    filetime::set_file_mtime(&stale, filetime::FileTime::from_system_time(a_year_ago)).unwrap();

    let plan = Planner::new(&base)
        .older_than(Some(Duration::from_secs(30 * 24 * 60 * 60)))
        .plan([Target::at(&fresh), Target::at(&stale)]);

    assert_eq!(targets(&plan), [stale]);
    assert!(
        matches!(
            refusals(&plan).as_slice(),
            [(path, Refusal::RecentlyUsed { .. })] if path == &fresh
        ),
        "{:?}",
        refusals(&plan)
    );
}

#[test]
fn nothing_is_excluded_for_its_age_by_default() {
    let (_tmp, base) = fixture();
    let fresh = base.join("fresh/node_modules");
    touch(&fresh.join("index.js"));

    let plan = plan_for(&base, std::slice::from_ref(&fresh));

    assert_eq!(targets(&plan), [fresh]);
    assert!(plan.kept().is_empty());
}

// ---------------------------------------------------------------------------------------
// 8. A failure costs one target, never the batch.
// ---------------------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn one_unremovable_target_does_not_cost_the_others() {
    let (_tmp, base) = fixture();
    let sealed = base.join("a/node_modules");
    touch(&sealed.join("dep/index.js"));
    let fine: Vec<PathBuf> = (0..8)
        .map(|n| base.join(format!("b{n}/node_modules")))
        .collect();
    for target in &fine {
        touch(&target.join("dep/index.js"));
    }
    if !seal(&sealed.join("dep")) {
        return; // running as root, where permissions prove nothing
    }

    let mut all = vec![sealed.clone()];
    all.extend(fine.iter().cloned());
    let removal = Deleter::new().remove(&plan_for(&base, &all));
    unseal(&sealed.join("dep"));

    assert_eq!(removal.failures.len(), 1, "{:?}", removal.failures);
    assert_eq!(removal.failures[0].path, sealed.join("dep"));
    for target in &fine {
        assert!(!target.exists(), "{} survived", target.display());
    }
    assert!(sealed.exists(), "the sealed target was removed anyway");
}

// ---------------------------------------------------------------------------------------
// The happy path, and the accounting it reports.
// ---------------------------------------------------------------------------------------

#[test]
fn a_plain_target_is_removed_whole_and_its_bytes_are_reported() {
    let (_tmp, base) = fixture();
    let target = base.join("app/node_modules");
    write(&target.join("a/one.bin"), 64 * 1024);
    write(&target.join("a/b/two.bin"), 64 * 1024);

    let removal = Deleter::new().remove(&plan_for(&base, std::slice::from_ref(&target)));

    assert!(removal.failures.is_empty(), "{:?}", removal.failures);
    assert!(removal.kept.is_empty(), "{:?}", removal.kept);
    assert!(!target.exists());
    assert!(base.join("app").exists(), "the parent went too");
    assert_eq!(removal.removed.len(), 1);
    assert!(removal.removed[0].complete);
    assert!(removal.bytes_freed() >= 128 * 1024, "{removal:?}");
    // two files, two directories below the target, and the target itself
    assert_eq!(removal.entries_removed(), 5);
}

#[test]
fn several_targets_are_removed_in_one_batch() {
    let (_tmp, base) = fixture();
    let targets: Vec<PathBuf> = (0..32)
        .map(|n| base.join(format!("p{n}/node_modules")))
        .collect();
    for target in &targets {
        write(&target.join("dep/index.js"), 1024);
    }

    let removal = Deleter::new().remove(&plan_for(&base, &targets));

    assert!(removal.failures.is_empty(), "{:?}", removal.failures);
    assert_eq!(removal.removed.len(), 32);
    for target in &targets {
        assert!(!target.exists(), "{} survived", target.display());
    }
}
