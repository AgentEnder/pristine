//! What the walker promises, stated as fixtures on a real filesystem.
//!
//! The tests that matter most here are the negative ones. `target`, `vendor` and `build` are
//! each claimed by several ecosystems and are each, in some project, hand-written source. A
//! walker that matched on the name alone would pass every positive test in this file and
//! still be unshippable.

// `allow-unwrap-in-tests` in clippy.toml only reaches code inside a `#[test]` function, and
// the fixture helpers below sit outside one. An unwrap in a fixture is an assertion.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::sync::mpsc::{channel, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime};

use pristine::{Claim, Found, Hit, Order, Rule, Ruleset, Size, SizeMode, Sort, Walker};
use tempfile::TempDir;

/// Every fixture in this file is tier one, so a hit that carries no rule is a failure rather
/// than a case to handle. Tier two has its own file.
fn rule(hit: &Hit) -> &Rule {
    hit.rule().expect("a tier-one hit carries its rule")
}

fn project_root(hit: &Hit) -> &Path {
    match &hit.claim {
        Claim::Rule(claim) => &claim.project_root,
        Claim::Ignored(_) => panic!("expected a tier-one hit, got {:?}", hit.claim),
    }
}

fn label(hit: &Hit) -> String {
    hit.label().into_owned()
}

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

/// A walker that measures, for the tests that need real byte counts.
fn breakdown(root: &Path) -> Walker {
    Walker::new(root, ruleset()).size_mode(SizeMode::Breakdown)
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

#[cfg(not(unix))]
fn seal(_dir: &Path) -> bool {
    false
}

#[cfg(not(unix))]
fn unseal(_dir: &Path) {}

/// Runs a default walk and returns the hits in a deterministic order.
fn scan(root: &Path) -> Vec<Hit> {
    scan_with(&Walker::new(root, ruleset()))
}

/// Runs a walk that asks for sizes, which a default one does not.
fn scan_with_sizes(root: &Path) -> Vec<Hit> {
    scan_with(&breakdown(root))
}

/// Collects a walk's claims with their prices folded back in, which is what every consumer
/// that is not a live front end has to do: a claim is published unpriced and its size follows
/// as its own event, from the pricing pool.
fn scan_with(walker: &Walker) -> Vec<Hit> {
    let hits = Mutex::new(Vec::new());
    let sizes = Mutex::new(Vec::new());
    let outcome = walker.run(|found| match found {
        Found::Claim(hit) => hits.lock().unwrap().push(hit),
        Found::Priced(priced) => sizes.lock().unwrap().push(priced),
    });
    assert!(
        outcome.errors.is_empty(),
        "unexpected: {:?}",
        outcome.errors
    );
    let mut hits = hits.into_inner().unwrap();
    for priced in sizes.into_inner().unwrap() {
        let hit = hits
            .iter_mut()
            .find(|hit| hit.path == priced.path)
            .expect("a price arrived for a claim nobody reported");
        hit.size = priced.size;
    }
    hits.sort_by(|a, b| a.path.cmp(&b.path));
    hits
}

/// The rule ids of every hit, in path order.
fn rule_ids(root: &Path) -> Vec<String> {
    scan(root).iter().map(|hit| rule(hit).id.clone()).collect()
}

#[test]
fn node_modules_beside_a_package_json_is_reclaimable() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("app/package.json"));
    touch(&tmp.path().join("app/node_modules/left-pad/index.js"));

    let hits = scan(tmp.path());

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, tmp.path().join("app/node_modules"));
    assert_eq!(project_root(&hits[0]), tmp.path().join("app"));
    assert_eq!(rule(&hits[0]).id, "node");
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
    assert_eq!(rule(&hits[0]).id, "cargo");
    assert_eq!(label(&hits[0]), "Rust Build Artifacts");
    assert_eq!(rule(&hits[1]).id, "maven");
    assert_eq!(label(&hits[1]), "Maven Build Artifacts");
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
    assert_eq!(rule(&hits[0]).id, "cmake");
    // Self-anchored, so the project root is the matched directory itself.
    assert_eq!(project_root(&hits[0]), hits[0].path);
}

#[test]
fn build_is_claimed_when_gradle_owns_it() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("svc/build.gradle.kts"));
    touch(&tmp.path().join("svc/build/libs/svc.jar"));
    touch(&tmp.path().join("svc/.gradle/8.5/checksums.bin"));

    // One ecosystem, two kinds, so two rules: `.gradle` is a cache and `build` is output, and
    // a rule carrying both could only label one of them honestly.
    assert_eq!(rule_ids(tmp.path()), ["gradle-caches", "gradle"]);
    let hits = scan(tmp.path());
    assert_eq!(label(&hits[0]), "Gradle Cache");
    assert_eq!(label(&hits[1]), "Gradle Build Artifacts");
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
    assert_eq!(rule(&hits[0]).id, "bundler");
    assert_eq!(project_root(&hits[0]), tmp.path().join("api"));
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
    assert_eq!(rule(&hits[0]).id, "python-caches");
    assert_eq!(project_root(&hits[0]), tmp.path().join("lib"));
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
    assert!(hits.iter().all(|hit| rule(hit).id == "dotnet"));
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
    assert_eq!(rule(&hits[0]).ecosystem, "Unity");
    assert!(
        rule(&hits[0]).note.is_some(),
        "the slow-reimport warning should survive"
    );
}

#[test]
fn an_unreal_build_directory_is_left_alone_because_it_holds_authored_files() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("game/Game.uproject"));
    // Authored per-platform build configuration. A `.uproject` beside it proves the project's
    // type, not that this directory is output — the same mistake as matching a bare `build`.
    touch(&tmp.path().join("game/Build/IOS/game.entitlements"));
    touch(&tmp.path().join("game/Build/Android/AndroidManifest.xml"));
    // Logs, configs and autosaves, likewise not regenerable by rebuilding.
    touch(&tmp.path().join("game/Saved/Autosaves/level.umap"));
    // These really are output.
    touch(&tmp.path().join("game/Intermediate/Build/obj.o"));
    touch(&tmp.path().join("game/Binaries/Mac/game.dylib"));
    touch(&tmp.path().join("game/DerivedDataCache/blob"));

    let claimed: Vec<_> = scan(tmp.path())
        .iter()
        .map(|hit| hit.path.strip_prefix(tmp.path()).unwrap().to_owned())
        .collect();

    assert_eq!(
        claimed,
        [
            Path::new("game/Binaries"),
            Path::new("game/DerivedDataCache"),
            Path::new("game/Intermediate"),
        ]
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

    // Tier two off: this `.git` is a fixture rather than a repository, and tier two would
    // rightly report that git refuses to answer for it. That report is its own test.
    assert!(scan_with(&Walker::new(tmp.path(), ruleset()).fallback(false)).is_empty());
}

#[test]
fn every_hit_carries_its_label_and_its_age() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("app/package.json"));
    touch(&tmp.path().join("app/node_modules/dep/index.js"));

    let hits = scan(tmp.path());
    let hit = &hits[0];

    // The ecosystem and the kind, composed: what the directory IS, which is a fact about it
    // rather than a guess about the machine it would be rebuilt on.
    assert_eq!(label(hit), "Node Dependencies");
    let modified = hit
        .modified
        .expect("a directory we just created has an mtime");
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    assert!(age < Duration::from_secs(120), "implausible age: {age:?}");
}

#[test]
fn a_nested_package_is_claimed_for_the_package_that_owns_it() {
    let tmp = TempDir::new().unwrap();
    // The shape every pnpm/nx/turbo monorepo has: one manifest at the root and one per
    // package, each with its own `node_modules`.
    touch(&tmp.path().join("repo/package.json"));
    touch(&tmp.path().join("repo/packages/ui/package.json"));
    touch(
        &tmp.path()
            .join("repo/packages/ui/node_modules/dep/index.js"),
    );

    let hits = scan(tmp.path());

    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0].path,
        tmp.path().join("repo/packages/ui/node_modules")
    );
    // The nearer manifest is the one that justifies the claim.
    assert_eq!(project_root(&hits[0]), tmp.path().join("repo/packages/ui"));
    assert_eq!(label(&hits[0]), "Node Dependencies");
}

#[test]
fn a_default_scan_records_claims_without_pricing_them() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("app/package.json"));
    write(
        &tmp.path().join("app/node_modules/a/big.bin"),
        4 * 1024 * 1024,
    );

    let (tree, outcome) = Walker::new(tmp.path(), ruleset()).run_to_tree();

    assert_eq!(outcome.hits, 1);
    assert_eq!(outcome.unmeasured, 1);
    assert_eq!(outcome.reclaimable_bytes, 0);
    assert_eq!(tree.unmeasured(), 1);
    let id = tree.find(&tmp.path().join("app/node_modules")).unwrap();
    assert_eq!(tree.node(id).hit.as_ref().unwrap().size, Size::Unmeasured);
}

#[test]
fn a_default_scan_does_not_read_inside_what_it_claims() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("app/package.json"));
    // An unreadable directory inside the claim. Any traversal of the claim has to report it,
    // so silence is the proof that nothing traversed.
    let sealed = tmp.path().join("app/node_modules/sealed");
    fs::create_dir_all(&sealed).unwrap();
    if !seal(&sealed) {
        return; // running as root, where permissions prove nothing
    }

    let outcome = Walker::new(tmp.path(), ruleset()).run(drop);
    let breakdown = Walker::new(tmp.path(), ruleset())
        .size_mode(SizeMode::Breakdown)
        .run(drop);
    unseal(&sealed);

    assert_eq!(outcome.hits, 1);
    assert!(
        outcome.errors.is_empty(),
        "the claim was read: {:?}",
        outcome.errors
    );
    // And the same fixture under Breakdown does report it, so the fixture is discriminating.
    assert_eq!(breakdown.errors.len(), 1, "{:?}", breakdown.errors);
}

#[test]
fn a_breakdown_covers_the_whole_claimed_subtree_and_nothing_else() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("app/package.json"));
    write(&tmp.path().join("app/node_modules/a/big.bin"), 512 * 1024);
    write(&tmp.path().join("app/node_modules/b/big.bin"), 512 * 1024);
    // Outside the claim, so it must not be counted.
    write(&tmp.path().join("app/src/big.bin"), 4 * 1024 * 1024);

    let hits = scan_with_sizes(tmp.path());

    assert_eq!(hits.len(), 1);
    let size = hits[0].size.bytes().unwrap();
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
fn claims_keep_arriving_while_a_price_is_stuck() {
    // The property background pricing exists for, and the one no synchronous measurement can
    // have: the walk publishes claims at the speed of the walk, whatever pricing is doing.
    //
    // The fixture is built to leave nowhere to hide. ONE walker thread and therefore one
    // pricing thread, and that single pricing thread is jammed on the first price it delivers.
    // Three claims must still arrive. Two designs fail this and both pass a weaker test:
    //
    // - Measuring on the walker thread, as this did until #618: the first claim cannot be
    //   published until its own subtree has been walked, and the size it carries proves it.
    // - Measuring on the walker thread but *reporting* the size as a second event: the walker
    //   thread itself jams on the first price, so claims two and three never come and the
    //   `recv_timeout` below expires.
    let tmp = TempDir::new().unwrap();
    for name in ["a", "b", "c"] {
        touch(&tmp.path().join(name).join("package.json"));
        write(
            &tmp.path().join(name).join("node_modules/big.bin"),
            64 * 1024,
        );
    }

    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let sizes = Mutex::new(Vec::new());
    let (claims, claimed) = channel();
    let walker = breakdown(tmp.path()).threads(1);

    let arrived = std::thread::scope(|threads| {
        threads.spawn(|| {
            walker.run(|found| match found {
                Found::Claim(hit) => claims.send(hit).unwrap(),
                Found::Priced(priced) => {
                    let (open, bell) = &*gate;
                    let mut open = open.lock().unwrap();
                    while !*open {
                        open = bell.wait(open).unwrap();
                    }
                    drop(open);
                    sizes.lock().unwrap().push(priced);
                }
            });
        });

        let arrived: Vec<_> = (0..3)
            .map(|_| claimed.recv_timeout(Duration::from_secs(30)))
            .collect();
        // Released before anything is asserted. A panic here with the pool still jammed would
        // deadlock the scope's join, and a test that hangs reports nothing at all.
        let (open, bell) = &*gate;
        *open.lock().unwrap() = true;
        bell.notify_all();
        arrived
    });

    for (nth, hit) in arrived.into_iter().enumerate() {
        let hit = hit.unwrap_or_else(|_| panic!("claim {nth} waited on a price that was stuck"));
        assert_eq!(
            hit.size,
            Size::Unmeasured,
            "claim {nth} arrived already priced, so the measurement was not backgrounded"
        );
    }

    // ...and every price does arrive, for claims that were published without one.
    let mut sizes = sizes.into_inner().unwrap();
    sizes.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(sizes.len(), 3, "{sizes:?}");
    assert_eq!(sizes[0].path, tmp.path().join("a/node_modules"));
    assert!(sizes[0].size.bytes().unwrap() >= 64 * 1024, "{sizes:?}");
}

#[test]
fn a_backgrounded_price_reaches_the_outcome_and_the_tree() {
    // Pricing off the walker thread must not cost the totals. `run` does not return until the
    // pool has drained, so every number it reports is final — otherwise a consumer that only
    // reads the outcome would see a scan that priced nothing.
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("app/package.json"));
    write(&tmp.path().join("app/node_modules/big.bin"), 256 * 1024);

    let (tree, outcome) = breakdown(tmp.path()).run_to_tree();

    assert_eq!(outcome.hits, 1);
    assert_eq!(outcome.unmeasured, 0, "the claim was left unpriced");
    assert!(outcome.reclaimable_bytes >= 256 * 1024);
    // The rollup carries the late price into every ancestor, and stops calling the claim
    // unpriced. A node with no bytes and a non-zero `unmeasured` is what the TUI renders as
    // "not priced", so leaving it set here would report a priced subtree as unknown.
    assert_eq!(tree.reclaimable(), outcome.reclaimable_bytes);
    assert_eq!(tree.unmeasured(), 0);
    let app = tree.find(&tmp.path().join("app")).unwrap();
    assert_eq!(tree.node(app).reclaimable, outcome.reclaimable_bytes);
    assert_eq!(tree.node(app).unmeasured, 0);
}

#[test]
fn the_tree_rolls_reclaimable_bytes_up_into_every_ancestor() {
    let tmp = TempDir::new().unwrap();
    touch(&tmp.path().join("repos/a/package.json"));
    write(&tmp.path().join("repos/a/node_modules/big.bin"), 256 * 1024);
    touch(&tmp.path().join("repos/b/Cargo.toml"));
    write(&tmp.path().join("repos/b/target/big.bin"), 256 * 1024);

    let (tree, outcome) = breakdown(tmp.path()).run_to_tree();

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

    let (tree, _) = breakdown(tmp.path()).run_to_tree();

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

    let (tree, _) = breakdown(tmp.path()).run_to_tree();
    let id = tree.find(&tmp.path().join("a/node_modules")).unwrap();
    let node = tree.node(id);

    assert!(node.children.is_empty());
    assert_eq!(rule(node.hit.as_ref().unwrap()).id, "node");
    assert_eq!(
        node.reclaimable,
        node.hit.as_ref().unwrap().size.bytes().unwrap()
    );
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

    let (mut tree, _) = breakdown(tmp.path()).run_to_tree();
    tree.sort_by(Sort::by(Order::Size));

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
        Walker::new(&root, ruleset()).run(|found| {
            if let Found::Claim(hit) = found {
                let _ = tx.send(hit);
            }
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
            .run(|found| {
                if matches!(found, Found::Claim(_)) {
                    *hits.lock().unwrap() += 1;
                }
            });
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
        kind = "build"
    "#;
    let rules = Arc::new(Ruleset::with_overrides(user).unwrap());

    let hits = Mutex::new(Vec::new());
    Walker::new(tmp.path(), rules).run(|found| {
        if let Found::Claim(hit) = found {
            hits.lock().unwrap().push(hit);
        }
    });
    let hits = hits.into_inner().unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(rule(&hits[0]).id, "myecosystem");
}
