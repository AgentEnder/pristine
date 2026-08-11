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
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use pristine::{Deleter, Freeing, Plan, Planner, Refusal, Removed, Step, Target};
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

/// Every file under `dir`, so a test can assert that a whole tree is still there rather than
/// naming its members one at a time.
fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current).unwrap() {
            let path = entry.unwrap().path();
            if path.symlink_metadata().unwrap().is_dir() {
                stack.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found.sort();
    found
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

/// The window between validating a path and acting on it, at its widest: planning and removal
/// are two calls a caller makes in order, and between them sit a printed plan and a person
/// answering a prompt.
///
/// A check is only worth what it is worth at the moment of the `unlink`. Everything the plan
/// proved was proved about a path, and a path is a name that something else can re-point, so
/// the removal cannot start from the name — it descends from a descriptor on the scan root.
///
/// This is the cheapest of the three statements of that property, and the only one that needs
/// no second thread. The two tests after it close the concurrent case.
#[cfg(unix)]
#[test]
fn an_ancestor_swapped_for_a_link_after_planning_cannot_take_the_removal_out_of_the_root() {
    let (_tmp, base) = fixture();
    let root = base.join("root");
    let target = root.join("app/node_modules");
    touch(&target.join("left-pad/index.js"));
    // Deliberately laid out so the swapped-in tree answers to the same relative path: this is
    // what makes the attack work at all, and a fixture that skipped it would pass against a
    // vulnerable deleter.
    let outside = base.join("precious");
    touch(&outside.join("node_modules/thesis.md"));

    // Validated while `root/app` really is a directory holding the target.
    let plan = plan_for(&root, std::slice::from_ref(&target));
    assert_eq!(targets(&plan), [target]);

    // ...and then it is not.
    fs::remove_dir_all(root.join("app")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("app")).unwrap();

    let removal = Deleter::new().remove(&plan);

    assert!(
        outside.join("node_modules/thesis.md").exists(),
        "the removal followed a swapped ancestor out of the root and deleted {}",
        outside.display()
    );
    assert!(removal.removed.is_empty(), "{:?}", removal.removed);
    assert!(!removal.failures.is_empty(), "the swap was not reported");
}

/// The scan ROOT swapped, which is the one name a descriptor-relative sweep still has to
/// resolve — every other descriptor descends from this one, so getting it wrong misdirects the
/// entire batch rather than one target.
///
/// The mirror sits on the same filesystem and is laid out name for name, so neither the
/// boundary check nor an `ENOENT` can be what saves this. Two separate guards have to hold:
/// the root's final component is opened with `O_NOFOLLOW`, and the descriptor's identity is
/// checked against the one the planner recorded.
#[cfg(unix)]
#[test]
fn the_scan_root_swapped_for_a_link_after_planning_misdirects_nothing() {
    let (_tmp, base) = fixture();
    let root = base.join("root");
    let target = root.join("app/node_modules");
    touch(&target.join("left-pad/index.js"));
    let mirror = base.join("mirror");
    touch(&mirror.join("app/node_modules/left-pad/index.js"));
    let intact = walk_files(&mirror);

    let plan = plan_for(&root, std::slice::from_ref(&target));
    assert_eq!(targets(&plan), std::slice::from_ref(&target));

    // The root itself moves this time, not something beneath it.
    fs::rename(&root, base.join("parked")).unwrap();
    std::os::unix::fs::symlink(&mirror, &root).unwrap();

    let removal = Deleter::new().remove(&plan);

    assert_eq!(
        walk_files(&mirror),
        intact,
        "the batch was anchored to a swapped root and deleted from the mirror"
    );
    assert!(removal.removed.is_empty(), "{:?}", removal.removed);
    assert!(
        !removal.failures.is_empty(),
        "the swapped root was not reported"
    );
}

/// The same swap done with a real directory rather than a symlink, which is what makes the
/// identity check load-bearing rather than belt-and-braces.
///
/// There is no link here to refuse, the replacement is a perfectly ordinary directory on the
/// same device, and its layout matches. Nothing about the *name* distinguishes it from the
/// directory the planner validated — only the inode does.
#[cfg(unix)]
#[test]
fn the_scan_root_replaced_by_a_real_directory_after_planning_misdirects_nothing() {
    let (_tmp, base) = fixture();
    let root = base.join("root");
    let target = root.join("app/node_modules");
    touch(&target.join("left-pad/index.js"));
    let mirror = base.join("mirror");
    touch(&mirror.join("app/node_modules/left-pad/index.js"));

    let plan = plan_for(&root, std::slice::from_ref(&target));
    assert_eq!(targets(&plan), std::slice::from_ref(&target));

    fs::rename(&root, base.join("parked")).unwrap();
    fs::rename(&mirror, &root).unwrap();

    let removal = Deleter::new().remove(&plan);

    // The mirror now answers to the root's name, so it is checked where it now lives.
    assert!(
        root.join("app/node_modules/left-pad/index.js").exists(),
        "the batch was anchored to a replaced root and deleted from it"
    );
    assert!(removal.removed.is_empty(), "{:?}", removal.removed);
    assert!(
        !removal.failures.is_empty(),
        "the replaced root was not reported"
    );
}

/// The escape attempted at the one instant it is guaranteed to matter: after the removal has
/// demonstrably begun descending into the target, and held there for the rest of the run.
///
/// Racing blind and hoping to land in the window makes a test that only sometimes notices the
/// bug, which is no guard at all. So the attacker synchronises on state anyone can observe —
/// it waits until entries start disappearing from the target, which proves the sweep is inside
/// its removal loop, and only then swaps the ancestor. A victim that resolves each child from
/// its path deletes the mirror image outside the root from that moment on.
///
/// The deleter is immune for a structural reason rather than a lucky one: by the time the
/// first entry disappears it already holds descriptors for the root, for `app` and for the
/// target, and a rename changes no descriptor. So the swap is not merely survived — the
/// removal goes on to finish correctly, on the real tree, which is what the last assertions
/// pin down.
#[cfg(unix)]
#[test]
fn an_ancestor_swapped_while_the_sweep_is_inside_the_target_cannot_redirect_it() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    const FILES: usize = 400;
    const ROUNDS: usize = 6;

    for round in 0..ROUNDS {
        let (_tmp, base) = fixture();
        let root = base.join("root");
        let outside = base.join("precious");
        let target = root.join("app/nm");

        for file in 0..FILES {
            let name = format!("f{file:04}.js");
            write(&target.join(&name), 256);
            // Name for name, so every unlink the victim issues after the swap lands on a
            // real file out here rather than harmlessly on `ENOENT`.
            write(&outside.join("nm").join(&name), 256);
        }
        let bait = walk_files(&outside);
        assert_eq!(bait.len(), FILES);

        let plan = plan_for(&root, std::slice::from_ref(&target));
        assert_eq!(targets(&plan), std::slice::from_ref(&target));

        let decoy = root.join("decoy");
        let parked = root.join("parked");
        std::os::unix::fs::symlink(&outside, &decoy).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let swapped = Arc::new(AtomicBool::new(false));
        let attacker = {
            let stop = Arc::clone(&stop);
            let swapped = Arc::clone(&swapped);
            let app = root.join("app");
            let target = target.clone();
            std::thread::spawn(move || {
                // Entries vanishing is the observable proof that the sweep has opened the
                // target and is working through it.
                while !stop.load(Ordering::Relaxed) {
                    let remaining = fs::read_dir(&target).map_or(0, Iterator::count);
                    if remaining < FILES {
                        break;
                    }
                    std::thread::yield_now();
                }
                if fs::rename(&app, &parked).is_ok() && fs::rename(&decoy, &app).is_ok() {
                    swapped.store(true, Ordering::Relaxed);
                }
                // Held for the rest of the removal, so there is no window to be lucky in.
                while !stop.load(Ordering::Relaxed) {
                    std::thread::yield_now();
                }
                let _ = fs::rename(&app, &decoy);
                let _ = fs::rename(&parked, &app);
            })
        };

        let removal = Deleter::new().remove(&plan);
        stop.store(true, Ordering::Relaxed);
        attacker.join().expect("the attacker thread must not panic");
        assert!(
            swapped.load(Ordering::Relaxed),
            "round {round}: the ancestor was never swapped, so nothing was exercised"
        );

        assert_eq!(
            walk_files(&outside),
            bait,
            "round {round}: the removal was redirected out of the scan root"
        );
        // Not merely survived: a rename changes no descriptor, so the sweep finished the job
        // it had already opened.
        assert!(removal.is_clean(), "round {round}: {:?}", removal.failures);
        assert!(
            !target.exists(),
            "round {round}: the target was left behind"
        );
    }
}

/// The same escape attempted by hammering rather than by synchronising, which explores
/// interleavings the test above pins down one of.
///
/// A second attacker thread renames an ancestor away and drops a symlink to an outside tree in
/// its place, over and over, for as long as the removal runs. Any implementation that resolves
/// a target from its name — however recently it re-validated that name — eventually issues one
/// syscall on the far side of a swap and deletes something it was never offered.
///
/// The deleter survives this because it does not resolve names: it opens the scan root once
/// and reaches every entry by `openat` from an already-open parent, with `O_NOFOLLOW`, then
/// removes by `unlinkat` against that same descriptor. A swap can therefore make the removal
/// *fail* — an `ELOOP`, an `ENOENT`, a target left standing — and those are all fine and all
/// reported. What it cannot do is redirect one.
///
/// The bait outside the root deliberately mirrors the names inside it, so a descent that did
/// follow the swap would find real files to unlink rather than harmlessly hitting `ENOENT`.
#[cfg(unix)]
#[test]
fn hammering_an_ancestor_throughout_a_removal_never_reaches_outside_the_root() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    const TARGETS: usize = 16;
    const ROUNDS: usize = 12;

    let mut total_swaps = 0_u64;
    for round in 0..ROUNDS {
        let (_tmp, base) = fixture();
        let root = base.join("root");
        let outside = base.join("precious");

        let mut targets = Vec::new();
        for i in 0..TARGETS {
            let target = root.join(format!("app/nm{i}"));
            for pkg in 0..8 {
                write(&target.join(format!("pkg{pkg}/index.js")), 512);
                write(&target.join(format!("pkg{pkg}/readme.md")), 512);
                // The bait mirrors the real tree name for name, which is what an attacker
                // would actually arrange: the victim collects child names from the real
                // directory and then removes them by path, so a swap only costs anything if
                // those same names resolve to something on the far side.
                write(&outside.join(format!("nm{i}/pkg{pkg}/index.js")), 512);
                write(&outside.join(format!("nm{i}/pkg{pkg}/readme.md")), 512);
            }
            targets.push(target);
        }
        let bait: Vec<PathBuf> = walk_files(&outside);
        assert!(!bait.is_empty());

        // Built while the ancestry is honest, exactly as a real run would be.
        let plan = plan_for(&root, &targets);
        assert_eq!(plan.targets().len(), TARGETS, "{:?}", refusals(&plan));

        let parked = root.join("parked");
        let decoy = root.join("decoy");
        std::os::unix::fs::symlink(&outside, &decoy).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let attacker = {
            let stop = Arc::clone(&stop);
            let app = root.join("app");
            std::thread::spawn(move || {
                let mut swaps = 0_u64;
                while !stop.load(Ordering::Relaxed) {
                    // Two renames rather than one: a symlink cannot be renamed over a
                    // non-empty directory, so the real exploit is move-the-real-one-away then
                    // move-the-link-in. That is what makes the window narrow but genuine.
                    if fs::rename(&app, &parked).is_ok() {
                        if fs::rename(&decoy, &app).is_ok() {
                            swaps += 1;
                            // Held, rather than reversed immediately. A victim that resolves
                            // by name is caught between its check and its `unlink`, and that
                            // gap is microseconds — so the link has to be *in place* for a
                            // meaningful share of the wall clock, not merely flickered.
                            std::thread::sleep(Duration::from_micros(200));
                            let _ = fs::rename(&app, &decoy);
                        }
                        let _ = fs::rename(&parked, &app);
                    }
                    std::thread::yield_now();
                }
                swaps
            })
        };

        let removal = Deleter::new().remove(&plan);
        stop.store(true, Ordering::Relaxed);
        total_swaps += attacker.join().expect("the attacker thread must not panic");

        for file in &bait {
            assert!(
                file.exists(),
                "round {round}: a swapped ancestor took the removal out of the root and \
                 deleted {}",
                file.display()
            );
        }
        // Nothing outside the root may be reported either, since nothing outside was touched.
        for removed in &removal.removed {
            assert!(
                removed.path.starts_with(&root),
                "round {round}: removed {} from outside the root",
                removed.path.display()
            );
        }
    }

    // Without this the test could pass by never racing at all, which would make it decoration.
    assert!(
        total_swaps > 0,
        "the attacker never completed a swap in {ROUNDS} rounds, so nothing was exercised"
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

#[test]
fn a_watcher_is_told_about_each_target_as_it_finishes_and_is_told_the_same_thing_twice() {
    let (_tmp, base) = fixture();
    let targets: Vec<PathBuf> = (0..32)
        .map(|n| base.join(format!("p{n}/node_modules")))
        .collect();
    for target in &targets {
        write(&target.join("dep/index.js"), 1024);
    }

    let watched = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&watched);
    let removal = Deleter::new()
        .watching(move |step| {
            if let Step::Finished(removed) = step {
                sink.lock().unwrap().push(removed.clone());
            }
        })
        .remove(&plan_for(&base, &targets));

    // The live view drops a row on each of these, and the report printed afterwards lists
    // what was removed. A row dropped for a target the report then calls untouched — or a
    // target removed that no row ever heard about — is the two disagreeing, so they are one
    // condition and this is the test that says so.
    let mut watched: Vec<_> = watched.lock().unwrap().iter().map(summarise).collect();
    let mut reported: Vec<_> = removal.removed.iter().map(summarise).collect();
    watched.sort();
    reported.sort();
    assert_eq!(watched.len(), 32);
    assert_eq!(watched, reported);
}

#[test]
fn a_watcher_is_told_how_far_a_target_has_got_while_it_is_still_going() {
    // The event a row's number falls on. Without it the front end knows only that a directory
    // has already gone, and anything it draws between the keystroke and that moment is an
    // animation over a fact rather than a report of one.
    let (_tmp, base) = fixture();
    let target = base.join("app/node_modules");
    for pkg in 0..40 {
        for file in 0..50 {
            write(&target.join(format!("p{pkg}/f{file}.js")), 1024);
        }
    }

    let steps = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&steps);
    let removal = Deleter::new()
        .threads(1)
        .watching(move |step| sink.lock().unwrap().push(step.clone()))
        .remove(&plan_for(&base, std::slice::from_ref(&target)));

    let steps = steps.lock().unwrap();
    let progress: Vec<&Freeing> = steps
        .iter()
        .filter_map(|step| match step {
            Step::Freeing(freeing) => Some(freeing),
            Step::Finished(_) | Step::Swept(_) => None,
        })
        .collect();

    // 2,000 files at 64 entries apiece, so a target worth watching reports many times over —
    // enough for a 30fps view to draw a number that actually moves.
    assert!(progress.len() > 10, "{} reports", progress.len());
    assert!(progress.iter().all(|freeing| freeing.path == target));

    // Cumulative and monotonic, which is what makes a consumer that keeps the latest per
    // target exact rather than approximate: it can never double-count and never go backwards.
    for pair in progress.windows(2) {
        assert!(pair[1].bytes >= pair[0].bytes, "{:?}", (pair[0], pair[1]));
        assert!(
            pair[1].entries > pair[0].entries,
            "{:?}",
            (pair[0], pair[1])
        );
    }

    // The last word is the final report's own figure, reached rather than restated: the
    // progress and the `Removal` are the same running total read at different moments, so a
    // counter climbing on one and then handed the other does not jump.
    let last = progress.last().expect("progress was reported");
    assert!(last.bytes <= removal.bytes_freed());
    assert_eq!(removal.removed.len(), 1);
    assert_eq!(removal.removed[0].bytes, removal.bytes_freed());

    // …and it is genuinely progress rather than one report at the end.
    assert!(
        last.entries < removal.entries_removed(),
        "the last progress report was the whole job"
    );
    let finished = steps
        .iter()
        .filter(|step| matches!(step, Step::Finished(_)))
        .count();
    assert_eq!(finished, 1);
    // The pool moving off the target is the last word on it, and it comes after the report of
    // what was removed — a consumer that drops a row on `Finished` and advances a position on
    // `Swept` must never see the position move first.
    assert!(
        matches!(steps.last(), Some(Step::Swept(path)) if path == &target),
        "the sweep reported progress after it had finished"
    );
    let order: Vec<&str> = steps
        .iter()
        .rev()
        .take(2)
        .map(|step| match step {
            Step::Freeing(_) => "freeing",
            Step::Finished(_) => "finished",
            Step::Swept(_) => "swept",
        })
        .collect();
    assert_eq!(order, ["swept", "finished"]);
}

#[test]
fn a_watcher_is_told_the_pool_moved_on_even_from_a_target_nothing_happened_to() {
    // The batch's position and the batch's outcome are different facts, and this is the case
    // that separates them: a target the deleter worked through and could not touch at all.
    // Counted as a removal it is nothing — `Removal::removed` must not claim it and no row may
    // disappear for it — but the deleter has demonstrably moved past it, so a progress
    // indicator that ignored it would sit below where the deleter is for the rest of the run.
    let (_tmp, base) = fixture();
    let doomed = base.join("vanishes/node_modules");
    let survives = base.join("app/node_modules");
    write(&doomed.join("dep/index.js"), 1024);
    write(&survives.join("dep/index.js"), 1024);

    let plan = plan_for(&base, &[doomed.clone(), survives.clone()]);
    // Planned, then the ground moves: the directory holding the target is gone by the time the
    // sweep tries to open its way down to it, so it fails before unlinking a single entry.
    fs::remove_dir_all(base.join("vanishes")).unwrap();

    let steps = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&steps);
    let removal = Deleter::new()
        .watching(move |step| sink.lock().unwrap().push(step.clone()))
        .remove(&plan);

    let steps = steps.lock().unwrap();
    let swept: Vec<&PathBuf> = steps
        .iter()
        .filter_map(|step| match step {
            Step::Swept(path) => Some(path),
            Step::Freeing(_) | Step::Finished(_) => None,
        })
        .collect();
    let finished: Vec<&Removed> = steps
        .iter()
        .filter_map(|step| match step {
            Step::Finished(removed) => Some(removed),
            Step::Freeing(_) | Step::Swept(_) => None,
        })
        .collect();

    // Every target in the plan, whatever became of it — which is what makes the count reach
    // its total rather than stopping one short for the rest of the run.
    assert_eq!(swept.len(), 2, "{swept:?}");
    assert!(
        swept.contains(&&doomed) && swept.contains(&&survives),
        "{swept:?}"
    );

    // And the existing rule is untouched: only the target something happened to is reported,
    // and it is the same one the final report lists.
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].path, survives);
    assert_eq!(removal.removed.len(), 1);
    assert_eq!(removal.removed[0].path, survives);
    assert_eq!(removal.failures.len(), 1, "{:?}", removal.failures);
}

#[test]
fn a_watcher_is_told_when_a_target_was_only_partly_removed() {
    let (_tmp, base) = fixture();
    let target = base.join("checkout/node_modules");
    write(&target.join("dep/index.js"), 1024);
    mkdir(&target.join("inner/.git"));

    let watched = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&watched);
    let removal = Deleter::new()
        .watching(move |step| {
            if let Step::Finished(removed) = step {
                sink.lock().unwrap().push(removed.clone());
            }
        })
        .remove(&plan_for(&base, std::slice::from_ref(&target)));

    // The sweep refused the checkout inside, so the target itself is still standing — and a
    // front end that dropped its row on the strength of "the deleter got to it" would tell a
    // reader their work tree is gone while it is still on disk.
    assert!(target.exists());
    let watched = watched.lock().unwrap();
    assert_eq!(watched.len(), 1);
    assert!(!watched[0].complete, "{:?}", watched[0]);
    assert_eq!(removal.removed.len(), 1);
}

/// A `Removed` reduced to the fields two readers of it have to agree on.
fn summarise(removed: &Removed) -> (PathBuf, u64, u64, bool) {
    (
        removed.path.clone(),
        removed.bytes,
        removed.entries,
        removed.complete,
    )
}
