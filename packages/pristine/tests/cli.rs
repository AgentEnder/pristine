//! What the command line promises, run as the binary rather than as the library.
//!
//! Some of these are properties of the *output* and the library cannot hold them on its own:
//! `--min-size` has to be a flag a person can type, a tier-two hit has to say out loud that it
//! does not know how to regenerate what it found, and a tier that could not run has to say so
//! instead of looking like a clean result.
//!
//! The rest are properties of the *run*, and no library test can hold them either: `--dry-run`
//! has to be inert, the confirmation has to default to no, and a run that could not do
//! everything it was asked has to exit non-zero. These drive the real binary because that is
//! the only place any of it is true or false.

// `allow-unwrap-in-tests` in clippy.toml only reaches code inside a `#[test]` function, and the
// fixture helpers below sit outside one. An unwrap in a fixture is an assertion.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
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

/// A repository with one gitignored directory of `OVER` bytes and nothing tracked in it.
fn repo_with_reclaimable_sediment() -> TempDir {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path());
    fs::write(tmp.path().join(".gitignore"), "sediment/\n").unwrap();
    write(&tmp.path().join("sediment/blob.bin"), OVER);
    tmp
}

/// A node project with a `node_modules` the ruleset will claim.
fn project(root: &Path, name: &str) -> PathBuf {
    let project = root.join(name);
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("package.json"), "{}").unwrap();
    let modules = project.join("node_modules");
    fs::create_dir_all(modules.join("left-pad")).unwrap();
    fs::write(modules.join("left-pad/index.js"), vec![b'x'; 4096]).unwrap();
    modules
}

/// A scan root that is already resolved, so a test can compare against what a plan holds.
fn fixture() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    (tmp, base)
}

struct Run {
    stdout: String,
    stderr: String,
    ok: bool,
}

/// Runs the real binary with `answer` on its standard input.
fn run(root: &Path, args: &[&str], answer: &str) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pristine"))
        .arg(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(answer.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    Run {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        ok: output.status.success(),
    }
}

/// Runs the binary with nothing on its input and returns its stdout, asserting it exited
/// cleanly.
fn succeeds(root: &Path, args: &[&str]) -> String {
    let run = run(root, args, "");
    assert!(
        run.ok,
        "pristine {args:?} failed:\n{}\n{}",
        run.stdout, run.stderr
    );
    run.stdout
}

/// Runs the binary expecting it to fail, and returns its stdout and stderr.
fn fails(root: &Path, args: &[&str]) -> (String, String) {
    let run = run(root, args, "");
    assert!(!run.ok, "pristine {args:?} succeeded:\n{}", run.stdout);
    (run.stdout, run.stderr)
}

// ---------------------------------------------------------------------------------------
// The floor, and what a listing says about each tier.
// ---------------------------------------------------------------------------------------

#[test]
fn min_size_defaults_to_ten_mebibytes_and_the_flag_moves_it() {
    let tmp = repo_with_reclaimable_sediment();

    // A quarter of a mebibyte does not clear the shipped floor.
    let defaulted = succeeds(tmp.path(), &[]);
    assert!(
        !defaulted.contains("sediment"),
        "the default floor let it through:\n{defaulted}"
    );

    // ...and clears one the user lowers.
    let lowered = succeeds(tmp.path(), &["--min-size", "128K"]);
    assert!(
        lowered.contains("sediment"),
        "--min-size did not reach the scan:\n{lowered}"
    );
}

#[test]
fn a_size_the_flag_cannot_read_is_refused_rather_than_guessed_at() {
    let tmp = repo_with_reclaimable_sediment();
    let run = run(tmp.path(), &["--min-size", "1 potato"], "");

    assert!(
        !run.ok,
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

    let printed = succeeds(tmp.path(), &["--min-size", "128K"]);

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
fn outside_a_work_tree_the_output_reports_inertness_rather_than_an_empty_result() {
    let tmp = TempDir::new().unwrap();
    // A `.gitignore` and no repository, so the tier cannot run — which is a different thing
    // from running and finding nothing, and the two must not read the same.
    fs::write(tmp.path().join(".gitignore"), "sediment/\n").unwrap();
    write(&tmp.path().join("sediment/blob.bin"), OVER);

    let printed = succeeds(tmp.path(), &["--min-size", "128K"]);

    assert!(!printed.contains("sediment"), "{printed}");
    assert!(printed.contains("inert"), "{printed}");
}

// ---------------------------------------------------------------------------------------
// Asking for sizes. A scan leaves tier one unpriced by design, so "how much do I get back"
// has to be a question the command line can ask.
// ---------------------------------------------------------------------------------------

/// The row for `path`, whatever the listing put on it. Matched by containment rather than by
/// suffix because a listing row carries the regeneration command after the path.
fn row<'a>(printed: &'a str, path: &str) -> &'a str {
    printed
        .lines()
        .find(|line| line.contains(path))
        .unwrap_or_else(|| panic!("no row for {path}:\n{printed}"))
}

#[test]
fn a_default_scan_leaves_a_claim_unpriced_and_says_how_to_price_it() {
    let (_tmp, base) = fixture();
    project(&base, "app");

    let printed = succeeds(&base, &[]);

    // The dash is not a zero, and a reader with no way to turn it into a number is being
    // told the headline question is unanswerable rather than unasked.
    assert!(row(&printed, "app/node_modules").contains('—'), "{printed}");
    assert!(printed.contains("1 not priced"), "{printed}");
    assert!(printed.contains("--breakdown"), "{printed}");
    assert!(printed.contains("--breakdown-under"), "{printed}");
}

#[test]
fn a_breakdown_prices_what_the_scan_pruned_at() {
    let (_tmp, base) = fixture();
    project(&base, "app");

    let printed = succeeds(&base, &["--breakdown"]);

    assert!(
        !row(&printed, "app/node_modules").contains('—'),
        "{printed}"
    );
    assert!(printed.contains("0 not priced"), "{printed}");
    // Nothing left to explain, so the hint stays out of the way.
    assert!(!printed.contains("--breakdown-under"), "{printed}");
}

#[test]
fn a_scoped_breakdown_prices_one_subtree_and_lists_the_rest_unpriced() {
    let (_tmp, base) = fixture();
    project(&base, "wanted");
    project(&base, "elsewhere");

    let printed = succeeds(
        &base,
        &["--breakdown-under", base.join("wanted").to_str().unwrap()],
    );

    // The point of the flag: a number for the subtree in question, at that subtree's price,
    // with the rest of the scan still listed rather than hidden.
    assert!(
        !row(&printed, "wanted/node_modules").contains('—'),
        "{printed}"
    );
    assert!(
        row(&printed, "elsewhere/node_modules").contains('—'),
        "{printed}"
    );
    assert!(printed.contains("1 not priced"), "{printed}");
}

#[test]
fn a_breakdown_scope_the_scan_does_not_cover_is_refused() {
    let (_tmp, base) = fixture();
    project(&base, "app");
    let elsewhere = TempDir::new().unwrap();

    // Honoured silently, this would price nothing and print a listing of dashes — which reads
    // exactly like a subtree that is worth nothing.
    let (_, complaints) = fails(
        &base,
        &["--breakdown-under", elsewhere.path().to_str().unwrap()],
    );

    assert!(
        complaints.contains("not inside the scan root"),
        "{complaints}"
    );
}

#[test]
fn a_breakdown_reaches_the_plan_as_well_as_the_listing() {
    let (_tmp, base) = fixture();
    let modules = project(&base, "app");

    let printed = succeeds(&base, &["--dry-run", "--breakdown"]);

    assert!(modules.exists(), "a dry run deleted something");
    assert!(printed.contains("0 not priced"), "{printed}");
    assert!(
        !printed.contains("plan: 1 directory, 0 B priced"),
        "the plan took the listing's sizes but not the breakdown's:\n{printed}"
    );
}

// ---------------------------------------------------------------------------------------
// Both tiers reach one plan. This is the property the #588/#594 merge creates, and neither
// side could have had a test for it.
// ---------------------------------------------------------------------------------------

#[test]
fn one_plan_covers_both_tiers() {
    let (_tmp, base) = fixture();
    init_repo(&base);
    // Only the tier-two candidate is gitignored. `node_modules` is claimed by a rule, which
    // runs first and prunes, so the two rows below cannot have come from the same tier.
    fs::write(base.join(".gitignore"), "sediment/\n").unwrap();
    write(&base.join("sediment/blob.bin"), OVER);
    project(&base, "app");

    let printed = succeeds(&base, &["--dry-run", "--min-size", "128K"]);

    let rows: Vec<&str> = printed.lines().collect();
    assert!(
        rows.iter().any(|line| line.ends_with("  sediment")),
        "the fallback tier's claim never reached the plan:\n{printed}"
    );
    assert!(
        rows.iter().any(|line| line.ends_with("  app/node_modules")),
        "the ruleset's claim never reached the plan:\n{printed}"
    );
    // ...and the plan says the second tier ran, rather than leaving the reader to infer it
    // from a row that a rule could equally have produced.
    assert!(
        printed.contains("fallback tier: 1 directory found in 1 work tree"),
        "{printed}"
    );
}

#[test]
fn a_removal_takes_what_either_tier_claimed() {
    let (_tmp, base) = fixture();
    init_repo(&base);
    fs::write(base.join(".gitignore"), "sediment/\n").unwrap();
    write(&base.join("sediment/blob.bin"), OVER);
    let modules = project(&base, "app");

    let run = run(&base, &["--delete", "--yes", "--min-size", "128K"], "");

    assert!(run.ok, "{}", run.stderr);
    assert!(
        !base.join("sediment").exists(),
        "a tier-two claim survived the removal:\n{}",
        run.stdout
    );
    assert!(!modules.exists(), "{}", run.stdout);
    // The evidence each tier used is untouched: the deleter removes what the plan named and
    // nothing that named it.
    assert!(base.join(".gitignore").exists(), "{}", run.stdout);
    assert!(base.join("app/package.json").exists(), "{}", run.stdout);
}

// ---------------------------------------------------------------------------------------
// `--dry-run` prints the resolved plan and deletes nothing.
// ---------------------------------------------------------------------------------------

#[test]
fn a_dry_run_prints_the_plan_and_removes_nothing() {
    let (_tmp, base) = fixture();
    let modules = project(&base, "app");

    let run = run(&base, &["--dry-run"], "");

    assert!(run.ok, "{}", run.stderr);
    assert!(
        modules.join("left-pad/index.js").exists(),
        "a dry run deleted something"
    );
    // The resolved plan, not a summary of it: the path it would unlink, and the fact that
    // it did not.
    assert!(
        run.stdout
            .lines()
            .any(|line| line.ends_with("  app/node_modules")),
        "{}",
        run.stdout
    );
    // Relative to the scan root. A plan holds resolved paths, so printing them as they are
    // held means every row is prefixed with a home directory nobody needs to read.
    assert!(
        !run.stdout.contains(base.to_str().unwrap()),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("dry run"), "{}", run.stdout);
}

#[test]
fn a_dry_run_names_what_it_would_refuse_as_well_as_what_it_would_remove() {
    let (_tmp, base) = fixture();
    let modules = project(&base, "app");
    fs::create_dir_all(modules.join("vendored/.git")).unwrap();

    let run = run(&base, &["--dry-run", "--older-than", "60d"], "");

    assert!(run.ok, "{}", run.stderr);
    // Everything here was touched seconds ago, so the age floor keeps all of it — and a
    // plan that silently listed nothing would look identical to a clean machine.
    assert!(run.stdout.contains("touched"), "{}", run.stdout);
    assert!(run.stdout.contains("kept"), "{}", run.stdout);
}

// ---------------------------------------------------------------------------------------
// The final confirmation defaults to no.
// ---------------------------------------------------------------------------------------

#[test]
fn a_bare_enter_at_the_confirmation_removes_nothing() {
    let (_tmp, base) = fixture();
    let modules = project(&base, "app");

    let run = run(&base, &["--delete"], "\n");

    assert!(run.ok, "{}", run.stderr);
    assert!(modules.exists(), "enter was read as consent");
    assert!(run.stdout.contains("[y/N]"), "{}", run.stdout);
    assert!(run.stdout.contains("nothing was removed"), "{}", run.stdout);
}

#[test]
fn a_closed_standard_input_removes_nothing() {
    let (_tmp, base) = fixture();
    let modules = project(&base, "app");

    // A script that pipes nothing at an irreversible prompt did not expect one.
    let run = run(&base, &["--delete"], "");

    assert!(run.ok, "{}", run.stderr);
    assert!(modules.exists(), "end of input was read as consent");
}

#[test]
fn saying_yes_removes_what_the_plan_listed() {
    let (_tmp, base) = fixture();
    let modules = project(&base, "app");

    let run = run(&base, &["--delete"], "y\n");

    assert!(run.ok, "{}", run.stderr);
    assert!(!modules.exists(), "{}", run.stdout);
    assert!(base.join("app/package.json").exists(), "the project went");
    assert!(run.stdout.contains("removed"), "{}", run.stdout);
}

#[test]
fn the_yes_flag_is_how_a_script_consents() {
    let (_tmp, base) = fixture();
    let modules = project(&base, "app");

    let run = run(&base, &["--delete", "--yes"], "");

    assert!(run.ok, "{}", run.stderr);
    assert!(!modules.exists(), "{}", run.stdout);
    assert!(!run.stdout.contains("[y/N]"), "it asked anyway");
}

// ---------------------------------------------------------------------------------------
// `--older-than` keeps what was touched recently.
// ---------------------------------------------------------------------------------------

#[test]
fn an_age_floor_keeps_a_directory_that_was_used_today() {
    let (_tmp, base) = fixture();
    let modules = project(&base, "app");

    let run = run(&base, &["--delete", "--yes", "--older-than", "7d"], "");

    assert!(run.ok, "{}", run.stderr);
    assert!(modules.exists(), "a directory made seconds ago was removed");
}

#[test]
fn a_duration_that_cannot_be_read_is_refused_rather_than_guessed_at() {
    let (_tmp, base) = fixture();

    for bad in ["", "7", "d", "1.5d", "-1d", "7 potatoes", "7s"] {
        let run = run(&base, &["--older-than", bad], "");
        assert!(!run.ok, "`{bad}` was accepted");
    }
}

// ---------------------------------------------------------------------------------------
// Failures set a non-zero exit.
// ---------------------------------------------------------------------------------------

#[test]
fn a_root_that_is_not_there_fails_instead_of_reporting_an_empty_tree() {
    // Nothing was scanned, so "0 directories reclaimable" is not a finding — but it reads
    // exactly like one, and a script cannot tell the difference from the text. The exit status
    // is where that difference has to live.
    let (printed, complaints) = fails(Path::new("/pristine/does/not/exist"), &[]);

    assert!(printed.contains("scan incomplete"), "{printed}");
    assert!(
        complaints.contains("/pristine/does/not/exist"),
        "{complaints}"
    );
}

#[test]
fn a_root_that_cannot_be_read_fails_a_dry_run_too() {
    // The same promise on the planning path: a plan built over a scan that saw nothing is not
    // an empty plan, and only the status says so.
    let (_tmp, base) = fixture();

    let run = run(&base.join("does-not-exist"), &["--dry-run"], "");

    assert!(!run.ok, "{}", run.stdout);
}

#[test]
fn a_repository_that_will_not_answer_makes_the_scan_incomplete() {
    let tmp = TempDir::new().unwrap();
    // A `.git` that is not a repository: the fallback tier goes inert here and says so, and
    // that is a scan which did not cover what it was pointed at rather than a clean one.
    fs::create_dir_all(tmp.path().join(".git")).unwrap();
    fs::write(tmp.path().join(".gitignore"), "sediment/\n").unwrap();
    write(&tmp.path().join("sediment/blob.bin"), OVER);

    let (printed, complaints) = fails(tmp.path(), &["--min-size", "128K"]);

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

    let printed = succeeds(tmp.path(), &["--min-size", "128K"]);

    assert!(printed.contains("sediment"), "{printed}");
    assert!(!printed.contains("scan incomplete"), "{printed}");
}

#[cfg(unix)]
#[test]
fn a_removal_that_could_not_finish_exits_non_zero() {
    use std::os::unix::fs::PermissionsExt;
    let (_tmp, base) = fixture();
    let modules = project(&base, "app");
    let sealed = modules.join("left-pad");
    fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&sealed).is_ok() {
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755)).unwrap();
        return; // running as root, where permissions prove nothing
    }

    let run = run(&base, &["--delete", "--yes"], "");
    fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755)).unwrap();

    assert!(!run.ok, "a batch that failed reported success");
    assert!(!run.stderr.is_empty(), "the failure was not reported");
}

#[test]
fn a_checkout_under_a_claim_is_reported_rather_than_silently_skipped() {
    let (_tmp, base) = fixture();
    let modules = project(&base, "app");
    let checkout = modules.join("vendored");
    fs::create_dir_all(checkout.join(".git")).unwrap();
    fs::write(checkout.join(".git/HEAD"), "ref: refs/heads/main").unwrap();

    let run = run(&base, &["--delete", "--yes"], "");

    assert!(run.ok, "{}", run.stderr);
    assert!(
        checkout.join(".git/HEAD").exists(),
        "a checkout was removed"
    );
    assert!(run.stdout.contains("checkout"), "{}", run.stdout);
}
