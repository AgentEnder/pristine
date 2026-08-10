//! What repo mode promises, run as the binary against a real git.
//!
//! Almost none of this can be proved against a mock. The whole design decision behind the mode
//! is "let git decide what is removable", so a fixture that answers the way we *expect* git to
//! answer proves nothing at all — it proves our expectation. Every test here builds a real work
//! tree and reads what git actually says about it.
//!
//! The load-bearing one is [`removing_untracked_files_does_not_take_the_ignored_cache_beside_them`].
//! It is the regression test for the bug that cost real data in the Node predecessor, and it is
//! the entire reason enumeration goes through `git clean -n` rather than
//! `git ls-files --others --directory`.

// `allow-unwrap-in-tests` in clippy.toml only reaches code inside a `#[test]` function, and the
// fixture helpers below sit outside one. An unwrap in a fixture is an assertion.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;

/// Runs git in `dir` with the developer's machine shut out, and returns its stdout.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "pristine")
        .env("GIT_AUTHOR_EMAIL", "pristine@example.invalid")
        .env("GIT_COMMITTER_NAME", "pristine")
        .env("GIT_COMMITTER_EMAIL", "pristine@example.invalid")
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// A work tree with one tracked file and one commit, resolved so a test can compare paths
/// against what a plan holds.
fn checkout() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    git(&root, &["init", "--quiet"]);
    write(&root.join("tracked.txt"), "the original\n");
    git(&root, &["add", "tracked.txt"]);
    git(&root, &["commit", "--quiet", "-m", "first"]);
    (tmp, root)
}

struct Run {
    stdout: String,
    stderr: String,
    ok: bool,
}

/// Runs `pristine repo <root> <args>` with `answer` on its standard input.
fn run(root: &Path, args: &[&str], answer: &str) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pristine"))
        .arg("repo")
        .arg(root)
        .args(args)
        // Repo mode reads git's prose. A developer running the suite under a translated
        // locale must not get a different answer from CI, and the binary forcing `LC_ALL=C`
        // for its own invocations is what makes that true — so the test hands it the hostile
        // environment rather than a clean one.
        .env("LANGUAGE", "de")
        .env("LC_ALL", "de_DE.UTF-8")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
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

/// Runs it and asserts it exited cleanly.
fn succeeds(root: &Path, args: &[&str], answer: &str) -> String {
    let run = run(root, args, answer);
    assert!(
        run.ok,
        "pristine repo {args:?} failed:\n{}\n{}",
        run.stdout, run.stderr
    );
    run.stdout
}

// ------------------------------------------------------------------------------------------
// The correction that cost real data: untracked and ignored are two independent choices, and
// taking one must not take the other.
// ------------------------------------------------------------------------------------------

/// Builds the exact shape the bug was found on: a directory holding an ignored cache beside
/// untracked data, with nothing tracked anywhere inside it.
fn mixed_directory(root: &Path) {
    write(&root.join(".gitignore"), ".nx/cache/\n");
    git(root, &["add", ".gitignore"]);
    git(root, &["commit", "--quiet", "-m", "ignore the cache"]);
    write(&root.join(".nx/cache/hash.bin"), "expensive to rebuild\n");
    write(&root.join(".nx/workspace-data/state.json"), "{}\n");
}

#[test]
fn removing_untracked_files_does_not_take_the_ignored_cache_beside_them() {
    let (_tmp, root) = checkout();
    mixed_directory(&root);

    let printed = succeeds(&root, &["--untracked", "--yes"], "");

    assert!(
        !root.join(".nx/workspace-data").exists(),
        "the untracked half survived:\n{printed}"
    );
    // The bug. `git ls-files --others --directory` collapses `.nx/` to one entry because
    // nothing in it is TRACKED, so removing untracked files also wiped the ignored cache the
    // user had chosen to keep. `git clean` collapses only when everything inside is going.
    assert!(
        root.join(".nx/cache/hash.bin").exists(),
        "removing untracked files took the ignored cache with them:\n{printed}"
    );
}

#[test]
fn removing_ignored_files_does_not_take_the_untracked_data_beside_them() {
    let (_tmp, root) = checkout();
    mixed_directory(&root);

    let printed = succeeds(&root, &["--ignored", "--yes"], "");

    assert!(!root.join(".nx/cache").exists(), "{printed}");
    assert!(
        root.join(".nx/workspace-data/state.json").exists(),
        "removing ignored files took the untracked data with them:\n{printed}"
    );
}

#[test]
fn a_tracked_file_is_never_removed_whatever_was_asked_for() {
    let (_tmp, root) = checkout();
    write(&root.join(".gitignore"), "dist/\n");
    write(&root.join("dist/bundle.js"), "built\n");
    write(&root.join("scratch.txt"), "untracked\n");

    succeeds(
        &root,
        &[
            "--untracked",
            "--ignored",
            "--node-modules",
            "--env",
            "--yes",
        ],
        "",
    );

    assert!(root.join("tracked.txt").exists(), "a tracked file went");
    assert!(!root.join("dist").exists());
    assert!(!root.join("scratch.txt").exists());
}

// ------------------------------------------------------------------------------------------
// Vendor and env are excluded from a list the user did ask for.
// ------------------------------------------------------------------------------------------

/// A checkout whose ignored set holds one ordinary artefact, one vendor directory and one env
/// file, plus an untracked env file that no ignore rule hides.
fn sediment(root: &Path) {
    write(
        &root.join(".gitignore"),
        "dist/\nnode_modules/\n.env.local\n",
    );
    git(root, &["add", ".gitignore"]);
    git(root, &["commit", "--quiet", "-m", "ignore the usual"]);
    write(&root.join("dist/bundle.js"), "built\n");
    write(&root.join("node_modules/left-pad/index.js"), "module\n");
    write(&root.join(".env.local"), "SECRET=1\n");
    write(&root.join(".env"), "SECRET=2\n");
}

#[test]
fn vendor_and_env_survive_a_run_that_did_not_ask_for_them() {
    let (_tmp, root) = checkout();
    sediment(&root);

    let printed = succeeds(&root, &["--untracked", "--ignored", "--yes"], "");

    assert!(!root.join("dist").exists(), "{printed}");
    assert!(
        root.join("node_modules/left-pad/index.js").exists(),
        "{printed}"
    );
    assert!(root.join(".env.local").exists(), "{printed}");
    // The untracked one too. The guard is about what the file is worth, not about which of
    // git's two lists it arrived in, and an env file git is not even hiding is the most
    // precious kind rather than the least.
    assert!(root.join(".env").exists(), "{printed}");
    // A count with no way to act on it is a puzzle rather than a report.
    assert!(printed.contains("--node-modules"), "{printed}");
    assert!(printed.contains("--env"), "{printed}");
}

#[test]
fn opting_in_takes_them() {
    let (_tmp, root) = checkout();
    sediment(&root);

    let printed = succeeds(
        &root,
        &[
            "--untracked",
            "--ignored",
            "--node-modules",
            "--env",
            "--yes",
        ],
        "",
    );

    assert!(!root.join("node_modules").exists(), "{printed}");
    assert!(!root.join(".env.local").exists(), "{printed}");
    assert!(!root.join(".env").exists(), "{printed}");
    assert!(root.join("tracked.txt").exists(), "{printed}");
}

// ------------------------------------------------------------------------------------------
// `--yes` gates the confirmation and nothing else.
// ------------------------------------------------------------------------------------------

#[test]
fn an_action_flag_without_yes_still_refuses_to_delete() {
    let (_tmp, root) = checkout();
    sediment(&root);

    // The whole point of the split: a script that selected something has not thereby consented
    // to it. With nothing on standard input the confirmation reads as no.
    let run = run(&root, &["--ignored"], "");

    assert!(run.ok, "{}", run.stderr);
    assert!(root.join("dist/bundle.js").exists(), "{}", run.stdout);
    assert!(run.stdout.contains("[y/N]"), "{}", run.stdout);
    assert!(run.stdout.contains("nothing was"), "{}", run.stdout);
}

#[test]
fn a_bare_enter_at_the_confirmation_removes_nothing() {
    let (_tmp, root) = checkout();
    sediment(&root);

    let run = run(&root, &["--ignored"], "\n");

    assert!(run.ok, "{}", run.stderr);
    assert!(root.join("dist/bundle.js").exists(), "enter was consent");
}

#[test]
fn saying_yes_at_the_confirmation_removes_what_the_plan_listed() {
    let (_tmp, root) = checkout();
    sediment(&root);

    let run = run(&root, &["--ignored"], "y\n");

    assert!(run.ok, "{}", run.stderr);
    assert!(!root.join("dist").exists(), "{}", run.stdout);
}

#[test]
fn yes_on_its_own_selects_nothing() {
    let (_tmp, root) = checkout();
    sediment(&root);

    // `--yes` is consent, not an instruction. It also makes the run non-interactive, so this
    // is the shape that would hang if the rule were the other way round.
    let printed = succeeds(&root, &["--yes"], "");

    assert!(root.join("dist/bundle.js").exists(), "{printed}");
    assert!(printed.contains("nothing selected"), "{printed}");
}

#[test]
fn yes_does_not_ask_what_to_do_however_eagerly_the_input_answers() {
    let (_tmp, root) = checkout();
    sediment(&root);
    write(&root.join("tracked.txt"), "changed\n");

    // Closed input proves nothing here, because every prompt defaults to no anyway. The
    // dangerous shape is an input that says yes to everything: if `--yes` let the cascade run,
    // it would select reset AND both lists and then skip the final confirmation it is supposed
    // to be the answer TO — turning "I consent to what I asked for" into "I consent to
    // whatever I am about to be asked".
    let printed = succeeds(&root, &["--yes"], "3\ny\ny\ny\ny\ny\n");

    assert!(
        !printed.contains("[y/N]"),
        "--yes reached a prompt:\n{printed}"
    );
    assert!(!printed.contains("Reset changed"), "{printed}");
    assert!(printed.contains("nothing selected"), "{printed}");
    assert!(root.join("dist/bundle.js").exists(), "{printed}");
    assert!(root.join("node_modules").exists(), "{printed}");
    assert_eq!(
        fs::read_to_string(root.join("tracked.txt")).unwrap(),
        "changed\n",
        "--yes reset a work tree nobody asked it to"
    );
}

// ------------------------------------------------------------------------------------------
// The dry run.
// ------------------------------------------------------------------------------------------

#[test]
fn a_dry_run_prints_the_plan_and_touches_nothing() {
    let (_tmp, root) = checkout();
    sediment(&root);
    write(&root.join("tracked.txt"), "changed\n");

    let printed = succeeds(
        &root,
        &[
            "--reset=hard",
            "--untracked",
            "--ignored",
            "--dry-run",
            "--yes",
        ],
        "",
    );

    assert!(root.join("dist/bundle.js").exists(), "{printed}");
    assert_eq!(
        fs::read_to_string(root.join("tracked.txt")).unwrap(),
        "changed\n",
        "a dry run reset the work tree:\n{printed}"
    );
    // The resolved plan, relative to the work tree root, plus what it would have reset.
    assert!(
        printed.lines().any(|line| line.ends_with("  dist")),
        "{printed}"
    );
    assert!(printed.contains("git reset --hard HEAD"), "{printed}");
    assert!(!printed.contains(root.to_str().unwrap()), "{printed}");
    assert!(printed.contains("dry run"), "{printed}");
}

// ------------------------------------------------------------------------------------------
// The reset verbs, and the one ordering that has a consequence.
// ------------------------------------------------------------------------------------------

#[test]
fn reset_worktree_discards_the_working_copy_and_leaves_the_index() {
    let (_tmp, root) = checkout();
    write(&root.join("staged.txt"), "staged\n");
    git(&root, &["add", "staged.txt"]);
    write(&root.join("tracked.txt"), "changed\n");

    succeeds(&root, &["--reset=worktree", "--yes"], "");

    assert_eq!(
        fs::read_to_string(root.join("tracked.txt")).unwrap(),
        "the original\n",
        "`git restore -- .` did not restore the working tree"
    );
    // `git restore -- .` is the working tree only, which is exactly what distinguishes it
    // from `hard`: a staged addition survives it.
    assert!(
        git(&root, &["diff", "--cached", "--name-only"]).contains("staged.txt"),
        "the index went with the working tree"
    );
}

#[test]
fn reset_hard_discards_the_index_too() {
    let (_tmp, root) = checkout();
    write(&root.join("staged.txt"), "staged\n");
    git(&root, &["add", "staged.txt"]);
    write(&root.join("tracked.txt"), "changed\n");

    succeeds(&root, &["--reset=hard", "--yes"], "");

    assert_eq!(
        fs::read_to_string(root.join("tracked.txt")).unwrap(),
        "the original\n"
    );
    assert!(
        git(&root, &["diff", "--cached", "--name-only"])
            .trim()
            .is_empty(),
        "a hard reset left the index alone"
    );
    // And the file itself is gone, which is the sharpest difference between the two verbs and
    // worth pinning: `git reset --hard HEAD` checks the working tree back out to match HEAD,
    // and a file that is in the index but not in HEAD is deleted rather than merely unstaged.
    // `git clean` never offered it — it was tracked when the plan was built — so this is the
    // reset's doing, and it is why `hard` is the answer a user has to reach for deliberately.
    assert!(
        !root.join("staged.txt").exists(),
        "a hard reset kept a file that is not in HEAD"
    );
}

#[test]
fn a_bare_reset_is_a_hard_one() {
    let (_tmp, root) = checkout();
    write(&root.join("staged.txt"), "staged\n");
    git(&root, &["add", "staged.txt"]);

    succeeds(&root, &["--reset", "--yes"], "");

    assert!(
        git(&root, &["diff", "--cached", "--name-only"])
            .trim()
            .is_empty(),
        "a bare --reset was not a hard one"
    );
}

#[test]
fn a_reset_that_makes_a_planned_target_tracked_does_not_delete_it() {
    let (_tmp, root) = checkout();
    // The stale-index window, and the reason the plan cannot outlive the reset. `git rm
    // --cached` takes the file out of the INDEX and leaves it on disk, so `git clean -n -d`
    // reports it as untracked and it lands on the plan — and then `git reset --hard HEAD` puts
    // it back in the index, making it a tracked file. A plan built before the reset and
    // executed after it deletes a file that is, by the time of the unlink, tracked and
    // committed.
    git(&root, &["rm", "--cached", "--quiet", "tracked.txt"]);
    write(&root.join("scratch.txt"), "untracked\n");
    assert!(
        git(&root, &["clean", "-n", "-d"]).contains("tracked.txt"),
        "the fixture did not reach the state the bug needs"
    );

    let printed = succeeds(&root, &["--reset=hard", "--untracked", "--yes"], "");

    assert!(
        root.join("tracked.txt").exists(),
        "a file the reset made tracked was deleted by a plan built before it:\n{printed}"
    );
    assert_eq!(
        fs::read_to_string(root.join("tracked.txt")).unwrap(),
        "the original\n"
    );
    // The rest of the run still happens: this is a narrowing, not an abort.
    assert!(!root.join("scratch.txt").exists(), "{printed}");
}

#[test]
fn a_reset_that_makes_git_collapse_a_directory_does_not_widen_the_plan() {
    let (_tmp, root) = checkout();
    // The other half of the same window, and the destructive one. `dir/` holds three things:
    // an untracked file, a file staged but never committed, and an env file that repo mode
    // holds back by default.
    write(&root.join("dir/a.txt"), "untracked\n");
    write(&root.join("dir/.env"), "SECRET=1\n");
    write(&root.join("dir/staged.txt"), "staged\n");
    git(&root, &["add", "dir/staged.txt"]);

    // Before the reset git will not collapse `dir/`, because `staged.txt` is tracked — so the
    // plan names `dir/a.txt`, and `dir/.env` is excluded from it.
    let before = git(&root, &["clean", "-n", "-d"]);
    assert!(before.contains("dir/a.txt"), "{before}");
    assert!(!before.contains("Would remove dir/\n"), "{before}");

    let printed = succeeds(&root, &["--reset=hard", "--untracked", "--yes"], "");

    // The reset deletes `staged.txt`, and git then collapses `dir/` — so a re-enumeration that
    // was merely trusted would remove the whole directory, taking with it the env file the
    // user was told had been held back and never saw on any plan.
    assert!(
        root.join("dir/.env").exists(),
        "the reset widened the plan onto a file that was deliberately excluded:\n{printed}"
    );
    assert!(
        printed.contains("withdrawn after the reset"),
        "the widening was not reported:\n{printed}"
    );
    // Nothing was staged, so the reset removed it.
    assert!(!root.join("dir/staged.txt").exists(), "{printed}");
}

#[test]
fn the_reset_happens_before_anything_is_removed() {
    let (_tmp, root) = checkout();
    // A tracked file deleted from the working tree, and an untracked file beside it. The reset
    // puts the tracked file back; the removal takes the untracked one. Run in the other order
    // the restored file would be a fresh untracked file the plan had already listed — which is
    // the only way these two steps can interfere, and the reason the order is stated at all.
    fs::remove_file(root.join("tracked.txt")).unwrap();
    write(&root.join("scratch.txt"), "untracked\n");

    succeeds(&root, &["--reset=hard", "--untracked", "--yes"], "");

    assert!(
        root.join("tracked.txt").exists(),
        "the reset did not restore the tracked file, or the removal took it back off"
    );
    assert!(!root.join("scratch.txt").exists());
}

// ------------------------------------------------------------------------------------------
// What git will not clean, and what is not a work tree at all.
// ------------------------------------------------------------------------------------------

#[test]
fn a_nested_checkout_is_left_alone_and_named() {
    let (_tmp, root) = checkout();
    write(&root.join(".gitignore"), "sandboxes/\n");
    git(&root, &["add", ".gitignore"]);
    git(&root, &["commit", "--quiet", "-m", "ignore the sandboxes"]);
    let inner = root.join("sandboxes/work");
    fs::create_dir_all(&inner).unwrap();
    git(&inner, &["init", "--quiet"]);
    write(&inner.join("uncommitted.txt"), "exists nowhere else\n");

    let printed = succeeds(&root, &["--ignored", "--yes"], "");

    assert!(
        inner.join("uncommitted.txt").exists(),
        "a nested checkout was removed:\n{printed}"
    );
    // Reported rather than silently absent. A user who does not see it named reads this run as
    // having covered everything.
    assert!(printed.contains("nested repositor"), "{printed}");
    assert!(printed.contains("sandboxes/work"), "{printed}");
}

#[test]
fn outside_a_work_tree_it_says_so_and_exits_non_zero() {
    let tmp = TempDir::new().unwrap();

    let run = run(tmp.path(), &["--ignored", "--yes"], "");

    assert!(!run.ok, "a directory that is not a checkout succeeded");
    assert!(run.stderr.contains("git work tree"), "{}", run.stderr);
}

#[test]
fn a_path_inside_the_checkout_cleans_the_whole_checkout() {
    let (_tmp, root) = checkout();
    write(&root.join(".gitignore"), "dist/\n");
    git(&root, &["add", ".gitignore"]);
    git(&root, &["commit", "--quiet", "-m", "ignore dist"]);
    write(&root.join("dist/a.js"), "built\n");
    write(&root.join("packages/web/dist/b.js"), "built\n");

    // Pointed at a subdirectory. `git clean` scoped there would clean only that subtree while
    // `git reset --hard` resets everything regardless, so the mode takes the checkout instead
    // of letting one run mean two different things by "here".
    let printed = succeeds(&root.join("packages/web"), &["--ignored", "--yes"], "");

    assert!(!root.join("dist").exists(), "{printed}");
    assert!(!root.join("packages/web/dist").exists(), "{printed}");
}

// ------------------------------------------------------------------------------------------
// The interactive cascade, driven through the real binary.
// ------------------------------------------------------------------------------------------

#[test]
fn a_run_with_no_flags_and_no_input_does_nothing() {
    let (_tmp, root) = checkout();
    sediment(&root);
    write(&root.join("tracked.txt"), "changed\n");

    // The CI shape that must not hang and must not delete. Every question defaults to the
    // answer that changes nothing, and end of input is every question at once.
    let printed = succeeds(&root, &[], "");

    assert!(root.join("dist/bundle.js").exists(), "{printed}");
    assert_eq!(
        fs::read_to_string(root.join("tracked.txt")).unwrap(),
        "changed\n"
    );
    assert!(
        printed.contains("Reset changed (tracked) files?"),
        "{printed}"
    );
}

#[test]
fn the_cascade_reaches_the_deleter_when_it_is_answered() {
    let (_tmp, root) = checkout();
    sediment(&root);

    // reset: no. untracked: no. ignored: yes. vendor: no. env: no. proceed: yes.
    let printed = succeeds(&root, &[], "1\nn\ny\nn\nn\ny\n");

    assert!(!root.join("dist").exists(), "{printed}");
    assert!(
        root.join("node_modules").exists(),
        "vendor was not held back"
    );
    assert!(root.join(".env.local").exists(), "env was not held back");
    assert!(root.join(".env").exists(), "{printed}");
}
