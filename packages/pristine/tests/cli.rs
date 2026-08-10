//! The command line's half of the safety model.
//!
//! Three of the safety model's promises are properties of the *program*, not of the library,
//! and no library test can hold them: `--dry-run` has to be inert, the confirmation has to
//! default to no, and a run that could not do everything it was asked has to exit non-zero.
//! These drive the real binary because that is the only place those are true or false.

// `allow-unwrap-in-tests` in clippy.toml only reaches code inside a `#[test]` function, and
// the fixture helpers below sit outside one. An unwrap in a fixture is an assertion.
#![allow(clippy::unwrap_used)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;

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

// ---------------------------------------------------------------------------------------
// 5. `--dry-run` prints the resolved plan and deletes nothing.
// ---------------------------------------------------------------------------------------

#[test]
fn a_dry_run_prints_the_plan_and_removes_nothing() {
    let (_tmp, base) = fixture();
    let modules = project(&base, "app");

    let run = run(&base, &["--dry-run"], "");

    assert!(run.ok, "{run:?}", run = (&run.stdout, &run.stderr));
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
// 6. The final confirmation defaults to no.
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
// 7. `--older-than` keeps what was touched recently.
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
// 8. Failures set a non-zero exit.
// ---------------------------------------------------------------------------------------

#[test]
fn a_root_that_cannot_be_read_exits_non_zero() {
    let (_tmp, base) = fixture();

    let run = run(&base.join("does-not-exist"), &["--dry-run"], "");

    assert!(!run.ok, "{}", run.stdout);
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
