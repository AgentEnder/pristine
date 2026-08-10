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

use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::{fmt, io};

/// Whether `dir` is the root of a git work tree.
///
/// A `.git` that is a *file* rather than a directory is a linked work tree or a submodule, and
/// is just as much a work tree root as the ordinary case.
#[must_use]
pub fn is_work_tree_root(dir: &Path) -> bool {
    dir.join(".git").symlink_metadata().is_ok()
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
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            // `--full-name` pins the paths to the work tree root rather than to a working
            // directory, and `-z` gives them raw: git quotes and escapes any other way, and a
            // path this module misreads is a path it wrongly believes is untracked.
            .args(["ls-files", "-z", "--full-name"])
            .stdin(Stdio::null())
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
            .map(Box::from)
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
    Some(out)
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
    use super::{WorkTree, as_index_path};
    use std::path::{Path, PathBuf};

    fn work_tree(root: &str, tracked: &[&str]) -> WorkTree {
        let mut tracked: Vec<Box<[u8]>> = tracked
            .iter()
            .map(|path| Box::from(path.as_bytes()))
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
    fn index_paths_are_slash_separated_and_reject_anything_exotic() {
        assert_eq!(
            as_index_path(Path::new("a/b/c")).unwrap(),
            b"a/b/c".to_vec()
        );
        assert_eq!(as_index_path(Path::new("")).unwrap(), Vec::<u8>::new());
        assert!(as_index_path(Path::new("../a")).is_none());
    }
}
