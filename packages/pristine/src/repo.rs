//! Repo mode: one checkout, cleaned the way `git clean -fdx` cleans it.
//!
//! This is the port of the unpublished Node `@agentender/pristine`, and the whole of its good
//! idea is that **it enumerates nothing itself**. Point it at a work tree and it asks git what
//! it would remove, then removes that. What comes free is every part of gitignore that a
//! hand-rolled matcher gets subtly wrong: nested `.gitignore` files, negations,
//! `info/exclude`, the user's global excludes, and the refusal to descend into a nested
//! repository.
//!
//! ## Why `git clean -n` and not `git ls-files --directory`
//!
//! The original design enumerated with `git ls-files --others --directory`, and dogfooding it
//! cost real data. `--directory` collapses a directory to `dir/` based only on the absence of
//! **tracked** files, so `.nx/` — an ignored cache beside untracked data, with nothing tracked
//! in it — collapsed to one entry `.nx/`. Removing *untracked* files then also wiped the
//! ignored cache the user had chosen to keep.
//!
//! `git clean` collapses a directory only when everything inside it is being removed, and
//! descends otherwise. Measured on that exact shape: `git clean -n -d` prints
//! `.nx/workspace-data/` and `git clean -n -d -X` prints `.nx/cache/`. The two lists are
//! disjoint by construction, which is why both can be offered as independent choices.
//!
//! ## Reading git's prose, which is the one uncomfortable part
//!
//! `git clean` has no `-z` and no porcelain format. It prints `Would remove <path>` and
//! `Would skip repository <path>`, so this module parses sentences, and two things follow.
//!
//! The sentences are translated, which [`crate::git::git`] handles by forcing the C locale for
//! every invocation. And the paths are quoted — git's own C-style escaping — which
//! [`unquote`] undoes. A line matching neither sentence is an **error**, never a target and
//! never silence: not understanding git's output is exactly the state in which nothing may be
//! deleted.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::{fmt, io};

use crate::delete::Target;
use crate::git::git;

/// What to do about tracked files that have been changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reset {
    /// Discard changes to tracked files in the working tree, leaving the index alone:
    /// `git restore -- .`.
    WorkTree,
    /// Discard everything, index included: `git reset --hard HEAD`.
    Hard,
}

impl Reset {
    /// What git is asked to do, as it would be typed.
    #[must_use]
    pub fn command(self) -> &'static str {
        match self {
            Self::WorkTree => "git restore -- .",
            Self::Hard => "git reset --hard HEAD",
        }
    }

    fn args(self) -> &'static [&'static str] {
        match self {
            Self::WorkTree => &["restore", "--", "."],
            Self::Hard => &["reset", "--hard", "HEAD"],
        }
    }
}

impl fmt::Display for Reset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkTree => write!(f, "discard working-tree changes"),
            Self::Hard => write!(f, "discard everything (hard reset)"),
        }
    }
}

/// What a run was asked to do, however it was asked.
///
/// The defaults are the safe ones and they are the same whether they came from flags or from
/// prompts: nothing is reset, nothing is removed, and vendor and env are excluded even from a
/// list the user did ask for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "these are four independent yes/no answers, from four flags or four prompts. The \
              lint's advice — a state machine — would put a shape between the question and the \
              answer that neither of them has"
)]
pub struct Selection {
    /// Whether to reset tracked changes, and how far.
    pub reset: Option<Reset>,
    /// Whether to remove untracked files.
    pub untracked: bool,
    /// Whether to remove ignored files.
    pub ignored: bool,
    /// Whether vendored dependency directories are in scope. Off by default: `node_modules`
    /// is the most expensive thing on the list to get back.
    pub vendor: bool,
    /// Whether env files are in scope. Off by default: a `.env` is usually the only copy of
    /// what is in it, and no command regenerates it.
    pub env: bool,
}

impl Selection {
    /// Whether this selection asks for anything at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.reset.is_none() && !self.untracked && !self.ignored
    }
}

/// Which of the three classes an entry falls in.
///
/// The partition is over the entries git returned, and it never re-walks the tree: a
/// collapsed `node_modules/` is one entry and one decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// A vendored dependency directory. Cheap to get back in the sense that a command does it,
    /// expensive in the sense that the command takes minutes and a network.
    Vendor,
    /// An env file. Nothing regenerates one.
    Env,
    /// Build output, caches, everything else.
    Other,
}

/// Which class `entry` falls in, judged only from its path.
///
/// `vendor` is any path with a `node_modules` component rather than only an entry that *is*
/// one. The broader reading matters because git hands back whatever it did not collapse: a
/// `node_modules` holding one tracked file arrives as its individual children, and each of
/// those is still a vendored file. Being broad here can only ever keep more, which is the
/// direction the default already leans.
///
/// `env` is the design's `*.env*` against the final component, which is `contains(".env")`.
/// It catches `.env`, `.env.local` and `prod.env`, and does not catch `environment`.
#[must_use]
pub fn classify(entry: &Path) -> Class {
    if entry
        .components()
        .any(|component| component.as_os_str() == "node_modules")
    {
        return Class::Vendor;
    }
    let name = entry.file_name().unwrap_or_default().to_string_lossy();
    if name.contains(".env") {
        return Class::Env;
    }
    Class::Other
}

/// What git says it would remove from one work tree.
///
/// The two lists are disjoint: `-d` is untracked-and-not-ignored, `-dX` is ignored only.
#[derive(Debug, Clone, Default)]
pub struct Enumeration {
    /// Untracked paths, absolute. A directory here means everything under it.
    pub untracked: Vec<PathBuf>,
    /// Ignored paths, absolute.
    pub ignored: Vec<PathBuf>,
    /// Nested repositories git refused to clean, absolute. Reported rather than dropped: a
    /// checkout parked inside a work tree usually holds work that exists nowhere else, and a
    /// user who does not see it named will read this run as having covered everything.
    pub skipped: Vec<PathBuf>,
}

/// The targets a [`Selection`] picks out of an [`Enumeration`], and what it left behind.
#[derive(Debug, Clone, Default)]
pub struct Selected {
    /// What to remove, untracked before ignored.
    pub targets: Vec<Target>,
    /// How many entries were left alone because vendor was not opted in.
    pub vendor: usize,
    /// How many entries were left alone because env was not opted in.
    pub env: usize,
}

/// Applies a selection to an enumeration.
///
/// The vendor and env filters apply to **both** lists, not only to the ignored one. The guard
/// is about what the file is worth, and an untracked-but-not-ignored `.env` is the most
/// precious kind rather than the least: it is the one git is not even hiding.
#[must_use]
pub fn select(enumeration: &Enumeration, selection: &Selection) -> Selected {
    let mut selected = Selected::default();
    let untracked = selection.untracked.then_some(&enumeration.untracked);
    let ignored = selection.ignored.then_some(&enumeration.ignored);
    for path in untracked.into_iter().chain(ignored).flatten() {
        match classify(path) {
            Class::Vendor if !selection.vendor => selected.vendor += 1,
            Class::Env if !selection.env => selected.env += 1,
            _ => selected.targets.push(Target::at(path.clone())),
        }
    }
    selected
}

/// One git work tree, asked what it would clean.
#[derive(Debug, Clone)]
pub struct Repo {
    root: PathBuf,
}

impl Repo {
    /// Finds the work tree containing `from` and returns it.
    ///
    /// The whole checkout, never a subdirectory of one, even when `from` is one. `git clean`
    /// scoped to a subdirectory cleans only that subtree while `git reset --hard` resets the
    /// entire work tree regardless, so a run rooted at a subdirectory would mean two different
    /// things by "here" in the same breath. Repo mode is the mode for cleaning a checkout, so
    /// it takes the checkout.
    ///
    /// # Errors
    ///
    /// If git cannot be run, or `from` is not inside a work tree.
    pub fn discover(from: &Path) -> Result<Self, RepoError> {
        let output = git(from)
            .args(["rev-parse", "--show-toplevel"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(RepoError::Run)?;
        if !output.status.success() {
            return Err(RepoError::NotAWorkTree {
                path: from.to_path_buf(),
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        let printed = trim_newline(&output.stdout);
        let root = decode(printed.to_vec()).ok_or_else(|| {
            RepoError::Unreadable(
                "git named a work tree root this cannot \
                express as a path"
                    .to_owned(),
            )
        })?;
        Ok(Self {
            root: PathBuf::from(root),
        })
    }

    /// The work tree's root directory, as git names it.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Asks git what it would remove.
    ///
    /// Two invocations, because the point is to offer the two lists separately: `-d` for
    /// untracked and `-d -X` for ignored.
    ///
    /// # Errors
    ///
    /// If git cannot be run, refuses, or prints something this cannot read.
    pub fn enumerate(&self) -> Result<Enumeration, RepoError> {
        let untracked = self.clean(&["clean", "-n", "-d"], "list untracked files")?;
        let ignored = self.clean(&["clean", "-n", "-d", "-X"], "list ignored files")?;

        let mut skipped = untracked.skipped;
        skipped.extend(ignored.skipped);
        skipped.sort_unstable();
        skipped.dedup();
        Ok(Enumeration {
            untracked: untracked.removals,
            ignored: ignored.removals,
            skipped,
        })
    }

    /// Discards tracked changes.
    ///
    /// Runs before any removal, because restoring a file the deleter is about to walk past is
    /// the one ordering here that has a consequence.
    ///
    /// # Errors
    ///
    /// If git cannot be run or refuses — an unborn `HEAD` is the ordinary way to reach the
    /// second, and a run that could not reset has not done what it said it would.
    pub fn reset(&self, reset: Reset) -> Result<(), RepoError> {
        let output = git(&self.root)
            .args(reset.args())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(RepoError::Run)?;
        if output.status.success() {
            return Ok(());
        }
        Err(RepoError::Refused {
            doing: reset.command(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }

    /// One `git clean -n` invocation, parsed.
    fn clean(&self, args: &[&str], doing: &'static str) -> Result<Cleaned, RepoError> {
        let output = git(&self.root)
            // Non-ASCII names then arrive as their own bytes instead of as octal escapes,
            // which leaves [`unquote`] with only the names that genuinely need quoting.
            .args(["-c", "core.quotePath=false"])
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(RepoError::Run)?;
        if !output.status.success() {
            return Err(RepoError::Refused {
                doing,
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        parse(&output.stdout, &self.root)
    }
}

/// What one `git clean -n` said.
#[derive(Debug, Default)]
struct Cleaned {
    removals: Vec<PathBuf>,
    skipped: Vec<PathBuf>,
}

/// The sentence `git clean -n` prints for something it would remove.
const WOULD_REMOVE: &[u8] = b"Would remove ";
/// The sentence it prints for a nested repository it will not touch.
const WOULD_SKIP: &[u8] = b"Would skip repository ";

/// Reads `git clean -n` output.
///
/// Anything that is neither sentence is refused. That is the load-bearing decision in this
/// function: the two alternatives are to treat an unknown line as a target, which deletes
/// whatever a future git prints a warning about, and to ignore it, which is the failure mode
/// #588 spent a review learning to distrust — a check that goes quiet and reads as a clear
/// result.
fn parse(stdout: &[u8], root: &Path) -> Result<Cleaned, RepoError> {
    let mut cleaned = Cleaned::default();
    for line in stdout.split(|byte| *byte == b'\n') {
        let line = trim_newline(line);
        if line.is_empty() {
            continue;
        }
        let (raw, into) = if let Some(rest) = line.strip_prefix(WOULD_REMOVE) {
            (rest, &mut cleaned.removals)
        } else if let Some(rest) = line.strip_prefix(WOULD_SKIP) {
            (rest, &mut cleaned.skipped)
        } else {
            return Err(RepoError::Unreadable(format!(
                "git clean said `{}`, which is not a sentence this knows how to read",
                String::from_utf8_lossy(line)
            )));
        };
        into.push(entry(raw, root)?);
    }
    Ok(cleaned)
}

/// One path out of one `git clean -n` line, resolved against the work tree root.
fn entry(raw: &[u8], root: &Path) -> Result<PathBuf, RepoError> {
    let mut bytes = if raw.first() == Some(&b'"') && raw.len() >= 2 && raw.last() == Some(&b'"') {
        unquote(&raw[1..raw.len() - 1]).ok_or_else(|| {
            RepoError::Unreadable(format!(
                "git clean quoted `{}` in a way this cannot unquote",
                String::from_utf8_lossy(raw)
            ))
        })?
    } else {
        raw.to_vec()
    };
    // git marks a directory with a trailing separator, which `PathBuf` keeps verbatim and
    // prints back at the user. Trimmed in bytes, before the path exists, so every target reads
    // the way one would be typed.
    if bytes.last() == Some(&b'/') {
        bytes.pop();
    }
    let decoded = decode(bytes).ok_or_else(|| {
        RepoError::Unreadable(format!(
            "git clean named `{}`, which cannot be expressed as a path here",
            String::from_utf8_lossy(raw)
        ))
    })?;

    // `PathBuf` drops the trailing separator git prints on a directory, so a target reads the
    // way a user would type it.
    let relative = PathBuf::from(decoded);
    // Refused rather than handed on. The planner would refuse an escaping path too, but it
    // would report it as one target the user did not get; git printing one at all means this
    // has misread the output, and the rest of the list cannot be trusted either. An empty
    // path is in the same class: joined to the root it *is* the root, which is the one
    // directory no plan may ever hold.
    if !relative
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
        || relative.as_os_str().is_empty()
    {
        return Err(RepoError::Unreadable(format!(
            "git clean named `{}`, which is not a path inside the work tree",
            relative.display()
        )));
    }
    Ok(root.join(relative))
}

/// Undoes git's C-style quoting, in bytes, without the surrounding quotes.
///
/// git escapes a name it cannot print literally, which is how a path holding a newline stays
/// on one line and line-based parsing stays safe. `\ooo` is always three octal digits and
/// always one byte, so a name that is not UTF-8 survives this intact.
fn unquote(inner: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(inner.len());
    let mut bytes = inner.iter().copied();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            out.push(byte);
            continue;
        }
        let escaped = match bytes.next()? {
            b'a' => 0x07,
            b'b' => 0x08,
            b't' => b'\t',
            b'n' => b'\n',
            b'v' => 0x0b,
            b'f' => 0x0c,
            b'r' => b'\r',
            b'"' => b'"',
            b'\\' => b'\\',
            first @ b'0'..=b'7' => {
                let mut value = u16::from(first - b'0');
                for _ in 0..2 {
                    let digit = bytes.next()?;
                    if !digit.is_ascii_digit() || digit > b'7' {
                        return None;
                    }
                    value = value * 8 + u16::from(digit - b'0');
                }
                u8::try_from(value).ok()?
            }
            _ => return None,
        };
        out.push(escaped);
    }
    Some(out)
}

/// Raw bytes as this platform's path string, or `None` where they cannot be one.
#[cfg(unix)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "infallible here and fallible off unix, where a path is UTF-16 and arbitrary \
              bytes are not one. Callers have to handle the failure that exists on the other \
              platform"
)]
fn decode(bytes: Vec<u8>) -> Option<OsString> {
    use std::os::unix::ffi::OsStringExt;
    Some(OsString::from_vec(bytes))
}

/// The same, where a path is UTF-16 and arbitrary bytes are not a path.
#[cfg(not(unix))]
fn decode(bytes: Vec<u8>) -> Option<OsString> {
    String::from_utf8(bytes).ok().map(OsString::from)
}

/// A line without whatever line ending it arrived with.
fn trim_newline(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Why a work tree could not be cleaned.
#[derive(Debug)]
#[non_exhaustive]
pub enum RepoError {
    /// git could not be run — most often because it is not installed.
    Run(io::Error),
    /// The path given is not inside a git work tree.
    NotAWorkTree {
        /// What was pointed at.
        path: PathBuf,
        /// Whatever git said about it.
        message: String,
    },
    /// git ran and refused, carrying what it was asked to do and what it said.
    Refused {
        /// The operation, as it would be typed.
        doing: &'static str,
        /// Whatever git said.
        message: String,
    },
    /// git printed something this cannot read, so the listing is not trustworthy and nothing
    /// is removed on the strength of it.
    Unreadable(String),
}

impl fmt::Display for RepoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Run(err) => write!(f, "could not run git: {err}"),
            Self::NotAWorkTree { path, message } => {
                let said = if message.is_empty() {
                    String::new()
                } else {
                    format!(": {message}")
                };
                write!(
                    f,
                    "{} is not inside a git work tree, and repo mode is the mode that cleans \
                     one{said}",
                    path.display()
                )
            }
            Self::Refused { doing, message } => {
                write!(f, "`{doing}` failed: {message}")
            }
            Self::Unreadable(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for RepoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Run(err) => Some(err),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Class, Enumeration, RepoError, Reset, Selection, classify, entry, parse, select, unquote,
    };
    use std::path::{Path, PathBuf};

    fn paths(cleaned: &[PathBuf]) -> Vec<String> {
        cleaned
            .iter()
            .map(|path| path.display().to_string())
            .collect()
    }

    #[test]
    fn the_two_sentences_git_prints_go_to_two_different_lists() {
        let cleaned = parse(
            b"Would remove out/\nWould skip repository vendor/inner\nWould remove note.txt\n",
            Path::new("/repo"),
        )
        .unwrap();

        assert_eq!(paths(&cleaned.removals), ["/repo/out", "/repo/note.txt"]);
        assert_eq!(paths(&cleaned.skipped), ["/repo/vendor/inner"]);
    }

    #[test]
    fn a_sentence_nothing_recognises_is_an_error_rather_than_a_target_or_a_silence() {
        // The two ways to get this wrong: treat it as a path, and delete whatever a future git
        // warns about; or ignore it, and report a listing that is short by an unknown amount
        // as if it were the whole truth.
        let read = parse(
            b"Would remove out/\nWurde etwas geloescht\n",
            Path::new("/repo"),
        );

        let Err(RepoError::Unreadable(why)) = read else {
            panic!("an unknown line was accepted: {read:?}");
        };
        assert!(why.contains("Wurde etwas geloescht"), "{why}");
    }

    #[test]
    fn a_translated_listing_is_refused_rather_than_read_as_a_clean_repository() {
        // What `LANGUAGE=de` actually prints. The locale is forced to C for every invocation,
        // so this is unreachable in practice — and it is exactly the failure that would be
        // invisible if it ever became reachable again, so the parser refuses it out loud.
        let read = parse("Würde out/ löschen\n".as_bytes(), Path::new("/repo"));

        assert!(matches!(read, Err(RepoError::Unreadable(_))), "{read:?}");
    }

    #[test]
    fn a_quoted_name_comes_back_as_the_bytes_it_was() {
        // git quotes anything it cannot print literally, which is what keeps a name holding a
        // newline on one line.
        let cleaned = parse(
            b"Would remove \"two\\nlines.txt\"\nWould remove \"say \\\"hi\\\".txt\"\n",
            Path::new("/repo"),
        )
        .unwrap();

        assert_eq!(
            paths(&cleaned.removals),
            ["/repo/two\nlines.txt", "/repo/say \"hi\".txt"]
        );
    }

    #[test]
    fn octal_escapes_are_bytes_and_not_characters() {
        // `café` decomposed, as git would escape it with `core.quotePath` left on.
        assert_eq!(unquote(b"caf\\303\\251").unwrap(), "café".as_bytes());
        assert_eq!(unquote(b"a\\tb").unwrap(), b"a\tb");
        // Truncated, and a byte past what one can hold.
        assert!(unquote(b"caf\\30").is_none());
        assert!(unquote(b"\\777").is_none());
        assert!(unquote(b"\\q").is_none());
        assert!(unquote(b"ends-with\\").is_none());
    }

    #[test]
    fn a_path_that_would_leave_the_work_tree_is_refused() {
        // Unreachable from a git that is behaving. Reachable from a misread line, and one
        // misread line means the whole listing is guesswork.
        for bad in [
            &b"Would remove ../elsewhere"[..],
            b"Would remove /etc/passwd",
            b"Would remove ",
        ] {
            assert!(
                parse(bad, Path::new("/repo")).is_err(),
                "`{}` was accepted",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn a_trailing_separator_is_not_part_of_the_target() {
        assert_eq!(
            entry(b"out/", Path::new("/repo")).unwrap(),
            PathBuf::from("/repo/out")
        );
    }

    #[test]
    fn vendor_is_any_path_with_a_node_modules_in_it() {
        assert_eq!(classify(Path::new("/r/node_modules")), Class::Vendor);
        assert_eq!(classify(Path::new("/r/app/node_modules")), Class::Vendor);
        // What git hands back when a `node_modules` could not be collapsed.
        assert_eq!(
            classify(Path::new("/r/node_modules/.bin/tsc")),
            Class::Vendor
        );
        assert_eq!(classify(Path::new("/r/node_modules_old")), Class::Other);
    }

    #[test]
    fn env_is_the_designs_star_dot_env_star_against_the_final_component() {
        for env in [".env", ".env.local", "prod.env", ".env.production.local"] {
            assert_eq!(classify(&Path::new("/r").join(env)), Class::Env, "{env}");
        }
        for other in ["environment", "dist", "envoy.yaml"] {
            assert_eq!(
                classify(&Path::new("/r").join(other)),
                Class::Other,
                "{other}"
            );
        }
    }

    fn enumeration() -> Enumeration {
        Enumeration {
            untracked: vec![PathBuf::from("/r/scratch.txt"), PathBuf::from("/r/.env")],
            ignored: vec![
                PathBuf::from("/r/dist"),
                PathBuf::from("/r/node_modules"),
                PathBuf::from("/r/.env.local"),
            ],
            skipped: Vec::new(),
        }
    }

    #[test]
    fn nothing_is_selected_by_default() {
        let selected = select(&enumeration(), &Selection::default());
        assert!(selected.targets.is_empty());
        assert!(Selection::default().is_empty());
    }

    #[test]
    fn vendor_and_env_are_held_back_from_a_list_the_user_did_ask_for() {
        let selected = select(
            &enumeration(),
            &Selection {
                untracked: true,
                ignored: true,
                ..Selection::default()
            },
        );

        assert_eq!(
            paths(
                &selected
                    .targets
                    .iter()
                    .map(|target| target.path.clone())
                    .collect::<Vec<_>>()
            ),
            ["/r/scratch.txt", "/r/dist"]
        );
        assert_eq!(selected.vendor, 1);
        // Both of them: the untracked `.env` as well as the ignored one. The guard is about
        // what the file is worth, not about which of git's two lists it arrived in.
        assert_eq!(selected.env, 2);
    }

    #[test]
    fn opting_in_puts_them_back() {
        let selected = select(
            &enumeration(),
            &Selection {
                untracked: true,
                ignored: true,
                vendor: true,
                env: true,
                ..Selection::default()
            },
        );

        assert_eq!(selected.targets.len(), 5);
        assert_eq!((selected.vendor, selected.env), (0, 0));
    }

    #[test]
    fn one_list_can_be_taken_without_the_other() {
        let only_ignored = select(
            &enumeration(),
            &Selection {
                ignored: true,
                ..Selection::default()
            },
        );
        assert_eq!(
            paths(
                &only_ignored
                    .targets
                    .iter()
                    .map(|target| target.path.clone())
                    .collect::<Vec<_>>()
            ),
            ["/r/dist"]
        );
    }

    #[test]
    fn the_reset_verbs_are_the_ones_the_design_named() {
        assert_eq!(Reset::WorkTree.command(), "git restore -- .");
        assert_eq!(Reset::Hard.command(), "git reset --hard HEAD");
    }
}
