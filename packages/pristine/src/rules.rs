//! The tier-one ruleset: what a reclaimable directory looks like, as data.
//!
//! The rules themselves live in [`rules.toml`](../src/rules.toml), which is compiled into the
//! binary and can be extended or replaced by a user file. Nothing in this module encodes a
//! single ecosystem, so adding one is a config edit rather than a release.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use crate::detect::Detector;

/// The ruleset shipped with the binary.
const BUILTIN: &str = include_str!("rules.toml");

/// Where the markers for a rule are looked for, relative to the directory being judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Anchor {
    /// The directory the target path hangs off. For a single-segment target such as
    /// `node_modules` that is simply the matched directory's parent.
    #[default]
    Parent,
    /// The matched directory itself. This is how an ambiguous name is made safe: a CMake
    /// `build/` is only output if it contains the `CMakeCache.txt` CMake wrote there.
    #[serde(rename = "self")]
    SelfDir,
    /// The nearest ancestor carrying a marker, searched upward as far as the scan root. For
    /// artefacts scattered through a project rather than parked at its root, like
    /// `__pycache__`.
    Ancestor,
}

/// What a reclaimable directory *is*, from a vocabulary of three.
///
/// The vocabulary is closed on purpose, and it is the half of a label that a machine can act
/// on: a kind sorts, groups and filters, so "show me every cache" is a question the front end
/// can answer. A free-text sentence never could.
///
/// It also carries the cost, which is the one thing the regeneration command it replaced was
/// genuinely for. A cache is free, an output is a compile, dependencies are a network fetch —
/// and unlike a command string, that reading holds without knowing anything about the machine
/// it would be paid on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Installed third-party code: `node_modules`, `.venv`, `vendor`.
    Dependencies,
    /// Compiled output: `target`, `bin`, `obj`, `dist`.
    Build,
    /// Regenerated automatically, and the cheapest of the three to lose: `__pycache__`,
    /// `.nx/cache`, `.gradle`, `.ipynb_checkpoints`.
    Cache,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Dependencies => "Dependencies",
            Self::Build => "Build Artifacts",
            Self::Cache => "Cache",
        })
    }
}

/// Whether one marker is enough, or all of them are needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarkersRequired {
    /// Any one marker proves the project type. The common case.
    #[default]
    Any,
    /// Every marker must be present. Unity is a `ProjectSettings/` **and** an `Assets/`.
    All,
}

/// One marker-anchored rule: the evidence that a project is of some kind, and the directories
/// that kind of project generates.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Stable identifier. A user rule with the same id replaces the built-in one.
    pub id: String,
    /// Human-readable name of the ecosystem: the first half of the label.
    pub ecosystem: String,
    /// What the directories this rule claims are: the second half of the label.
    ///
    /// A rule whose targets do not all share one kind is two rules, which is why `python` and
    /// `python-caches` are separate and why `gradle`'s `build` and `.gradle` are. One rule
    /// covering both could only label one of them honestly.
    pub kind: Kind,
    /// File or directory names proving the anchor is a project of this kind. A name
    /// containing `*` or `?` is a glob and costs a directory listing to check.
    pub markers: Vec<String>,
    /// Whether one marker is enough.
    #[serde(default)]
    pub markers_required: MarkersRequired,
    /// Reclaimable paths relative to the anchor. Multi-segment paths (`vendor/bundle`) and a
    /// globbed final segment (`bazel-*`) are both allowed.
    pub targets: Vec<String>,
    /// Where the markers are looked for.
    #[serde(default)]
    pub anchor: Anchor,
    /// An optional caveat to surface next to the hit.
    #[serde(default)]
    pub note: Option<String>,
}

impl Rule {
    /// What the directories this rule claims are, named: the ecosystem and the kind.
    ///
    /// Composed rather than written out per rule, because thirty hand-written strings are
    /// thirty chances to drift apart — and because the half that a reader groups by has to be
    /// the same word every time it appears.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{} {}", self.ecosystem, self.kind)
    }
}

/// A parsed, validated and compiled set of rules.
#[derive(Debug)]
pub struct Ruleset {
    rules: Vec<Arc<Rule>>,
    detector: Detector,
}

/// The wire shape of a rules file. Only ever seen by serde.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RulesFile {
    #[serde(default)]
    rules: Vec<Rule>,
}

impl Ruleset {
    /// The ruleset compiled into the binary.
    ///
    /// # Errors
    ///
    /// Only if the compiled-in TOML is malformed, which a unit test in this module rules out.
    pub fn builtin() -> Result<Self, RuleError> {
        Self::from_rules(Self::parse_rules(BUILTIN)?)
    }

    /// Parses a rules file on its own, replacing the built-in set entirely.
    ///
    /// # Errors
    ///
    /// If the TOML is malformed, a rule is incomplete, or a glob fails to compile.
    pub fn parse(toml: &str) -> Result<Self, RuleError> {
        Self::from_rules(Self::parse_rules(toml)?)
    }

    /// Parses a user rules file and layers it over the built-in set: a rule whose `id`
    /// already exists replaces the built-in one in place, and a new `id` is appended.
    ///
    /// Replacing in place rather than appending keeps the first-match-wins ordering
    /// predictable — overriding `cargo` does not quietly move it behind `maven`.
    ///
    /// # Errors
    ///
    /// If either file is malformed, a rule is incomplete, or a glob fails to compile.
    pub fn with_overrides(user_toml: &str) -> Result<Self, RuleError> {
        let mut rules = Self::parse_rules(BUILTIN)?;
        for rule in Self::parse_rules(user_toml)? {
            match rules.iter().position(|existing| existing.id == rule.id) {
                Some(at) => rules[at] = rule,
                None => rules.push(rule),
            }
        }
        Self::from_rules(rules)
    }

    /// Loads the ruleset, layering the user's file over the built-in set when it exists.
    ///
    /// Passing `None` looks in the default location, [`Ruleset::user_config_path`]. A missing
    /// file is not an error; an unreadable or malformed one is, because silently falling back
    /// to the built-in set would hide the user's edits from them.
    ///
    /// # Errors
    ///
    /// If the user file exists but cannot be read or parsed.
    pub fn load(user_path: Option<&Path>) -> Result<Self, RuleError> {
        let Some(path) = user_path.map(PathBuf::from).or_else(Self::user_config_path) else {
            return Self::builtin();
        };
        match fs::read_to_string(&path) {
            Ok(toml) => Self::with_overrides(&toml),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::builtin(),
            Err(err) => Err(RuleError::Read(path, err)),
        }
    }

    /// The default location of the user's rules file, `$XDG_CONFIG_HOME/pristine/rules.toml`
    /// falling back to `~/.config/pristine/rules.toml`.
    #[must_use]
    pub fn user_config_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(base.join("pristine").join("rules.toml"))
    }

    /// The rules, in evaluation order.
    #[must_use]
    pub fn rules(&self) -> &[Arc<Rule>] {
        &self.rules
    }

    pub(crate) fn detector(&self) -> &Detector {
        &self.detector
    }

    fn parse_rules(toml: &str) -> Result<Vec<Rule>, RuleError> {
        let file: RulesFile =
            toml::from_str(toml).map_err(|err| RuleError::Parse(err.to_string()))?;
        Ok(file.rules)
    }

    fn from_rules(rules: Vec<Rule>) -> Result<Self, RuleError> {
        for rule in &rules {
            if rule.markers.is_empty() {
                return Err(RuleError::NoMarkers(rule.id.clone()));
            }
            if rule.targets.is_empty() {
                return Err(RuleError::NoTargets(rule.id.clone()));
            }
            if let Some(other) = rules.iter().filter(|r| r.id == rule.id).nth(1) {
                return Err(RuleError::DuplicateId(other.id.clone()));
            }
        }
        let rules: Vec<Arc<Rule>> = rules.into_iter().map(Arc::new).collect();
        let detector = Detector::new(&rules)?;
        Ok(Self { rules, detector })
    }
}

/// Why a ruleset could not be loaded.
#[derive(Debug)]
#[non_exhaustive]
pub enum RuleError {
    /// The user's rules file could not be read.
    Read(PathBuf, std::io::Error),
    /// The TOML was malformed or a rule was missing a mandatory field.
    Parse(String),
    /// A rule declared no markers, which would make it a bare-name match.
    NoMarkers(String),
    /// A rule declared nothing to reclaim.
    NoTargets(String),
    /// Two rules share an id, so an override would be ambiguous.
    DuplicateId(String),
    /// A marker or target pattern is not a valid glob.
    Glob(String, String),
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(path, err) => write!(f, "reading rules from {}: {err}", path.display()),
            Self::Parse(err) => write!(f, "parsing rules: {err}"),
            Self::NoMarkers(id) => write!(
                f,
                "rule `{id}` declares no markers; a rule without one is a bare-name match, \
                 which is how a cleaner deletes somebody's source"
            ),
            Self::NoTargets(id) => write!(f, "rule `{id}` declares nothing to reclaim"),
            Self::DuplicateId(id) => write!(f, "two rules share the id `{id}`"),
            Self::Glob(pattern, err) => write!(f, "`{pattern}` is not a valid glob: {err}"),
        }
    }
}

impl std::error::Error for RuleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(_, err) => Some(err),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Anchor, Kind, MarkersRequired, RuleError, Ruleset};

    #[test]
    fn the_builtin_ruleset_parses() {
        let ruleset = Ruleset::builtin().unwrap();
        assert!(
            ruleset.rules().len() >= 20,
            "kondo covers 20+ project types and pristine must not ship narrower"
        );
    }

    #[test]
    fn every_builtin_rule_is_marker_anchored_and_names_what_it_claims() {
        for rule in Ruleset::builtin().unwrap().rules() {
            assert!(!rule.markers.is_empty(), "{} has no marker", rule.id);
            assert!(!rule.targets.is_empty(), "{} reclaims nothing", rule.id);
            assert!(
                !rule.ecosystem.is_empty(),
                "{} has no ecosystem name",
                rule.id
            );
        }
    }

    #[test]
    fn a_label_is_the_ecosystem_and_the_kind() {
        let ruleset = Ruleset::builtin().unwrap();
        let labelled = |id: &str| {
            ruleset
                .rules()
                .iter()
                .find(|rule| rule.id == id)
                .unwrap_or_else(|| panic!("no rule for {id}"))
                .label()
        };
        assert_eq!(labelled("node"), "Node Dependencies");
        assert_eq!(labelled("dotnet"), ".NET Build Artifacts");
        assert_eq!(labelled("nx-caches"), "Nx Cache");
        // One ecosystem, two kinds, two rules — the split the vocabulary makes explicit.
        assert_eq!(labelled("python"), "Python Dependencies");
        assert_eq!(labelled("python-caches"), "Python Cache");
    }

    #[test]
    fn a_rule_that_does_not_say_what_it_claims_is_rejected() {
        // Not defaulted: a kind that can be omitted is a kind that is silently wrong, and it
        // is the field a reader groups and filters by.
        let err = Ruleset::parse(
            r#"
            [[rules]]
            id = "nameless"
            ecosystem = "Nameless"
            markers = ["m"]
            targets = ["out"]
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, RuleError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn a_kind_outside_the_vocabulary_is_rejected() {
        let err = Ruleset::parse(
            r#"
            [[rules]]
            id = "inventive"
            ecosystem = "Inventive"
            markers = ["m"]
            targets = ["out"]
            kind = "sediment"
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, RuleError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn the_builtin_ruleset_covers_the_ecosystems_kondo_does() {
        let ruleset = Ruleset::builtin().unwrap();
        let ids: Vec<&str> = ruleset.rules().iter().map(|r| r.id.as_str()).collect();
        for expected in [
            "node",
            "cargo",
            "go",
            "python",
            "dotnet",
            "gradle",
            "maven",
            "composer",
            "bundler",
            "elixir",
            "swift",
            "dart",
            "zig",
            "unity",
            "nx",
            "bazel",
            "cmake",
            "godot",
            "unreal",
            "terraform",
            "react-native",
            "sbt",
            "stack",
            "cabal",
            "pixi",
            "jupyter",
            "turborepo",
        ] {
            assert!(ids.contains(&expected), "no rule for {expected}");
        }
    }

    #[test]
    fn anchors_marker_modes_and_kinds_round_trip_from_toml() {
        let ruleset = Ruleset::parse(
            r#"
            [[rules]]
            id = "a"
            ecosystem = "A"
            anchor = "self"
            markers_required = "all"
            markers = ["m1", "m2"]
            targets = ["out"]
            kind = "build"
            "#,
        )
        .unwrap();
        let rule = &ruleset.rules()[0];
        assert_eq!(rule.anchor, Anchor::SelfDir);
        assert_eq!(rule.markers_required, MarkersRequired::All);
        assert_eq!(rule.kind, Kind::Build);
        assert_eq!(rule.label(), "A Build Artifacts");
    }

    #[test]
    fn a_user_rule_replaces_a_builtin_one_in_place() {
        let builtin = Ruleset::builtin().unwrap();
        let cargo_at = builtin
            .rules()
            .iter()
            .position(|r| r.id == "cargo")
            .unwrap();

        let ruleset = Ruleset::with_overrides(
            r#"
            [[rules]]
            id = "cargo"
            ecosystem = "Rust"
            markers = ["Cargo.toml"]
            targets = ["target", "coverage"]
            kind = "build"
            "#,
        )
        .unwrap();

        assert_eq!(ruleset.rules().len(), builtin.rules().len());
        let cargo = &ruleset.rules()[cargo_at];
        assert_eq!(cargo.targets, ["target", "coverage"]);
    }

    #[test]
    fn a_rule_without_markers_is_rejected() {
        let err = Ruleset::parse(
            r#"
            [[rules]]
            id = "reckless"
            ecosystem = "Reckless"
            markers = []
            targets = ["build"]
            kind = "build"
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, RuleError::NoMarkers(id) if id == "reckless"));
    }

    #[test]
    fn an_unknown_field_is_rejected_rather_than_silently_ignored() {
        let err = Ruleset::parse(
            r#"
            [[rules]]
            id = "typo"
            ecosystem = "Typo"
            markers = ["m"]
            target = ["build"]
            targets = ["build"]
            kind = "build"
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, RuleError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let err = Ruleset::parse(
            r#"
            [[rules]]
            id = "dup"
            ecosystem = "A"
            markers = ["a"]
            targets = ["out"]
            kind = "build"

            [[rules]]
            id = "dup"
            ecosystem = "B"
            markers = ["b"]
            targets = ["out"]
            kind = "build"
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, RuleError::DuplicateId(id) if id == "dup"));
    }
}
