//! Turning the ruleset into a decision about one directory.
//!
//! The hot path is "is this directory reclaimable", asked once per directory in the tree, so
//! the ruleset is compiled into a name index first. A directory whose name matches no
//! target's final segment — the overwhelming majority — costs one hash lookup and nothing
//! else. Only a name that could match pays for the `stat` calls that check the markers.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use globset::{Glob, GlobMatcher, GlobSet, GlobSetBuilder};

use crate::rules::{Anchor, MarkersRequired, Rule, RuleError};
use crate::walk::RuleClaim;

/// How far up an `anchor = "ancestor"` rule will look before giving up. Deep enough for any
/// real project layout, shallow enough that a pathological tree cannot make the search
/// quadratic.
const MAX_ANCESTOR_SEARCH: usize = 32;

/// The compiled ruleset.
#[derive(Debug)]
pub(crate) struct Detector {
    rules: Vec<CompiledRule>,
    targets: Vec<CompiledTarget>,
    /// Target indices keyed by their literal final segment, in rule order.
    by_leaf: HashMap<String, Vec<usize>>,
    /// Targets whose final segment is a glob (`bazel-*`), and the set that matches them.
    leaf_globs: GlobSet,
    leaf_glob_targets: Vec<usize>,
}

#[derive(Debug)]
struct CompiledRule {
    rule: Arc<Rule>,
    literal_markers: Vec<String>,
    glob_markers: Vec<GlobMatcher>,
}

#[derive(Debug)]
struct CompiledTarget {
    rule: usize,
    /// Every segment but the last, in path order, verified against the matched directory's
    /// ancestors. Always literal: no shipped rule needs a glob here and allowing one would
    /// mean walking the path twice.
    parents: Vec<String>,
    /// `parents.len() + 1`, so the minimum walk depth at which this target can match.
    depth: usize,
}

impl Detector {
    /// Compiles the rules into the name index.
    ///
    /// # Errors
    ///
    /// If a marker or target pattern is not a valid glob, or a non-final target segment is
    /// globbed.
    pub(crate) fn new(rules: &[Arc<Rule>]) -> Result<Self, RuleError> {
        let mut compiled_rules = Vec::with_capacity(rules.len());
        let mut targets = Vec::new();
        let mut by_leaf: HashMap<String, Vec<usize>> = HashMap::new();
        let mut leaf_globs = GlobSetBuilder::new();
        let mut leaf_glob_targets = Vec::new();

        for (index, rule) in rules.iter().enumerate() {
            let mut literal_markers = Vec::new();
            let mut glob_markers = Vec::new();
            for marker in &rule.markers {
                if is_glob(marker) {
                    glob_markers.push(compile(marker)?.compile_matcher());
                } else {
                    literal_markers.push(marker.clone());
                }
            }
            compiled_rules.push(CompiledRule {
                rule: Arc::clone(rule),
                literal_markers,
                glob_markers,
            });

            for target in &rule.targets {
                let mut segments: Vec<&str> = target
                    .split('/')
                    .filter(|segment| !segment.is_empty())
                    .collect();
                let Some(leaf) = segments.pop() else {
                    return Err(RuleError::NoTargets(rule.id.clone()));
                };
                if let Some(globbed) = segments.iter().find(|segment| is_glob(segment)) {
                    return Err(RuleError::Glob(
                        (*globbed).to_owned(),
                        "only the final segment of a target may be globbed".to_owned(),
                    ));
                }

                let at = targets.len();
                targets.push(CompiledTarget {
                    rule: index,
                    parents: segments.iter().map(|s| (*s).to_owned()).collect(),
                    depth: segments.len() + 1,
                });
                if is_glob(leaf) {
                    leaf_globs.add(compile(leaf)?);
                    leaf_glob_targets.push(at);
                } else {
                    by_leaf.entry(leaf.to_owned()).or_default().push(at);
                }
            }
        }

        let leaf_globs = leaf_globs
            .build()
            .map_err(|err| RuleError::Glob("target".to_owned(), err.to_string()))?;
        Ok(Self {
            rules: compiled_rules,
            targets,
            by_leaf,
            leaf_globs,
            leaf_glob_targets,
        })
    }

    /// Judges one directory. `depth` is its depth below `scan_root`, which the caller already
    /// knows from the walk, and `scan_root` bounds the upward search of ancestor-anchored
    /// rules.
    ///
    /// Rules are evaluated in ruleset order and the first match wins, so a directory is
    /// claimed once even when two ecosystems could both explain it.
    pub(crate) fn detect(&self, dir: &Path, scan_root: &Path, depth: usize) -> Option<RuleClaim> {
        // A name that is not valid UTF-8 cannot equal any target we ship, all of which are
        // ASCII, so skipping it loses nothing.
        let name = dir.file_name()?.to_str()?;

        let mut candidates: Vec<usize> = self.by_leaf.get(name).cloned().unwrap_or_default();
        if self.leaf_globs.is_match(name) {
            candidates.extend(
                self.leaf_globs
                    .matches(name)
                    .into_iter()
                    .map(|at| self.leaf_glob_targets[at]),
            );
            candidates.sort_unstable();
            candidates.dedup();
        }

        for candidate in candidates {
            let target = &self.targets[candidate];
            if depth < target.depth {
                continue;
            }
            let Some(parent_anchor) = walk_up(dir, &target.parents) else {
                continue;
            };
            let compiled = &self.rules[target.rule];
            let anchor = match compiled.rule.anchor {
                Anchor::Parent => compiled
                    .markers_present(parent_anchor)
                    .then(|| parent_anchor.to_path_buf()),
                Anchor::SelfDir => compiled.markers_present(dir).then(|| dir.to_path_buf()),
                Anchor::Ancestor => compiled.nearest_marked_ancestor(parent_anchor, scan_root),
            };
            if let Some(project_root) = anchor {
                return Some(RuleClaim {
                    regenerate: compiled.regeneration_command(&project_root, scan_root),
                    rule: Arc::clone(&compiled.rule),
                    project_root,
                });
            }
        }
        None
    }
}

impl CompiledRule {
    /// Whether `anchor` carries the evidence this rule demands.
    fn markers_present(&self, anchor: &Path) -> bool {
        let all = self.rule.markers_required == MarkersRequired::All;
        let mut any = false;

        for marker in &self.literal_markers {
            // `symlink_metadata` rather than `exists`: one syscall, and a marker that is a
            // dangling symlink still counts as present.
            let present = anchor.join(marker).symlink_metadata().is_ok();
            if all && !present {
                return false;
            }
            any |= present;
        }

        if !self.glob_markers.is_empty() {
            // Globbed markers (`*.csproj`) cannot be probed by name, so they cost one
            // listing. This is why the shipped rules prefer literals.
            let mut seen = vec![false; self.glob_markers.len()];
            if let Ok(entries) = fs::read_dir(anchor) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    for (at, matcher) in self.glob_markers.iter().enumerate() {
                        seen[at] |= matcher.is_match(AsRef::<OsStr>::as_ref(&name));
                    }
                    if seen.iter().all(|found| *found) {
                        break;
                    }
                }
            }
            if all && seen.iter().any(|found| !found) {
                return false;
            }
            any |= seen.iter().any(|found| *found);
        }

        all || any
    }

    /// The concrete command that regenerates a claim owned by `project_root`.
    ///
    /// A rule with no `regenerate_when` entries — every rule but Node today — answers from
    /// its own field without touching the filesystem. Node pays a handful of `stat` calls per
    /// claim to turn "npm ci / pnpm install / yarn" into the one of those the project
    /// actually uses.
    ///
    /// The search starts at the project root and walks up, because a workspace keeps one
    /// lockfile at its root while every package under it has its own `package.json` and its
    /// own `node_modules`. It stops at the scan root: a lockfile above the tree the user
    /// pointed us at is not evidence we are entitled to use.
    fn regeneration_command(&self, project_root: &Path, scan_root: &Path) -> String {
        if self.rule.regenerate_when.is_empty() {
            return self.rule.regenerate.clone();
        }
        let mut cursor = Some(project_root);
        for _ in 0..MAX_ANCESTOR_SEARCH {
            let Some(candidate) = cursor else { break };
            if !candidate.starts_with(scan_root) {
                break;
            }
            for when in &self.rule.regenerate_when {
                if candidate.join(&when.marker).symlink_metadata().is_ok() {
                    return when.regenerate.clone();
                }
            }
            if candidate == scan_root {
                break;
            }
            cursor = candidate.parent();
        }
        self.rule.regenerate.clone()
    }

    /// The nearest ancestor of `from` (inclusive) carrying this rule's markers, searched no
    /// higher than `scan_root`.
    fn nearest_marked_ancestor(&self, from: &Path, scan_root: &Path) -> Option<PathBuf> {
        let mut cursor = Some(from);
        for _ in 0..MAX_ANCESTOR_SEARCH {
            let candidate = cursor?;
            if !candidate.starts_with(scan_root) {
                return None;
            }
            if self.markers_present(candidate) {
                return Some(candidate.to_path_buf());
            }
            if candidate == scan_root {
                return None;
            }
            cursor = candidate.parent();
        }
        None
    }
}

/// Verifies `dir`'s ancestors against a multi-segment target and returns the directory the
/// target path hangs off. For a single-segment target that is just `dir.parent()`.
fn walk_up<'a>(dir: &'a Path, parents: &[String]) -> Option<&'a Path> {
    let mut cursor = dir.parent()?;
    for expected in parents.iter().rev() {
        if cursor.file_name()? != OsStr::new(expected) {
            return None;
        }
        cursor = cursor.parent()?;
    }
    Some(cursor)
}

fn is_glob(pattern: &str) -> bool {
    pattern.contains(['*', '?', '[', '{'])
}

fn compile(pattern: &str) -> Result<Glob, RuleError> {
    Glob::new(pattern).map_err(|err| RuleError::Glob(pattern.to_owned(), err.to_string()))
}
