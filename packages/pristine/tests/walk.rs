//! What the walker promises, stated as fixtures on a real filesystem.
//!
//! The tests that matter most here are the negative ones. `target`, `vendor` and `build` are
//! each claimed by several ecosystems and are each, in some project, hand-written source. A
//! walker that matched on the name alone would pass every positive test in this file and
//! still be unshippable.

// `allow-unwrap-in-tests` in clippy.toml only reaches code inside a `#[test]` function, and
// the fixture helpers below sit outside one. An unwrap in a fixture is an assertion.
#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use pristine::{Hit, Ruleset, SizeMode, Walker};
use tempfile::TempDir;

/// Creates `path` and every parent, then writes `bytes` bytes of filler into it.
fn write(path: &Path, bytes: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![b'x'; bytes]).unwrap();
}

/// Creates an empty file, and every directory above it.
fn touch(path: &Path) {
    write(path, 0);
}

fn mkdir(path: &Path) {
    fs::create_dir_all(path).unwrap();
}

fn ruleset() -> Arc<Ruleset> {
    Arc::new(Ruleset::builtin().unwrap())
}

/// Runs a default walk and returns the hits in a deterministic order.
fn scan(root: &Path) -> Vec<Hit> {
    let hits = Mutex::new(Vec::new());
    let outcome = Walker::new(root, ruleset()).run(|hit| hits.lock().unwrap().push(hit));
    assert!(
        outcome.errors.is_empty(),
        "unexpected: {:?}",
        outcome.errors
    );
    let mut hits = hits.into_inner().unwrap();
    hits.sort_by(|a, b| a.path.cmp(&b.path));
    hits
}

/// The rule ids of every hit, in path order.
fn rule_ids(root: &Path) -> Vec<String> {
    scan(root).iter().map(|hit| hit.rule.id.clone()).collect()
}

#[test]
fn node_modules_beside_a_package_json_is_reclaimable() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("app/package.json"));
    touch(&tmp.path().join("app/node_modules/left-pad/index.js"));

    let hits = scan(tmp.path());

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, tmp.path().join("app/node_modules"));
    assert_eq!(hits[0].project_root, tmp.path().join("app"));
    assert_eq!(hits[0].rule.id, "node");
}

#[test]
fn node_modules_without_a_package_json_is_left_alone() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("notes/node_modules/thesis.md"));

    assert!(scan(tmp.path()).is_empty());
}

#[test]
fn a_matched_directory_is_never_descended_into() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("app/package.json"));
    // A real node_modules is full of directories that would each match on their own. Finding
    // more than one hit here means the walk enumerated the tree it had already claimed,
    // which is the exact cost this design exists to avoid.
    touch(&tmp.path().join("app/node_modules/dep/package.json"));
    touch(
        &tmp.path()
            .join("app/node_modules/dep/node_modules/nested/package.json"),
    );
    touch(
        &tmp.path()
            .join("app/node_modules/dep/node_modules/nested/node_modules/x.js"),
    );

    let hits = scan(tmp.path());

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, tmp.path().join("app/node_modules"));
}

#[test]
fn target_is_rust_or_maven_according_to_the_marker() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("crate/Cargo.toml"));
    touch(&tmp.path().join("crate/target/debug/bin"));
    touch(&tmp.path().join("service/pom.xml"));
    touch(&tmp.path().join("service/target/classes/Main.class"));

    let hits = scan(tmp.path());

    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].rule.id, "cargo");
    assert_eq!(hits[0].rule.regenerate, "cargo build");
    assert_eq!(hits[1].rule.id, "maven");
    assert_eq!(hits[1].rule.regenerate, "mvn package");
}

#[test]
fn a_target_directory_with_no_marker_above_it_is_left_alone() {
    let tmp = TempDir::new().unwrap();
    // Somebody's shooting-range logbook. No manifest, so no claim.
    touch(&tmp.path().join("archery/target/2026-scores.csv"));

    assert!(scan(tmp.path()).is_empty());
}

#[test]
fn vendor_is_go_or_composer_according_to_the_marker() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("svc/go.mod"));
    touch(&tmp.path().join("svc/vendor/modules.txt"));
    touch(&tmp.path().join("site/composer.json"));
    touch(&tmp.path().join("site/vendor/autoload.php"));

    assert_eq!(rule_ids(tmp.path()), ["composer", "go"]);
}

#[test]
fn a_cmake_build_directory_is_claimed_only_when_cmake_generated_it() {
    let tmp = TempDir::new().unwrap();
    // The dangerous shape: a CMake project whose `build/` holds hand-written source.
    touch(&tmp.path().join("engine/CMakeLists.txt"));
    touch(&tmp.path().join("engine/build/toolchain.cmake"));
    // The safe shape: CMake writes CMakeCache.txt into the binary directory it owns.
    touch(&tmp.path().join("engine/cmake-build-debug/CMakeCache.txt"));

    let hits = scan(tmp.path());

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, tmp.path().join("engine/cmake-build-debug"));
    assert_eq!(hits[0].rule.id, "cmake");
    // Self-anchored, so the project root is the matched directory itself.
    assert_eq!(hits[0].project_root, hits[0].path);
}

#[test]
fn build_is_claimed_when_gradle_owns_it() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("svc/build.gradle.kts"));
    touch(&tmp.path().join("svc/build/libs/svc.jar"));
    touch(&tmp.path().join("svc/.gradle/8.5/checksums.bin"));

    assert_eq!(rule_ids(tmp.path()), ["gradle", "gradle"]);
}

#[test]
fn a_multi_segment_target_matches_its_whole_path() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("api/Gemfile"));
    touch(
        &tmp.path()
            .join("api/vendor/bundle/ruby/3.3.0/gems/rake/rake.rb"),
    );
    // `vendor` itself is not claimed: no go.mod and no composer.json sit beside it.
    let hits = scan(tmp.path());

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, tmp.path().join("api/vendor/bundle"));
    assert_eq!(hits[0].rule.id, "bundler");
    assert_eq!(hits[0].project_root, tmp.path().join("api"));
}

#[test]
fn an_ancestor_anchored_rule_finds_caches_scattered_through_a_project() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("lib/pyproject.toml"));
    // Nothing but Python source sits beside these, so a parent-anchored rule would miss them.
    touch(
        &tmp.path()
            .join("lib/src/pkg/sub/__pycache__/mod.cpython-312.pyc"),
    );
    touch(&tmp.path().join("lib/src/pkg/sub/mod.py"));

    let hits = scan(tmp.path());

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].rule.id, "python-caches");
    assert_eq!(hits[0].project_root, tmp.path().join("lib"));
}

#[test]
fn a_pycache_outside_any_python_project_is_left_alone() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("scratch/__pycache__/whatever.pyc"));

    assert!(scan(tmp.path()).is_empty());
}

#[test]
fn a_glob_marker_identifies_a_dotnet_project() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("Svc/Svc.csproj"));
    touch(&tmp.path().join("Svc/bin/Debug/Svc.dll"));
    touch(&tmp.path().join("Svc/obj/project.assets.json"));
    // No project file here, so `bin` is just a directory called bin.
    touch(&tmp.path().join("scripts/bin/deploy.sh"));

    let hits = scan(tmp.path());

    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|hit| hit.rule.id == "dotnet"));
    assert!(
        hits.iter()
            .all(|hit| hit.path.starts_with(tmp.path().join("Svc")))
    );
}

#[test]
fn markers_required_all_needs_every_marker() {
    let tmp = TempDir::new().unwrap();
    // Unity wants ProjectSettings AND Assets. A stray `Library` beside only one of them is
    // not a Unity project.
    mkdir(&tmp.path().join("half/Assets"));
    mkdir(&tmp.path().join("half/Library"));
    mkdir(&tmp.path().join("whole/Assets"));
    mkdir(&tmp.path().join("whole/ProjectSettings"));
    mkdir(&tmp.path().join("whole/Library"));

    let hits = scan(tmp.path());

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, tmp.path().join("whole/Library"));
    assert_eq!(hits[0].rule.ecosystem, "Unity");
    assert!(
        hits[0].rule.note.is_some(),
        "the slow-reimport warning should survive"
    );
}

#[test]
fn the_git_directory_is_never_walked() {
    let tmp = TempDir::new().unwrap();
    mkdir(&tmp.path().join("repo/.git"));
    // Git's own object store is not reclaimable, and a fixture inside it must not be found.
    touch(&tmp.path().join("repo/.git/modules/sub/package.json"));
    touch(
        &tmp.path()
            .join("repo/.git/modules/sub/node_modules/dep/index.js"),
    );

    assert!(scan(tmp.path()).is_empty());
}

#[test]
fn every_hit_carries_its_regeneration_command_and_its_age() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("app/package.json"));
    touch(&tmp.path().join("app/node_modules/dep/index.js"));

    let hits = scan(tmp.path());
    let hit = &hits[0];

    assert_eq!(
        hit.rule.regenerate,
        "npm ci / pnpm install / yarn — whichever lockfile is present"
    );
    let modified = hit
        .modified
        .expect("a directory we just created has an mtime");
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    assert!(age < Duration::from_secs(120), "implausible age: {age:?}");
}

#[test]
fn a_recursive_measurement_covers_the_whole_claimed_subtree() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("app/package.json"));
    write(&tmp.path().join("app/node_modules/a/big.bin"), 512 * 1024);
    write(&tmp.path().join("app/node_modules/b/big.bin"), 512 * 1024);
    // Outside the claim, so it must not be counted.
    write(&tmp.path().join("app/src/big.bin"), 4 * 1024 * 1024);

    let hits = scan(tmp.path());

    assert_eq!(hits.len(), 1);
    let size = hits[0].size;
    assert!(
        size >= 1024 * 1024,
        "expected at least the 1 MiB written, got {size}"
    );
    assert!(
        size < 3 * 1024 * 1024,
        "the sibling source tree leaked in: {size}"
    );
}

#[test]
fn directory_only_measurement_does_not_traverse_the_claim() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("app/package.json"));
    write(
        &tmp.path().join("app/node_modules/a/big.bin"),
        4 * 1024 * 1024,
    );

    let hits = Mutex::new(Vec::new());
    Walker::new(tmp.path(), ruleset())
        .size_mode(SizeMode::DirectoryOnly)
        .run(|hit| hits.lock().unwrap().push(hit));
    let hits = hits.into_inner().unwrap();

    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].size < 4 * 1024 * 1024,
        "DirectoryOnly reported {} bytes, so it walked the subtree",
        hits[0].size
    );
}

#[test]
fn the_tree_rolls_reclaimable_bytes_up_into_every_ancestor() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("repos/a/package.json"));
    write(&tmp.path().join("repos/a/node_modules/big.bin"), 256 * 1024);
    touch(&tmp.path().join("repos/b/Cargo.toml"));
    write(&tmp.path().join("repos/b/target/big.bin"), 256 * 1024);

    let (tree, outcome) = Walker::new(tmp.path(), ruleset()).run_to_tree();

    assert_eq!(outcome.hits, 2);
    assert_eq!(tree.reclaimable(), outcome.reclaimable_bytes);

    let repos = tree.find(&tmp.path().join("repos")).unwrap();
    let a = tree.find(&tmp.path().join("repos/a")).unwrap();
    let b = tree.find(&tmp.path().join("repos/b")).unwrap();
    assert_eq!(
        tree.node(repos).reclaimable,
        tree.node(a).reclaimable + tree.node(b).reclaimable
    );
    assert_eq!(
        tree.node(tree.root()).reclaimable,
        tree.node(repos).reclaimable
    );
    assert!(tree.node(a).reclaimable >= 256 * 1024);
}

#[test]
fn the_tree_holds_only_paths_that_lead_to_something_reclaimable() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("repos/a/package.json"));
    write(&tmp.path().join("repos/a/node_modules/big.bin"), 4096);
    // An ordinary source tree with nothing to reclaim anywhere beneath it.
    touch(&tmp.path().join("repos/docs/guide/intro.md"));

    let (tree, _) = Walker::new(tmp.path(), ruleset()).run_to_tree();

    assert!(tree.find(&tmp.path().join("repos/docs")).is_none());
    assert!(
        tree.find(&tmp.path().join("repos/a/node_modules"))
            .is_some()
    );
    // root, repos, a, node_modules — and nothing else.
    assert_eq!(tree.len(), 4);
}

#[test]
fn a_hit_node_is_a_leaf_and_carries_its_rule() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("a/package.json"));
    write(&tmp.path().join("a/node_modules/big.bin"), 4096);

    let (tree, _) = Walker::new(tmp.path(), ruleset()).run_to_tree();
    let id = tree.find(&tmp.path().join("a/node_modules")).unwrap();
    let node = tree.node(id);

    assert!(node.children.is_empty());
    assert_eq!(node.hit.as_ref().unwrap().rule.id, "node");
    assert_eq!(node.reclaimable, node.hit.as_ref().unwrap().size);
}

#[test]
fn children_sort_within_their_own_level() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("small/package.json"));
    write(&tmp.path().join("small/node_modules/f.bin"), 4096);
    touch(&tmp.path().join("large/package.json"));
    write(
        &tmp.path().join("large/node_modules/f.bin"),
        2 * 1024 * 1024,
    );

    let (mut tree, _) = Walker::new(tmp.path(), ruleset()).run_to_tree();
    tree.sort_by_reclaimable();

    let top: Vec<_> = tree
        .children(tree.root())
        .iter()
        .map(|&id| tree.node(id).name.to_string_lossy().into_owned())
        .collect();
    assert_eq!(top, ["large", "small"]);
}

#[test]
fn a_walk_is_consumed_while_it_runs_rather_than_after_it() {
    let tmp = TempDir::new().unwrap();
    for name in ["a", "b", "c"] {
        touch(&tmp.path().join(name).join("package.json"));
        touch(&tmp.path().join(name).join("node_modules/dep/index.js"));
    }

    // A rendezvous channel: every send blocks until this thread takes the hit, so the walk
    // only completes if the consumer is being fed during it.
    let (tx, rx) = sync_channel::<Hit>(0);
    let root = tmp.path().to_path_buf();
    let walk = std::thread::spawn(move || {
        Walker::new(&root, ruleset()).run(|hit| {
            let _ = tx.send(hit);
        })
    });

    let received: Vec<Hit> = rx.iter().collect();
    let outcome = walk.join().unwrap();

    assert_eq!(received.len(), 3);
    assert_eq!(outcome.hits, 3);
}

#[test]
fn max_depth_bounds_the_walk() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("deep/nest/app/package.json"));
    touch(&tmp.path().join("deep/nest/app/node_modules/dep/index.js"));

    let count = |depth: usize| {
        let hits = Mutex::new(0usize);
        Walker::new(tmp.path(), ruleset())
            .max_depth(Some(depth))
            .run(|_| *hits.lock().unwrap() += 1);
        hits.into_inner().unwrap()
    };

    assert_eq!(count(3), 0, "node_modules sits at depth 4");
    assert_eq!(count(4), 1);
}

#[test]
fn a_user_rule_extends_the_built_in_set() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("proj/Makefile.myecosystem"));
    touch(&tmp.path().join("proj/.myecosystem-out/artifact"));

    let user = r#"
        [[rules]]
        id = "myecosystem"
        ecosystem = "My Ecosystem"
        markers = ["Makefile.myecosystem"]
        targets = [".myecosystem-out"]
        regenerate = "make"
    "#;
    let rules = Arc::new(Ruleset::with_overrides(user).unwrap());

    let hits = Mutex::new(Vec::new());
    Walker::new(tmp.path(), rules).run(|hit| hits.lock().unwrap().push(hit));
    let hits = hits.into_inner().unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].rule.id, "myecosystem");
}
