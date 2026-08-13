//! What git knows about a directory: where its work tree is, and whether anything under it is
//! tracked.
//!
//! ## Why the index, and why by asking git for it
//!
//! Tier two's safety property is "contains no tracked file at any depth", which is exactly the
//! guarantee `git clean` enforces and the only reason the tier can be on by default. The index
//! is the authority for it, and getting the index wrong means deleting somebody's source — so
//! this module asks `git ls-files` rather than parsing `.git/index` itself. Split indexes,
//! version-4 path compression, sparse directory entries and linked work trees are all shapes a
//! hand-rolled reader gets wrong quietly, and quietly is the one failure mode a cleaner cannot
//! afford. One subprocess per work tree buys the exact answer.
//!
//! If git cannot be run, or the repository will not answer, tier two goes inert for that work
//! tree and says so. It never falls back to a guess.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::{fmt, io, str};

use unicode_normalization::UnicodeNormalization;

/// Environment variables that redirect git at a repository other than the one it was pointed
/// at, cleared before every invocation.
///
/// This is not hygiene, it is the safety property again. Anything running inside a git hook,
/// a `filter-branch` or a rebase has `GIT_DIR` and `GIT_INDEX_FILE` set, and they win over
/// `-C`: `GIT_INDEX_FILE=<another repo's index> git -C here ls-files` lists the *other*
/// repository's files and reports nothing about this one. Every path here would then look
/// untracked, and looking untracked is what makes a directory deletable. It fails silently and
/// in the dangerous direction, which is the combination that has to be designed out.
const AMBIENT_GIT_ENV: [&str; 8] = [
    "GIT_DIR",
    "GIT_INDEX_FILE",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CEILING_DIRECTORIES",
    "GIT_NAMESPACE",
];

/// A `git` invocation pointed at `dir`, with everything ambient that could change its answer
/// shut out.
///
/// Two families of variable, and they are dangerous for different reasons.
///
/// [`AMBIENT_GIT_ENV`] redirects git at another *repository*, which is the failure described
/// above it.
///
/// The locale redirects git's *prose*, and repo mode reads git's prose because `git clean` has
/// no `-z` and no porcelain format — it prints `Would remove <path>`, and that sentence is
/// translated. Measured, not assumed: under `LANGUAGE=de` the same command prints
/// `Würde … löschen`, and a parser looking for `Would remove` finds nothing at all. So a repo
/// full of build output would report as already clean. `LC_ALL=C` wins over `LANGUAGE`
/// (measured), and `LANGUAGE` is cleared as well because it is the one variable that otherwise
/// wins over `LANG`.
pub(crate) fn git(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).stdin(Stdio::null());
    for variable in AMBIENT_GIT_ENV {
        command.env_remove(variable);
    }
    command.env("LC_ALL", "C").env("LANGUAGE", "");
    command
}

/// Whether `dir` is the root of a git work tree.
///
/// A `.git` that is a *file* rather than a directory is a linked work tree or a submodule, and
/// is just as much a work tree root as the ordinary case.
#[must_use]
pub fn is_work_tree_root(dir: &Path) -> bool {
    dir.join(".git").symlink_metadata().is_ok()
}

/// What the `.git` at a work tree root actually is.
///
/// [`is_work_tree_root`] deliberately collapses all three, which is right for the safety model's
/// question — "is there a checkout here" — and wrong for the only question where the difference
/// decides whether a directory is disposable.
///
/// **A linked work tree is the one kind that holds no history of its own.** Its commits go to the
/// repository's object store and its branch is an ordinary ref there, so deleting the directory
/// costs the checked-out files and nothing else — verified rather than assumed: a commit made in
/// a linked work tree is still readable through its branch after the directory is removed
/// outright. A [`Repository`](Self::Repository) *is* the object store, and a
/// [`Submodule`](Self::Submodule) is a checkout the superproject's index points at, which is a
/// different promise from "somewhere to do some work".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checkout {
    /// `.git` is a directory: the repository itself, holding every object and ref under it.
    Repository,
    /// `.git` is a file pointing into another repository's `worktrees/`, which is where the
    /// objects and the branch actually live.
    Linked,
    /// `.git` is a file pointing into a superproject's `modules/`.
    Submodule,
}

/// Which kind of checkout is rooted at `dir`, or `None` if there is not one.
///
/// **Asked of git rather than read out of the `.git` file**, for this module's founding reason:
/// the file's `gitdir:` line has to be resolved relative to the work tree, can be absolute or
/// relative, and points somewhere whose *shape* is what distinguishes a linked work tree from a
/// submodule. A hand-rolled reader gets that wrong quietly, and quietly is the failure mode a
/// cleaner cannot afford — here it would mean reading a submodule as disposable.
///
/// The discriminator is git's own: `--git-dir` and `--git-common-dir` are equal for a repository
/// and for a submodule, and a linked work tree is the only case where the first sits at
/// `<common>/worktrees/<name>`. Measured against all three, rather than inferred from the
/// documentation.
///
/// Fails toward [`Repository`](Checkout::Repository) — never toward `Linked` — whenever git
/// cannot be run or will not answer. Everything downstream reads `Linked` as permission.
#[must_use]
pub fn checkout_at(dir: &Path) -> Option<Checkout> {
    if !is_work_tree_root(dir) {
        return None;
    }
    let Ok(output) = git(dir)
        .args(["rev-parse", "--path-format=absolute", "--git-dir"])
        .arg("--git-common-dir")
        .output()
    else {
        return Some(Checkout::Repository);
    };
    if !output.status.success() {
        return Some(Checkout::Repository);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let (Some(git_dir), Some(common)) = (lines.next(), lines.next()) else {
        return Some(Checkout::Repository);
    };
    let git_dir = Path::new(git_dir.trim());
    // `<common>/worktrees/<name>`, checked a component at a time rather than by string prefix, so
    // a repository that happens to live under a directory called `worktrees` cannot match.
    let linked = git_dir
        .parent()
        .is_some_and(|holder| holder.file_name() == Some(OsStr::new("worktrees")))
        && git_dir.parent().and_then(Path::parent) == Some(Path::new(common.trim()));
    Some(if linked {
        Checkout::Linked
    } else if git_dir
        .components()
        .any(|part| part.as_os_str() == OsStr::new("modules"))
    {
        Checkout::Submodule
    } else {
        Checkout::Repository
    })
}

/// Whether the work tree at `dir` holds work that exists nowhere else.
///
/// `git status --porcelain` is empty exactly when there is nothing uncommitted and nothing
/// untracked — and it says nothing about *ignored* files, which is what makes this usable here at
/// all: a work tree carrying 4 GiB of `node_modules` reads clean, because that is precisely the
/// content this program exists to regenerate rather than preserve.
///
/// Errs toward "not clean" on every failure. The answer is the gate on an irreversible removal.
#[must_use]
pub fn is_clean(dir: &Path) -> bool {
    git(dir)
        .args(["status", "--porcelain"])
        .output()
        .is_ok_and(|output| output.status.success() && output.stdout.is_empty())
}

/// Whether `HEAD` names a branch rather than sitting detached.
///
/// **The one way removing a linked work tree can lose a commit.** A commit made on a detached
/// `HEAD` is reachable only through that work tree's own `HEAD`, so once the directory is gone
/// and the administrative files are pruned nothing refers to it and `gc` will collect it —
/// measured, not reasoned about: `git fsck --unreachable` lists it immediately after. A commit on
/// a branch is reachable through an ordinary ref in the repository and survives.
///
/// Errs toward "detached" on every failure, which is the answer that keeps the directory.
#[must_use]
pub fn head_on_branch(dir: &Path) -> bool {
    git(dir)
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .output()
        .is_ok_and(|output| output.status.success())
}

/// The nearest ancestor of `from`, `from` itself included, that is a git work tree root.
///
/// This is git's own rule, which is what makes a checkout inside another checkout behave: the
/// inner repository is the authority for everything under it, and the outer one has no say.
#[must_use]
pub fn discover(from: &Path) -> Option<PathBuf> {
    let mut cursor = Some(from);
    while let Some(dir) = cursor {
        if is_work_tree_root(dir) {
            return Some(dir.to_path_buf());
        }
        cursor = dir.parent();
    }
    None
}

/// One git work tree, with the set of paths its index tracks.
#[derive(Debug)]
pub struct WorkTree {
    root: PathBuf,
    /// Every tracked path, relative to `root`, as raw bytes and sorted. Sorted so a "is
    /// anything under this directory tracked" question is a binary search rather than a scan
    /// of an index that can hold hundreds of thousands of entries.
    tracked: Vec<Box<[u8]>>,
}

impl WorkTree {
    /// Reads the index of the work tree rooted at `root`.
    ///
    /// # Errors
    ///
    /// If git cannot be run at all, or refuses to list the index.
    pub fn open(root: &Path) -> Result<Self, GitError> {
        let mut command = git(root);
        // `--full-name` pins the paths to the work tree root rather than to a working
        // directory, and `-z` gives them raw: git quotes and escapes any other way, and a
        // path this module misreads is a path it wrongly believes is untracked.
        command.args(["ls-files", "-z", "--full-name"]);

        let output = command
            .output()
            .map_err(|err| GitError::Run(root.to_path_buf(), err))?;
        if !output.status.success() {
            return Err(GitError::Refused(
                root.to_path_buf(),
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }

        // Sorted here rather than trusted to arrive sorted. The index is sorted and `ls-files`
        // walks it in order, but the binary search below is a safety property and it should
        // not rest on an implementation detail of another program.
        let mut tracked: Vec<Box<[u8]>> = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            // Composed here as well as on the query side, so the two agree. See `comparable`.
            .map(|path| Box::from(comparable(path.to_vec())))
            .collect();
        tracked.sort_unstable();
        tracked.dedup();

        Ok(Self {
            root: root.to_path_buf(),
            tracked,
        })
    }

    /// The work tree's root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// How many paths the index tracks.
    #[must_use]
    pub fn tracked(&self) -> usize {
        self.tracked.len()
    }

    /// Whether anything tracked lives at or below `dir`.
    ///
    /// Answers `true` for a path this work tree cannot express, because every caller is asking
    /// in order to decide whether deleting `dir` is safe, and "I could not tell" has to read as
    /// "do not touch it".
    #[must_use]
    pub fn holds_tracked_path(&self, dir: &Path) -> bool {
        let Ok(relative) = dir.strip_prefix(&self.root) else {
            return true;
        };
        let Some(mut prefix) = as_index_path(relative) else {
            return true;
        };
        if prefix.is_empty() {
            // The work tree root itself: anything at all is under it.
            return !self.tracked.is_empty();
        }
        // A gitlink — a submodule — is a tracked entry for the directory itself rather than for
        // anything beneath it, so the exact path has to be checked as well as the prefix.
        if self.tracked.binary_search(&prefix.clone().into()).is_ok() {
            return true;
        }
        prefix.push(b'/');
        let at = self
            .tracked
            .partition_point(|path| path.as_ref() < prefix.as_slice());
        self.tracked
            .get(at)
            .is_some_and(|path| path.starts_with(&prefix))
    }
}

/// A work-tree-relative path in the form git's index uses: `/`-separated, raw bytes.
///
/// Returns `None` for anything that is not a plain sequence of normal components, which cannot
/// be compared against index entries and which callers treat as "assume it is tracked".
fn as_index_path(relative: &Path) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for component in relative.components() {
        let Component::Normal(segment) = component else {
            return None;
        };
        if !out.is_empty() {
            out.push(b'/');
        }
        out.extend_from_slice(segment.as_encoded_bytes());
    }
    Some(comparable(out))
}

/// The form both sides of the tracked-path comparison have to be in.
///
/// The two sides disagree about Unicode normalization, and on macOS they disagree *by default*.
/// `readdir` on APFS hands back the bytes a name was created with, which for anything touched by
/// an HFS-era tool is decomposed; git sets `core.precomposeunicode` on macOS, so `git add`
/// composes the name before storing it. A directory called `café` is then `cafe\xcc\x81` on disk
/// and `caf\xc3\xa9` in the index — measured, not assumed — and a raw byte comparison misses.
///
/// That miss is the dangerous direction: a directory that *does* hold a tracked file looks
/// untracked, and looking untracked is what makes it eligible for deletion. It is reachable
/// without anyone doing anything exotic, since only one component of the path has to be
/// non-ASCII: `docs/café/build` matched by an ordinary `build/` ignore rule is enough.
///
/// So both sides are composed before they are compared. Where two names on the same filesystem
/// differ only by normalization — possible on Linux, which normalizes nothing — this conflates
/// them, and conflating errs toward "tracked", which is the side to err on. Anything that is not
/// UTF-8 cannot be normalized and is compared as it stands, which is correct: nothing converts
/// it on either side either.
fn comparable(path: Vec<u8>) -> Vec<u8> {
    if path.is_ascii() {
        return path;
    }
    match str::from_utf8(&path) {
        Ok(text) => text.nfc().collect::<String>().into_bytes(),
        Err(_) => path,
    }
}

/// Why a work tree could not be consulted.
#[derive(Debug)]
#[non_exhaustive]
pub enum GitError {
    /// git could not be run — most often because it is not installed.
    Run(PathBuf, io::Error),
    /// git ran and refused, carrying whatever it said about why.
    Refused(PathBuf, String),
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Run(root, err) => write!(
                f,
                "could not run git in {}, so nothing there can be judged safe to remove: {err}",
                root.display()
            ),
            Self::Refused(root, message) => write!(
                f,
                "git would not list the index of {}, so nothing there can be judged safe to \
                 remove: {message}",
                root.display()
            ),
        }
    }
}

impl std::error::Error for GitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Run(_, err) => Some(err),
            Self::Refused(..) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Checkout, WorkTree, as_index_path, checkout_at, comparable, git, head_on_branch, is_clean,
    };
    use std::path::{Path, PathBuf};

    /// A repository with one commit, made with git rather than by writing `.git` by hand — the
    /// shapes this module distinguishes are git's, and a fixture that spelled them itself would
    /// be asserting that the fixture agrees with the code.
    fn repo(at: &Path) {
        std::fs::create_dir_all(at).unwrap();
        run(at, &["init", "--quiet", "."]);
        run(at, &["config", "user.email", "test@example.com"]);
        run(at, &["config", "user.name", "test"]);
        std::fs::write(at.join("tracked.txt"), "content").unwrap();
        run(at, &["add", "."]);
        run(at, &["commit", "--quiet", "-m", "first"]);
    }

    /// `git worktree add --quiet`, which every fixture below needs and which does not fit on
    /// one line spelled out at each call.
    fn worktree(main: &Path, args: &[&str]) {
        let mut all = vec!["worktree", "add", "--quiet"];
        all.extend_from_slice(args);
        run(main, &all);
    }

    fn run(at: &Path, args: &[&str]) {
        let output = git(at).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?} in {}: {}",
            at.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn a_linked_work_tree_is_told_apart_from_the_repository_and_from_a_submodule() {
        // The distinction the whole feature rests on, and none of the three can be told apart by
        // whether `.git` is a file: a submodule has one too.
        let tmp = tempfile::TempDir::new().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let main = base.join("main");
        let inner = base.join("inner");
        repo(&main);
        repo(&inner);
        worktree(&main, &["../linked", "-b", "feature"]);
        run(
            &main,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "--quiet",
                "add",
                inner.to_str().unwrap(),
                "vendored",
            ],
        );
        run(&main, &["commit", "--quiet", "-m", "vendored"]);

        assert_eq!(checkout_at(&main), Some(Checkout::Repository));
        assert_eq!(checkout_at(&base.join("linked")), Some(Checkout::Linked));
        assert_eq!(
            checkout_at(&main.join("vendored")),
            Some(Checkout::Submodule),
            "a submodule was read as a disposable work tree"
        );
        // Not a checkout at all, which is every other directory on the disk.
        assert_eq!(checkout_at(&base), None);
    }

    #[test]
    fn a_work_tree_is_clean_despite_ignored_build_output_and_dirty_with_anything_else() {
        // The property that makes this usable: the directories pristine exists to reclaim are
        // exactly the ones that must not count as work.
        let tmp = tempfile::TempDir::new().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let main = base.join("main");
        repo(&main);
        std::fs::write(main.join(".gitignore"), "node_modules/\n").unwrap();
        run(&main, &["add", ".gitignore"]);
        run(&main, &["commit", "--quiet", "-m", "ignore"]);

        std::fs::create_dir_all(main.join("node_modules/dep")).unwrap();
        std::fs::write(main.join("node_modules/dep/index.js"), "x").unwrap();
        assert!(
            is_clean(&main),
            "4 GiB of node_modules must not read as work that exists nowhere else"
        );

        // An untracked file that nothing ignores is work, and so is an edit to a tracked one.
        std::fs::write(main.join("notes.md"), "only copy").unwrap();
        assert!(!is_clean(&main));
        std::fs::remove_file(main.join("notes.md")).unwrap();
        assert!(is_clean(&main));
        std::fs::write(main.join("tracked.txt"), "edited").unwrap();
        assert!(!is_clean(&main));
    }

    #[test]
    fn a_detached_head_is_refused_because_its_commits_are_reachable_from_nothing_else() {
        // Measured in #656's follow-up: a commit made on a detached HEAD in a linked work tree is
        // listed by `git fsck --unreachable` the moment the directory is removed and pruned. On a
        // branch it survives, because the branch is an ordinary ref in the repository.
        let tmp = tempfile::TempDir::new().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let main = base.join("main");
        repo(&main);
        worktree(&main, &["../onbranch", "-b", "feature"]);
        worktree(&main, &["--detach", "../loose"]);

        assert!(head_on_branch(&base.join("onbranch")));
        assert!(!head_on_branch(&base.join("loose")));
    }

    #[test]
    fn every_answer_that_decides_a_deletion_fails_toward_keeping_the_directory() {
        // A directory git will not speak for at all. `is_clean` and `head_on_branch` gate an
        // irreversible removal, so silence has to read as "do not touch it" — the same discipline
        // the tier-two fallback keeps when a work tree will not answer.
        let tmp = tempfile::TempDir::new().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        assert!(!is_clean(&base), "a directory git disowns read as clean");
        assert!(!head_on_branch(&base));
        // And a checkout it cannot classify is the kind nothing is allowed to remove.
        assert_eq!(checkout_at(&base), None);
    }

    /// Mirrors what [`WorkTree::open`] does to `git ls-files` output, so a fixture and a real
    /// index are the same shape.
    fn work_tree(root: &str, tracked: &[&str]) -> WorkTree {
        let mut tracked: Vec<Box<[u8]>> = tracked
            .iter()
            .map(|path| Box::from(comparable(path.as_bytes().to_vec())))
            .collect();
        tracked.sort_unstable();
        WorkTree {
            root: PathBuf::from(root),
            tracked,
        }
    }

    #[test]
    fn a_tracked_file_at_any_depth_bars_the_directory_above_it() {
        let tree = work_tree("/repo", &["out/deep/deeper/keep.txt", "src/main.rs"]);
        assert!(tree.holds_tracked_path(Path::new("/repo/out")));
        assert!(tree.holds_tracked_path(Path::new("/repo/out/deep")));
        assert!(!tree.holds_tracked_path(Path::new("/repo/out/other")));
    }

    #[test]
    fn a_name_that_merely_starts_the_same_is_not_a_match() {
        // `-`, `.` and `/` are 0x2d, 0x2e and 0x2f, so these three sort either side of the
        // `out/` prefix and a sloppy comparison picks up the wrong ones.
        let tree = work_tree("/repo", &["out-takes/a.txt", "out.txt", "outer/b.txt"]);
        assert!(!tree.holds_tracked_path(Path::new("/repo/out")));
        assert!(tree.holds_tracked_path(Path::new("/repo/out-takes")));
        assert!(tree.holds_tracked_path(Path::new("/repo/outer")));
    }

    #[test]
    fn a_gitlink_bars_the_directory_it_names() {
        // A submodule is one index entry for the directory itself, with nothing under it.
        let tree = work_tree("/repo", &["vendor/sub"]);
        assert!(tree.holds_tracked_path(Path::new("/repo/vendor/sub")));
        assert!(tree.holds_tracked_path(Path::new("/repo/vendor")));
    }

    #[test]
    fn a_path_outside_the_work_tree_is_assumed_tracked() {
        let tree = work_tree("/repo", &["src/main.rs"]);
        assert!(tree.holds_tracked_path(Path::new("/elsewhere/out")));
    }

    #[test]
    fn an_empty_index_holds_nothing() {
        let tree = work_tree("/repo", &[]);
        assert!(!tree.holds_tracked_path(Path::new("/repo")));
        assert!(!tree.holds_tracked_path(Path::new("/repo/out")));
    }

    #[test]
    fn a_decomposed_path_matches_the_composed_one_git_stored() {
        // `café` as git records it on macOS against `café` as `readdir` hands it back. Without
        // composing both sides these are different byte strings, the search misses, and a
        // directory holding a tracked file is reported as free to delete.
        let tree = work_tree("/repo", &["caf\u{e9}/build/keep.txt"]);
        assert!(tree.holds_tracked_path(Path::new("/repo/cafe\u{301}/build")));
        assert!(tree.holds_tracked_path(Path::new("/repo/caf\u{e9}/build")));
        assert!(!tree.holds_tracked_path(Path::new("/repo/cafe\u{301}/other")));
    }

    #[test]
    fn index_paths_are_slash_separated_and_reject_anything_exotic() {
        assert_eq!(
            as_index_path(Path::new("a/b/c")).unwrap(),
            b"a/b/c".to_vec()
        );
        assert_eq!(as_index_path(Path::new("")).unwrap(), Vec::<u8>::new());
        assert!(as_index_path(Path::new("../a")).is_none());
    }
}
