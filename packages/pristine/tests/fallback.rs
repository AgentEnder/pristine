//! What the tier-two gitignore fallback promises, stated as fixtures on real git repositories.
//!
//! Tier two is the differentiator from `kondo`, which is curated-rules-only and therefore blind
//! to any ecosystem nobody wrote a rule for. It is also the tier with the most room to be
//! reckless, so most of what follows is negative: an ignored directory holding a tracked file,
//! an untracked directory nothing ignores, a directory nothing but a name suggests is output.
//! Every one of those must be left alone.

// `allow-unwrap-in-tests` in clippy.toml only reaches code inside a `#[test]` function, and the
// fixture helpers below sit outside one. An unwrap in a fixture is an assertion.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use pristine::{Claim, Found, Hit, Kind, Ruleset, Size, SizeMode, Walker};
use tempfile::TempDir;

/// Comfortably over the floor the tests set, and nowhere near the 10 MiB default, so a test
/// that means to exercise the default floor has to say so.
const OVER: usize = 256 * 1024;
/// The floor most tests use, chosen so a fixture costs kilobytes rather than tens of megabytes.
const FLOOR: u64 = 128 * 1024;

fn write(path: &Path, bytes: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![b'x'; bytes]).unwrap();
}

fn touch(path: &Path) {
    write(path, 0);
}

fn ruleset() -> Arc<Ruleset> {
    Arc::new(Ruleset::builtin().unwrap())
}

/// Runs git in `repo`, with the ambient configuration and any inherited repository redirection
/// shut out, so neither a developer's global `.gitconfig` nor a `GIT_DIR` from a surrounding
/// hook or rebase can change what a test proves. The library clears the same variables for the
/// same reason; running this suite under `GIT_DIR=<some other repo>/.git` is what proves it.
fn git(repo: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    command
        .current_dir(repo)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::null());
    for variable in [
        "GIT_DIR",
        "GIT_INDEX_FILE",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CEILING_DIRECTORIES",
        "GIT_NAMESPACE",
    ] {
        command.env_remove(variable);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?} in {}: {}",
        repo.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Creates a git work tree at `path`, along with every directory above it.
fn init_repo(path: &Path) {
    fs::create_dir_all(path).unwrap();
    git(path, &["init", "--quiet"]);
}

/// A walker with tier two on and a floor low enough for a cheap fixture.
fn walker(root: &Path) -> Walker {
    Walker::new(root, ruleset()).min_size(FLOOR)
}

/// Runs a walk and returns the hits in a deterministic order, asserting nothing went wrong.
///
/// Prices are folded back in, because a claim is published unpriced and its size follows as
/// its own event. Tier two's claims arrive priced — the survey has already walked them — so
/// this only ever matters here for a tier-one claim under a breakdown.
fn scan_with(walker: &Walker) -> Vec<Hit> {
    let hits = Mutex::new(Vec::new());
    let sizes = Mutex::new(Vec::new());
    let outcome = walker.run(|found| match found {
        Found::Claim(hit) => hits.lock().unwrap().push(hit),
        Found::Pricing(_) => {}
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

fn scan(root: &Path) -> Vec<Hit> {
    scan_with(&walker(root))
}

/// The paths of every hit, relative to the scan root.
fn claimed(root: &Path) -> Vec<String> {
    scan(root)
        .iter()
        .map(|hit| {
            hit.path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

#[test]
fn an_ignored_directory_with_nothing_tracked_under_it_is_reclaimable() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "sediment/\n").unwrap();
    write(&tmp.path().join("sediment/blob.bin"), OVER);

    let hits = scan(tmp.path());

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, tmp.path().join("sediment"));
    let Claim::Ignored(ref claim) = hits[0].claim else {
        panic!("expected a tier-two claim, got {:?}", hits[0].claim)
    };
    assert_eq!(claim.work_tree, tmp.path());
}

#[test]
fn a_tier_two_hit_admits_it_does_not_know_what_the_directory_is() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "sediment/\n").unwrap();
    write(&tmp.path().join("sediment/blob.bin"), OVER);
    // A tier-one claim in the same scan, so the asymmetry is visible side by side rather than
    // asserted in isolation. That asymmetry is the feature: it tells the user which deletions
    // are cheap.
    touch(&tmp.path().join("app/package.json"));
    write(&tmp.path().join("app/node_modules/dep/index.js"), OVER);

    let hits = scan(tmp.path());

    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].path, tmp.path().join("app/node_modules"));
    assert_eq!(hits[0].label(), "Node Dependencies");
    assert_eq!(hits[1].path, tmp.path().join("sediment"));
    // Not a blank and not a guess: what this tier knows is that git hides the directory, and
    // saying only that is what keeps the asymmetry against the named row above it.
    assert_eq!(hits[1].label(), "Gitignored, kind unknown");
    assert!(hits[1].rule().is_none());
}

#[test]
fn an_ignored_directory_holding_a_tracked_file_is_never_claimed() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "sediment/\n").unwrap();
    write(&tmp.path().join("sediment/blob.bin"), OVER);
    // `git add -f` is how a tracked file ends up inside an ignored directory, and it is the
    // whole reason condition two exists as a check rather than an inference from condition one.
    touch(&tmp.path().join("sediment/deep/nested/keep.txt"));
    git(tmp.path(), &["add", "-f", "sediment/deep/nested/keep.txt"]);

    assert!(claimed(tmp.path()).is_empty());
}

#[test]
fn a_sibling_of_a_tracked_file_inside_an_ignored_directory_is_still_claimed() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "sediment/\n").unwrap();
    touch(&tmp.path().join("sediment/keep.txt"));
    git(tmp.path(), &["add", "-f", "sediment/keep.txt"]);
    // `sediment` is barred by the tracked file, but nothing tracked lives under this, so
    // `git clean -fdX` would remove it and so may we.
    write(&tmp.path().join("sediment/scratch/blob.bin"), OVER);

    assert_eq!(claimed(tmp.path()), ["sediment/scratch"]);
}

#[test]
fn an_ignored_directory_below_the_floor_is_left_alone() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "sediment/\n").unwrap();
    write(&tmp.path().join("sediment/crumb.bin"), 1024);

    assert!(claimed(tmp.path()).is_empty());
}

#[test]
fn the_floor_defaults_to_ten_mebibytes() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "sediment/\n").unwrap();
    write(&tmp.path().join("sediment/blob.bin"), OVER);

    // A quarter of a mebibyte clears the test floor and not the shipped one.
    assert_eq!(claimed(tmp.path()), ["sediment"]);
    let hits = scan_with(&Walker::new(tmp.path(), ruleset()));
    assert!(hits.is_empty(), "the default floor let {hits:?} through");
    assert_eq!(pristine::DEFAULT_MIN_SIZE, 10 * 1024 * 1024);
}

#[test]
fn an_untracked_directory_that_nothing_ignores_is_left_alone() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    // Untracked is not the test. Somebody's unfinished chapter is untracked, and deleting it
    // because it is large and unfamiliar is exactly the failure mode tier two must not have.
    write(&tmp.path().join("manuscript/draft.bin"), OVER);

    assert!(claimed(tmp.path()).is_empty());
}

#[test]
fn the_whole_ignore_stack_is_consulted_not_just_the_root_file() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    // A nested `.gitignore`, which a root-file-only implementation would never read.
    fs::create_dir_all(tmp.path().join("svc")).unwrap();
    fs::write(tmp.path().join("svc/.gitignore"), "sediment/\n").unwrap();
    write(&tmp.path().join("svc/sediment/blob.bin"), OVER);
    // The same name outside the nested file's scope is not ignored, so it is not claimed.
    write(&tmp.path().join("other/sediment/blob.bin"), OVER);

    assert_eq!(claimed(tmp.path()), ["svc/sediment"]);
}

#[test]
fn a_negation_puts_a_directory_back() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "out/\n!out/keep/\n").unwrap();
    write(&tmp.path().join("out/blob.bin"), OVER);
    write(&tmp.path().join("out/keep/blob.bin"), OVER);

    // `out` itself is still ignored and still claimed, and claiming it prunes, so the
    // negation only shows up once `out` is out of the way.
    assert_eq!(claimed(tmp.path()), ["out"]);

    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "out/*\n!out/keep\n").unwrap();
    write(&tmp.path().join("out/sediment/blob.bin"), OVER);
    write(&tmp.path().join("out/keep/blob.bin"), OVER);

    assert_eq!(claimed(tmp.path()), ["out/sediment"]);
}

#[test]
fn info_exclude_is_part_of_the_stack() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    // Not in any `.gitignore`; only in the repository's private exclude file, which is
    // exactly where a developer parks a personal scratch directory.
    fs::create_dir_all(tmp.path().join(".git/info")).unwrap();
    fs::write(tmp.path().join(".git/info/exclude"), "scratch/\n").unwrap();
    write(&tmp.path().join("scratch/blob.bin"), OVER);

    assert_eq!(claimed(tmp.path()), ["scratch"]);
}

#[test]
fn a_tier_one_rule_keeps_what_it_claims() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    // The overwhelmingly common shape: the directory is gitignored *and* a rule knows it.
    // Tier one must win, or every `node_modules` on the machine loses its name.
    fs::write(tmp.path().join(".gitignore"), "node_modules/\n").unwrap();
    touch(&tmp.path().join("package.json"));
    write(&tmp.path().join("node_modules/dep/index.js"), OVER);

    let hits = scan(tmp.path());

    assert_eq!(hits.len(), 1);
    assert!(matches!(hits[0].claim, Claim::Rule(_)));
    assert_eq!(hits[0].label(), "Node Dependencies");
}

#[test]
fn what_tier_two_claims_it_does_not_descend_into() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "sediment/\n").unwrap();
    // A whole project inside the claim. Finding it as a second hit would mean tier two
    // enumerated the tree it had already claimed, which is the cost this design avoids.
    touch(&tmp.path().join("sediment/vendored/package.json"));
    write(
        &tmp.path()
            .join("sediment/vendored/node_modules/dep/index.js"),
        OVER,
    );

    assert_eq!(claimed(tmp.path()), ["sediment"]);
}

#[test]
fn outside_a_git_work_tree_the_fallback_is_inert_and_says_so() {
    let tmp = TempDir::new().unwrap();
    // A `.gitignore` and no repository. Reading it anyway would mean honouring rules no git
    // ever applies, and the honest answer is that there is no signal here at all.
    fs::write(tmp.path().join(".gitignore"), "sediment/\n").unwrap();
    write(&tmp.path().join("sediment/blob.bin"), OVER);

    let outcome = walker(tmp.path()).run(drop);

    assert_eq!(outcome.hits, 0);
    assert_eq!(outcome.fallback.work_trees, 0);
    assert!(outcome.fallback.enabled);
    assert!(
        outcome.fallback.is_inert(),
        "inertness has to be reported, not left to look like an empty result: {:?}",
        outcome.fallback
    );
    assert!(outcome.fallback.outside_work_tree > 0);
}

#[test]
fn a_sweep_reports_the_work_trees_it_consulted_and_the_ground_it_could_not_judge() {
    let tmp = TempDir::new().unwrap();
    for repo in ["one", "two"] {
        let root = tmp.path().join(repo);
        init_repo(&root);
        fs::write(root.join(".gitignore"), "sediment/\n").unwrap();
        write(&root.join("sediment/blob.bin"), OVER);
    }
    // Not a repository, so tier two can say nothing about it.
    write(&tmp.path().join("loose/blob.bin"), OVER);

    let outcome = walker(tmp.path()).run(drop);

    assert_eq!(outcome.hits, 2);
    assert_eq!(outcome.fallback.hits, 2);
    assert_eq!(outcome.fallback.work_trees, 2);
    assert!(!outcome.fallback.is_inert());
    assert!(outcome.fallback.outside_work_tree > 0);
}

#[test]
fn the_work_tree_may_sit_above_the_scan_root() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "sediment/\n").unwrap();
    write(&tmp.path().join("packages/ui/sediment/blob.bin"), OVER);

    // Pointing at a subdirectory of a checkout is ordinary. Refusing to look upward for the
    // work tree would make tier two inert for it, which is not the honest answer: there is a
    // git repository here and it has an opinion.
    let hits = scan(&tmp.path().join("packages/ui"));

    assert_eq!(hits.len(), 1);
    let Claim::Ignored(ref claim) = hits[0].claim else {
        panic!("expected a tier-two claim, got {:?}", hits[0].claim)
    };
    assert_eq!(claim.work_tree, tmp.path());
}

#[test]
fn an_outer_repositorys_ignore_rules_stop_at_a_nested_repository() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "sediment/\n").unwrap();
    // A checkout living inside another checkout. Git does not apply the outer repository's
    // ignore rules inside it, and neither may we: `sediment` here is somebody's source.
    init_repo(&tmp.path().join("inner"));
    write(&tmp.path().join("inner/sediment/blob.bin"), OVER);
    // The same name in the outer repository is ignored, so the fixture proves the boundary
    // rather than proving the rule never fires.
    write(&tmp.path().join("sediment/blob.bin"), OVER);

    assert_eq!(claimed(tmp.path()), ["sediment"]);
}

#[test]
fn a_tracked_file_in_the_outer_repository_does_not_shield_a_nested_one() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    init_repo(&tmp.path().join("inner"));
    fs::write(tmp.path().join("inner/.gitignore"), "sediment/\n").unwrap();
    write(&tmp.path().join("inner/sediment/blob.bin"), OVER);
    // Tracked in the OUTER repository at a path that would look like a prefix match if the
    // wrong index were consulted.
    touch(&tmp.path().join("inner/sediment/decoy.txt"));

    // The inner work tree is the authority, and it tracks nothing.
    assert_eq!(claimed(tmp.path()), ["inner/sediment"]);
}

#[test]
fn a_submodule_inside_an_ignored_directory_bars_the_claim() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "sediment/\n").unwrap();
    write(&tmp.path().join("sediment/blob.bin"), OVER);
    // A gitlink is a tracked entry for the directory itself rather than for a file under it,
    // so a prefix search that only looked for `sediment/...` would miss it and cheerfully
    // claim a directory holding somebody's submodule.
    fs::create_dir_all(tmp.path().join("sediment/sub")).unwrap();
    git(
        tmp.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            // The empty tree: a real object id, and the cheapest one to name.
            "160000,4b825dc642cb6eb9a060e54bf8d69288fbee4904,sediment/sub",
        ],
    );

    assert!(claimed(tmp.path()).is_empty());
}

#[test]
fn a_repository_that_will_not_answer_is_reported_rather_than_guessed_at() {
    let tmp = TempDir::new().unwrap();
    // A `.git` that is not a repository. Everything here looks like a work tree and none of it
    // can be judged, which is inertness by another route and must read as such rather than as
    // a clean scan.
    fs::create_dir_all(tmp.path().join(".git")).unwrap();
    fs::write(tmp.path().join(".gitignore"), "sediment/\n").unwrap();
    write(&tmp.path().join("sediment/blob.bin"), OVER);

    let outcome = walker(tmp.path()).run(drop);

    assert_eq!(outcome.hits, 0);
    assert!(outcome.fallback.is_inert());
    assert_eq!(outcome.errors.len(), 1, "{:?}", outcome.errors);
    assert_eq!(outcome.errors[0].path.as_deref(), Some(tmp.path()));
}

#[cfg(unix)]
#[test]
fn a_subtree_that_cannot_be_read_through_is_not_claimed() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "sediment/\n").unwrap();
    write(&tmp.path().join("sediment/blob.bin"), OVER);
    // Unreadable, so "holds no checkout" is not established — and that is a claim about the
    // whole subtree, not about the part that happened to be legible.
    let sealed = tmp.path().join("sediment/sealed");
    fs::create_dir(&sealed).unwrap();
    fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&sealed).is_ok() {
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755)).unwrap();
        return; // running as root, where permissions prove nothing
    }

    let outcome = walker(tmp.path()).run(drop);
    fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(outcome.hits, 0);
    // Every complaint names the one directory nobody could read, so a user who expected
    // `sediment` to show up can see exactly why it did not.
    assert!(!outcome.errors.is_empty());
    assert!(
        outcome
            .errors
            .iter()
            .all(|error| error.path.as_deref() == Some(sealed.as_path())),
        "{:?}",
        outcome.errors
    );
}

#[test]
fn a_tracked_file_under_a_non_ascii_path_still_bars_the_claim() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "build/\n").unwrap();
    // `café` with a combining accent, which is what `readdir` hands back on macOS. git's
    // `core.precomposeunicode` is on there by default, so `git add` stores the *composed*
    // form and a raw byte comparison misses the tracked file below — leaving a directory that
    // demonstrably holds one looking free to delete. Only one component has to be non-ASCII,
    // and the ignore rule matching it is an ordinary ASCII `build/`.
    let cafe = "cafe\u{301}";
    write(&tmp.path().join(cafe).join("build/blob.bin"), OVER);
    touch(&tmp.path().join(cafe).join("build/keep.txt"));
    git(
        tmp.path(),
        &["add", "-f", &format!("{cafe}/build/keep.txt")],
    );

    assert!(
        claimed(tmp.path()).is_empty(),
        "a tracked file went missing behind a normalization mismatch"
    );
}

#[test]
fn a_decoy_git_dir_in_the_environment_cannot_redirect_the_index_lookup() {
    // `GIT_DIR` and `GIT_INDEX_FILE` beat `git -C <root>`, and anything running inside a hook,
    // a rebase or a filter-branch has them set. The environment has to be poisoned before the
    // process starts and cargo runs every test in one process, so the actual scan happens in a
    // child — this half only sets the trap.
    let decoy = TempDir::new().unwrap();
    init_repo(decoy.path());
    touch(&decoy.path().join("decoy.txt"));
    git(decoy.path(), &["add", "decoy.txt"]);

    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "the_scan_that_runs_under_a_decoy_git_dir",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env("GIT_DIR", decoy.path().join(".git"))
        .env("GIT_INDEX_FILE", decoy.path().join(".git/index"))
        .env("GIT_WORK_TREE", decoy.path())
        .status()
        .unwrap();

    assert!(
        status.success(),
        "the scan read the decoy repository's index instead of the one it was pointed at"
    );
}

/// The child half of [`a_decoy_git_dir_in_the_environment_cannot_redirect_the_index_lookup`].
/// Ignored so it only ever runs from that parent, with the trap already set.
#[test]
#[ignore = "run by its parent with a decoy GIT_DIR in the environment"]
fn the_scan_that_runs_under_a_decoy_git_dir() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "sediment/\n").unwrap();
    write(&tmp.path().join("sediment/blob.bin"), OVER);
    touch(&tmp.path().join("sediment/keep.txt"));
    git(tmp.path(), &["add", "-f", "sediment/keep.txt"]);

    // Honour the decoy and this repository's index looks empty, so `sediment` looks untracked
    // and is claimed. Everything the tier promises rests on reading the right index.
    assert!(claimed(tmp.path()).is_empty());
}

#[test]
fn the_fallback_can_be_switched_off() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "sediment/\n").unwrap();
    write(&tmp.path().join("sediment/blob.bin"), OVER);

    let outcome = walker(tmp.path()).fallback(false).run(drop);

    assert_eq!(outcome.hits, 0);
    assert!(!outcome.fallback.enabled);
    assert!(
        !outcome.fallback.is_inert(),
        "a tier switched off is not a tier with nothing to work with"
    );
}

#[test]
fn a_tier_two_claim_carries_a_real_size_even_on_a_default_scan() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "sediment/\nnode_modules/\n").unwrap();
    for part in 0..8 {
        write(&tmp.path().join(format!("sediment/{part}.bin")), OVER);
    }
    // Tier one beside it, to pin the asymmetry: this one is still unpriced by default, because
    // nothing forced the scan to look inside it.
    touch(&tmp.path().join("app/package.json"));
    write(&tmp.path().join("app/node_modules/dep/index.js"), OVER);

    let hits = scan(tmp.path());

    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].size, Size::Unmeasured, "tier one");
    let bytes = hits[1].size.bytes().expect("tier two");
    assert!(bytes >= 8 * OVER as u64, "{bytes}");
}

#[test]
fn a_directory_holding_a_checkout_is_not_collapsed_into_one_removal() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "sandboxes/\n").unwrap();
    // The shape this rule exists for: checkouts parked under an ignored directory. Claiming
    // `sandboxes` would be offering to delete uncommitted work in `sandboxes/box/checkout`.
    init_repo(&tmp.path().join("sandboxes/box/checkout"));
    write(&tmp.path().join("sandboxes/box/checkout/draft.bin"), OVER);
    // A sibling with no checkout under it, which `git clean` would remove and so may we.
    write(&tmp.path().join("sandboxes/spill/blob.bin"), OVER);

    let outcome = walker(tmp.path()).run(drop);

    assert_eq!(claimed(tmp.path()), ["sandboxes/spill"]);
    assert!(outcome.fallback.holding_a_checkout >= 1);
}

#[test]
fn a_linked_work_trees_dot_git_file_counts_as_a_checkout() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "sediment/\n").unwrap();
    write(&tmp.path().join("sediment/blob.bin"), OVER);
    // A linked work tree keeps a `.git` file naming the real gitdir rather than a directory,
    // and it is every bit as much somebody's checkout.
    fs::create_dir_all(tmp.path().join("sediment/worktree")).unwrap();
    fs::write(
        tmp.path().join("sediment/worktree/.git"),
        "gitdir: /elsewhere/.git/worktrees/w\n",
    )
    .unwrap();

    let outcome = walker(tmp.path()).run(drop);

    assert_eq!(outcome.hits, 0);
    assert_eq!(outcome.fallback.holding_a_checkout, 1);
    // And the dangling work tree is itself unjudgeable, which is reported rather than passed
    // over — the same rule as any repository that will not answer.
    assert_eq!(outcome.errors.len(), 1, "{:?}", outcome.errors);
}

#[test]
fn the_rollup_tree_carries_tier_two_claims_alongside_tier_one() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "sediment/\nnode_modules/\n").unwrap();
    write(&tmp.path().join("app/sediment/blob.bin"), OVER);
    touch(&tmp.path().join("app/package.json"));
    write(&tmp.path().join("app/node_modules/dep/index.js"), OVER);

    let (tree, outcome) = walker(tmp.path())
        .size_mode(SizeMode::Breakdown)
        .run_to_tree();

    assert_eq!(outcome.hits, 2);
    let app = tree.find(&tmp.path().join("app")).unwrap();
    let sediment = tree.find(&tmp.path().join("app/sediment")).unwrap();
    let modules = tree.find(&tmp.path().join("app/node_modules")).unwrap();
    assert_eq!(
        tree.node(app).reclaimable,
        tree.node(sediment).reclaimable + tree.node(modules).reclaimable
    );
    assert_eq!(tree.reclaimable(), outcome.reclaimable_bytes);
}

// ---------------------------------------------------------------------------------------
// Gitignored FILES.
//
// A different job from the rest of the tier, which is what most of these are about: the size
// floor does not reach them, they are never unpriced, and a walk that did not ask for them
// must behave exactly as it did before. The safety half — that an unrecoverable one is never
// swept up by a mark on a parent and never removed by a script that did not name the flag —
// is in `tui::state` and `tests/cli.rs`, because it is about what is done with a claim rather
// than about finding one.
// ---------------------------------------------------------------------------------------

/// A walker that claims gitignored files as well as directories.
fn with_files(root: &Path) -> Walker {
    walker(root).ignored_files(true)
}

/// A repository whose `.gitignore` hides an env file, a log and a directory under the floor.
fn repo_with_ignored_files() -> TempDir {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), ".env*\n*.log\nscraps/\n").unwrap();
    write(&tmp.path().join(".env"), 40);
    write(&tmp.path().join(".env.local"), 40);
    write(&tmp.path().join("build.log"), 40);
    // Under the floor on purpose: a directory this small is not worth a row, and the files
    // beside it are — which is the whole distinction.
    write(&tmp.path().join("scraps/leftover.bin"), 16);
    touch(&tmp.path().join("kept.txt"));
    git(tmp.path(), &["add", "kept.txt", ".gitignore"]);
    tmp
}

#[test]
fn a_gitignored_file_is_invisible_until_the_walk_is_asked_for_files() {
    // The bug this exists to fix, and its other half: a walk that did not ask is unchanged.
    let tmp = repo_with_ignored_files();

    assert!(
        claimed(tmp.path()).is_empty(),
        "a default sweep claims no files, and `scraps/` is under the floor"
    );

    let mut found: Vec<String> = scan_with(&with_files(tmp.path()))
        .iter()
        .map(|hit| {
            hit.path
                .strip_prefix(tmp.path())
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    found.sort();
    assert_eq!(
        found,
        [".env", ".env.local", "build.log", "scraps/leftover.bin"]
    );
}

#[test]
fn a_directory_the_floor_refused_is_descended_into_and_its_files_claimed_one_by_one() {
    // The consequence of the floor and prune-on-match meeting, pinned deliberately because it
    // reads as a surprise otherwise. An ignored directory OVER the floor is claimed and never
    // descended into, so its contents are one row. One UNDER it is refused, the walk goes in —
    // as it already did, since a rule may still match deeper — and every ignored file inside is
    // a candidate on its own.
    //
    // Which is the behaviour worth having: a `.env` parked in a small ignored directory is
    // exactly the thing this feature exists to surface, and it is found here and covered by
    // the directory's own claim in the other case. What it costs is that the floor stops
    // thinning that one subtree, which is why it is a test rather than a footnote.
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "small/\nbig/\n").unwrap();
    write(&tmp.path().join("small/.env"), 40);
    write(&tmp.path().join("big/.env"), 40);
    write(&tmp.path().join("big/blob.bin"), OVER);

    let mut found: Vec<String> = scan_with(&with_files(tmp.path()))
        .iter()
        .map(|hit| {
            hit.path
                .strip_prefix(tmp.path())
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    found.sort();
    assert_eq!(found, ["big", "small/.env"]);
}

#[test]
fn the_size_floor_does_not_reach_a_file() {
    // The floor is about rows on a list sorted by size: an ignored directory under it is not
    // worth one. A 40-byte `.env` is worth one for a reason that has nothing to do with its
    // size, so the floor has nothing to say about it — proved against a floor far above every
    // file in the fixture, with `scraps/` failing the same floor to show it is still in force
    // where it belongs.
    let tmp = repo_with_ignored_files();

    let hits = scan_with(&with_files(tmp.path()).min_size(1024 * 1024));

    assert_eq!(hits.len(), 4, "{hits:?}");
    assert!(
        hits.iter()
            .all(|hit| hit.size.bytes().unwrap() < 1024 * 1024),
        "every one of these is far below the floor it was found under"
    );
    assert!(
        !hits.iter().any(|hit| hit.path.ends_with("scraps")),
        "the floor still keeps a small ignored DIRECTORY off the list"
    );
}

#[test]
fn a_file_is_always_priced_even_on_a_scan_that_prices_nothing_else() {
    // One `lstat` is the exact answer in constant time, so a file never enters the unpriced
    // state a tier-one directory lives in — and the pricing pool never has to grow a branch
    // for one. Asserted beside a tier-one claim on the same default scan, which is a dash.
    let tmp = repo_with_ignored_files();
    touch(&tmp.path().join("app/package.json"));
    write(&tmp.path().join("app/node_modules/dep/index.js"), OVER);

    let hits = scan_with(&with_files(tmp.path()).size_mode(SizeMode::Skip));

    let env = hits.iter().find(|hit| hit.path.ends_with(".env")).unwrap();
    assert!(env.size.bytes().is_some(), "a file arrived unpriced");
    let modules = hits
        .iter()
        .find(|hit| hit.path.ends_with("node_modules"))
        .unwrap();
    assert_eq!(
        modules.size,
        Size::Unmeasured,
        "a default scan still prices no tier-one directory"
    );
}

#[test]
fn a_files_kind_is_read_off_its_name_and_says_what_losing_it_costs() {
    let tmp = repo_with_ignored_files();

    let hits = scan_with(&with_files(tmp.path()));
    let kind_of = |name: &str| {
        hits.iter()
            .find(|hit| hit.path.ends_with(name))
            .unwrap_or_else(|| panic!("no claim for {name}"))
            .kind()
    };

    assert_eq!(kind_of(".env"), Some(Kind::Unrecoverable));
    assert_eq!(kind_of(".env.local"), Some(Kind::Unrecoverable));
    assert_eq!(kind_of("build.log"), Some(Kind::Noise));
    let env = hits.iter().find(|hit| hit.path.ends_with(".env")).unwrap();
    assert_eq!(env.label(), "Gitignored, unrecoverable");
    assert!(env.is_ignored_file());
    assert!(env.rule().is_none(), "no rule named it, and none could");
}

#[test]
fn a_gitignored_file_whose_name_says_nothing_is_still_claimed_and_still_unnamed() {
    // The tier-two asymmetry, in a file's clothes: git knows the file is disposable and
    // nothing knows what it is. Claiming only the names in a table would make the feature a
    // pattern list rather than a tier.
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "dump.sql\n").unwrap();
    write(&tmp.path().join("dump.sql"), 4096);

    let hits = scan_with(&with_files(tmp.path()));

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].kind(), None);
    assert_eq!(hits[0].label(), "Gitignored, kind unknown");
    assert!(hits[0].is_ignored_file());
}

#[test]
fn a_tracked_file_matching_an_ignore_pattern_is_never_claimed() {
    // The safety property, on the shape where it is easiest to get wrong. `git add -f` on an
    // ignored path is ordinary — a committed `.env.example`, a checked-in log — and the index
    // is the only thing that knows. It is an exact-path question for a file rather than a
    // prefix one, which is the lookup a directory-only tier never had to get right.
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "*.env\n").unwrap();
    write(&tmp.path().join("committed.env"), 40);
    write(&tmp.path().join("scratch.env"), 40);
    git(tmp.path(), &["add", "-f", "committed.env"]);

    let hits = scan_with(&with_files(tmp.path()));

    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].path, tmp.path().join("scratch.env"));
}

#[test]
fn a_file_inside_a_claimed_directory_is_not_claimed_again() {
    // Prune-on-match is what stops the feature turning one 40 GB `node_modules` into four
    // hundred thousand rows. It is not a new rule — a claimed directory is never descended
    // into — but it is the one this change could most easily have broken, since a file is
    // exactly what lives under a pruned directory.
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "node_modules/\n").unwrap();
    touch(&tmp.path().join("package.json"));
    write(&tmp.path().join("node_modules/dep/.env"), 40);
    write(&tmp.path().join("node_modules/dep/index.js"), OVER);

    let hits = scan_with(&with_files(tmp.path()));

    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].path, tmp.path().join("node_modules"));
}

#[test]
fn a_file_a_pattern_only_matches_as_a_directory_is_left_alone() {
    // git's own matcher answers differently for a file and a directory — `scraps/` matches
    // only the directory — so the walk has to tell it which it is asking about. Passing the
    // wrong one claims files a `.gitignore` never mentioned, which is the failure this whole
    // tier is built to avoid.
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "scraps/\n").unwrap();
    // A FILE named `scraps`, beside a directory the same pattern does claim.
    write(&tmp.path().join("scraps"), 40);

    let hits = scan_with(&with_files(tmp.path()));

    assert!(hits.is_empty(), "{hits:?}");
}

#[test]
fn the_report_says_whether_files_were_looked_for_at_all() {
    // "There were none" and "nobody looked" are opposite facts and they look identical
    // without something that says which — this report's founding rule, applied to the second
    // question it now answers.
    let tmp = repo_with_ignored_files();

    let quiet = walker(tmp.path()).run(|_| {});
    assert!(!quiet.fallback.files_enabled);
    assert_eq!(quiet.fallback.files, 0);

    let asked = with_files(tmp.path()).run(|_| {});
    assert!(asked.fallback.files_enabled);
    assert_eq!(asked.fallback.files, 4);
    assert_eq!(asked.fallback.hits, 4);
}
