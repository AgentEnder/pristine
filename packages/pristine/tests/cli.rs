//! What the command line promises, run as the binary rather than as the library.
//!
//! Three of these are properties of the *output* rather than of the scan, and the library
//! cannot hold them on its own: `--min-size` has to be a flag a person can type, a tier-two hit
//! has to say out loud that it does not know how to regenerate what it found, and a tier that
//! could not run has to say so instead of looking like a clean result.

// `allow-unwrap-in-tests` in clippy.toml only reaches code inside a `#[test]` function, and the
// fixture helpers below sit outside one. An unwrap in a fixture is an assertion.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use tempfile::TempDir;

/// A quarter of a mebibyte: over any floor a test sets, and well under the shipped default.
const OVER: usize = 256 * 1024;

fn write(path: &Path, bytes: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![b'x'; bytes]).unwrap();
}

/// Creates a git work tree, with the ambient configuration and any inherited repository
/// redirection shut out so nothing about the developer's machine can change what a test proves.
fn init_repo(path: &Path) {
    fs::create_dir_all(path).unwrap();
    let output = Command::new("git")
        .current_dir(path)
        .args(["init", "--quiet"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "git init in {}", path.display());
}

/// Runs the binary and returns its stdout, asserting it exited cleanly.
fn run(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_pristine"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("the pristine binary should be runnable");
    assert!(
        output.status.success(),
        "pristine {args:?} exited with {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("output should be utf-8")
}

/// Runs the binary expecting it to fail, and returns its stdout and stderr.
fn run_expecting_failure(args: &[&str]) -> (String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_pristine"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("the pristine binary should be runnable");
    assert!(
        !output.status.success(),
        "pristine {args:?} succeeded:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    (
        String::from_utf8(output.stdout).expect("output should be utf-8"),
        String::from_utf8(output.stderr).expect("output should be utf-8"),
    )
}

/// A repository with one gitignored directory of `OVER` bytes and nothing tracked in it.
fn repo_with_reclaimable_sediment() -> TempDir {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "sediment/\n").unwrap();
    write(&tmp.path().join("sediment/blob.bin"), OVER);
    tmp
}

#[test]
fn min_size_defaults_to_ten_mebibytes_and_the_flag_moves_it() {
    let tmp = repo_with_reclaimable_sediment();
    let root = tmp.path().to_string_lossy().into_owned();

    // A quarter of a mebibyte does not clear the shipped floor.
    let defaulted = run(&[&root]);
    assert!(
        !defaulted.contains("sediment"),
        "the default floor let it through:\n{defaulted}"
    );

    // ...and clears one the user lowers.
    let lowered = run(&[&root, "--min-size", "128K"]);
    assert!(
        lowered.contains("sediment"),
        "--min-size did not reach the scan:\n{lowered}"
    );
}

#[test]
fn a_size_the_flag_cannot_read_is_refused_rather_than_guessed_at() {
    let tmp = repo_with_reclaimable_sediment();
    let output = Command::new(env!("CARGO_BIN_EXE_pristine"))
        .args([&tmp.path().to_string_lossy(), "--min-size", "1 potato"])
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "a floor nobody can read must not be silently replaced with one they did not ask for"
    );
}

#[test]
fn the_output_says_which_deletions_are_cheap_and_which_are_not() {
    let tmp = repo_with_reclaimable_sediment();
    fs::write(tmp.path().join(".gitignore"), "sediment/\nnode_modules/\n").unwrap();
    write(&tmp.path().join("app/package.json"), 0);
    write(&tmp.path().join("app/pnpm-lock.yaml"), 0);
    write(&tmp.path().join("app/node_modules/dep/index.js"), OVER);

    let printed = run(&[&tmp.path().to_string_lossy(), "--min-size", "128K"]);

    // Tier one knows the price of getting it back. Tier two knows only that it is safe to
    // remove, and the asymmetry has to survive all the way into what the user reads.
    let sediment = printed
        .lines()
        .find(|line| line.contains("sediment"))
        .unwrap_or_else(|| panic!("no row for the gitignored directory:\n{printed}"));
    let modules = printed
        .lines()
        .find(|line| line.contains("node_modules"))
        .unwrap_or_else(|| panic!("no row for node_modules:\n{printed}"));

    assert!(modules.contains("pnpm install"), "{modules}");
    assert!(
        sediment.contains("no known way to regenerate"),
        "{sediment}"
    );
}

#[test]
fn a_root_that_is_not_there_fails_instead_of_reporting_an_empty_tree() {
    // Nothing was scanned, so "0 directories reclaimable" is not a finding — but it reads
    // exactly like one, and a script cannot tell the difference from the text. The exit status
    // is where that difference has to live.
    let (printed, complaints) = run_expecting_failure(&["/pristine/does/not/exist"]);

    assert!(printed.contains("scan incomplete"), "{printed}");
    assert!(
        complaints.contains("/pristine/does/not/exist"),
        "{complaints}"
    );
}

#[test]
fn a_repository_that_will_not_answer_makes_the_scan_incomplete() {
    let tmp = TempDir::new().unwrap();
    // A `.git` that is not a repository: the fallback tier goes inert here and says so, and
    // that is a scan which did not cover what it was pointed at rather than a clean one.
    fs::create_dir_all(tmp.path().join(".git")).unwrap();
    fs::write(tmp.path().join(".gitignore"), "sediment/\n").unwrap();
    write(&tmp.path().join("sediment/blob.bin"), OVER);

    let (printed, complaints) =
        run_expecting_failure(&[&tmp.path().to_string_lossy(), "--min-size", "128K"]);

    assert!(printed.contains("scan incomplete"), "{printed}");
    assert!(
        complaints.contains("git would not list the index"),
        "{complaints}"
    );
}

#[test]
fn a_scan_that_covered_everything_it_was_pointed_at_succeeds() {
    // The other half of the pair above: a clean scan has to stay clean, or the exit status
    // stops carrying information.
    let tmp = repo_with_reclaimable_sediment();
    let output = Command::new(env!("CARGO_BIN_EXE_pristine"))
        .args([&tmp.path().to_string_lossy(), "--min-size", "128K"])
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert!(output.status.success(), "{:?}", output.status.code());
    let printed = String::from_utf8(output.stdout).unwrap();
    assert!(printed.contains("sediment"), "{printed}");
    assert!(!printed.contains("scan incomplete"), "{printed}");
}

#[test]
fn outside_a_work_tree_the_output_reports_inertness_rather_than_an_empty_result() {
    let tmp = TempDir::new().unwrap();
    // A `.gitignore` and no repository, so the tier cannot run — which is a different thing
    // from running and finding nothing, and the two must not read the same.
    fs::write(tmp.path().join(".gitignore"), "sediment/\n").unwrap();
    write(&tmp.path().join("sediment/blob.bin"), OVER);

    let printed = run(&[&tmp.path().to_string_lossy(), "--min-size", "128K"]);

    assert!(!printed.contains("sediment"), "{printed}");
    assert!(printed.contains("inert"), "{printed}");
}
