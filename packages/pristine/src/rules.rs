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

/// What a reclaimable thing *is*, from a closed vocabulary **ordered by what it costs to
/// lose**.
///
/// The vocabulary is closed on purpose, and it is the half of a label that a machine can act
/// on: a kind sorts, groups and filters, so "show me every cache" is a question the front end
/// can answer. A free-text sentence never could.
///
/// It also carries the cost, which is the one thing the regeneration command it replaced was
/// genuinely for. A cache is free, an output is a compile, dependencies are a network fetch —
/// and unlike a command string, that reading holds without knowing anything about the machine
/// it would be paid on.
///
/// # The ordering is the point, and it is [`Kind::ALL`]
///
/// The three middle members were already ordered — what was fetched, what was compiled, what
/// will come back on its own — and [`Kind::ALL`] is now that ordering made explicit, with a
/// member at each extreme:
///
/// | | cost to lose |
/// |---|---|
/// | [`Unrecoverable`](Self::Unrecoverable) | **nothing brings it back** |
/// | [`Dependencies`](Self::Dependencies) | a network fetch |
/// | [`Build`](Self::Build) | a compile |
/// | [`Cache`](Self::Cache) | it returns on its own |
/// | [`Noise`](Self::Noise) | nothing will miss it |
///
/// Everything else in the crate reads the order off `ALL` rather than restating it, so the
/// confirmation groups the expensive end first and the front end derives a key per member.
///
/// # A kind NAMES, it does not gate
///
/// [`Unrecoverable`](Self::Unrecoverable) inverts the premise of the rest of the vocabulary —
/// nothing brings it back — and it is tempting to make the code treat it apart: skip it in a
/// bulk mark, put a second flag in front of deleting one. **That was built and then removed,
/// and the reason is worth keeping.** A mark is a statement about a subtree, and the fractional
/// glyph on an ancestor is a true reading of how much of it is spoken for; a mark that silently
/// skipped some descendants would make that glyph describe a set no reader can see, and the
/// exception would live nowhere a reader could find it.
///
/// So the safety is carried entirely by **what a view shows**, which is a lever the front end
/// already had: no lens shows gitignored files until `i` says so, no sweep claims one until
/// `--ignored-files` says so, and a mark carries the lens it was made through. Seeing a `.env`
/// at all is the deliberate act. After that it is a row like any other — same keys, same rules,
/// one confirmation that lists the whole batch with the expensive end first.
///
/// What the kind still does is *name* the thing, which is what a label has been for since the
/// regeneration command was deleted. See [`Kind::of_ignored_file`] and [`super::tui::lens`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Something nothing regenerates: an `.env`, a private key, a `credentials` file.
    ///
    /// A rule may declare it — a `secrets/` directory somebody's own ruleset names is exactly
    /// as unrecoverable as a `.env` — but nothing in the shipped ruleset does, because the
    /// evidence a marker-anchored rule offers is "this project is of that type" rather than
    /// "this file is the only copy".
    Unrecoverable,
    /// Installed third-party code: `node_modules`, `.venv`, `vendor`.
    Dependencies,
    /// Compiled output: `target`, `bin`, `obj`, `dist`.
    Build,
    /// Regenerated automatically: `__pycache__`, `.nx/cache`, `.gradle`, `.ipynb_checkpoints`.
    Cache,
    /// Written by something nobody asked, and read by nothing: `*.log`, `.DS_Store`,
    /// `Thumbs.db`. The cheapest thing here to lose.
    Noise,
}

/// What `*.env*` reduces to against a single path component.
///
/// Shared with [`crate::repo`], which asks the same question of a `git clean` entry. A run where
/// the two disagreed would report holding an env file back through one door and delete it
/// through the other.
pub(crate) const ENV_MARK: &str = ".env";

/// Names that mean "nothing brings this back", matched whole against a lowercased file name.
const UNRECOVERABLE_NAMES: [&str; 5] = [".npmrc", "credentials", "id_rsa", "id_ecdsa", "id_ed25519"];

/// Suffixes that mean the same. `.pem` is a private key far more often than it is anything
/// else, and the times it is a certificate it is still not something a rebuild produces.
const UNRECOVERABLE_SUFFIXES: [&str; 1] = [".pem"];

/// Names nothing will miss, matched whole.
const NOISE_NAMES: [&str; 2] = [".ds_store", "thumbs.db"];

/// Suffixes nothing will miss.
const NOISE_SUFFIXES: [&str; 1] = [".log"];

impl Kind {
    /// The whole vocabulary, **in order of what it costs to lose**: what nothing brings back,
    /// what was fetched, what was compiled, what will come back on its own, what nothing will
    /// miss.
    ///
    /// Being able to enumerate it is half of what "closed" buys — the front end derives a key
    /// and a help sentence per kind from this rather than listing them again, and the
    /// confirmation's grouping is this order, so a sixth kind would arrive already filterable
    /// and already sorted.
    pub const ALL: [Self; 5] = [
        Self::Unrecoverable,
        Self::Dependencies,
        Self::Build,
        Self::Cache,
        Self::Noise,
    ];

    /// Where this kind sits on the cost axis, counting from the expensive end.
    ///
    /// Read off [`Kind::ALL`] rather than written out a second time, because a listing that
    /// grouped in one order while the help page named them in another would be two claims
    /// about one vocabulary.
    #[must_use]
    pub fn cost(self) -> usize {
        Self::ALL
            .iter()
            .position(|&kind| kind == self)
            .unwrap_or(Self::ALL.len())
    }

    /// What the name of a gitignored **file** says it is, or `None` when the name says
    /// nothing.
    ///
    /// This is a claim about a name and not about contents, which is why the two ends of the
    /// vocabulary are the only ones it can reach: `.env` and `id_rsa` are the only copy of
    /// something, `*.log` and `.DS_Store` are the copy of nothing. A name that says neither
    /// gets `None` and reads as the tier-two directory claim already does — git knows the file
    /// is disposable and nothing knows what it is.
    ///
    /// Matched against the lowercased name, because `.DS_Store` and `Thumbs.db` are written
    /// both ways by the systems that create them and the case is not information.
    #[must_use]
    pub fn of_ignored_file(name: &str) -> Option<Self> {
        let name = name.to_ascii_lowercase();
        // The expensive end is asked first, and that ordering is the safety property rather
        // than a tidiness one: a name that could be read either way — `.env.log` — is the one
        // where being wrong in the cheap direction is the failure that cannot be undone.
        if name.contains(ENV_MARK)
            || UNRECOVERABLE_NAMES.contains(&name.as_str())
            || UNRECOVERABLE_SUFFIXES
                .iter()
                .any(|suffix| name.ends_with(suffix))
        {
            return Some(Self::Unrecoverable);
        }
        if NOISE_NAMES.contains(&name.as_str())
            || NOISE_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
        {
            return Some(Self::Noise);
        }
        None
    }

    /// One word, for somewhere with no room for the whole label.
    #[must_use]
    pub fn short(self) -> &'static str {
        match self {
            Self::Unrecoverable => "unrecoverable",
            Self::Dependencies => "dependencies",
            Self::Build => "build",
            Self::Cache => "cache",
            Self::Noise => "noise",
        }
    }

    /// What losing one of these costs, said out loud.
    ///
    /// The vocabulary's whole content in one sentence per member, which is what the help page
    /// and the confirmation print. `Unrecoverable` is not decoration and a reader has to be
    /// able to find out what it means without reading this file.
    #[must_use]
    pub fn cost_said(self) -> &'static str {
        match self {
            Self::Unrecoverable => "nothing brings this back",
            Self::Dependencies => "a network fetch brings this back",
            Self::Build => "a compile brings this back",
            Self::Cache => "this comes back on its own",
            Self::Noise => "nothing will miss this",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unrecoverable => "Unrecoverable",
            Self::Dependencies => "Dependencies",
            Self::Build => "Build Artifacts",
            Self::Cache => "Cache",
            Self::Noise => "Noise",
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
    fn the_vocabulary_is_ordered_by_what_it_costs_to_lose() {
        // The ordering is the thing worth more than the individual patterns: everything else
        // reads the cost axis off `ALL`, so this is where it is pinned. Unrecoverable is the
        // expensive end and noise is the cheap one, with the three regenerable kinds between
        // them in the order the design has always named them.
        assert_eq!(
            Kind::ALL.map(Kind::short),
            [
                "unrecoverable",
                "dependencies",
                "build",
                "cache",
                "noise"
            ]
        );
        assert!(Kind::Unrecoverable.cost() < Kind::Dependencies.cost());
        assert!(Kind::Dependencies.cost() < Kind::Build.cost());
        assert!(Kind::Build.cost() < Kind::Cache.cost());
        assert!(Kind::Cache.cost() < Kind::Noise.cost());
    }

    #[test]
    fn a_name_that_is_the_only_copy_of_something_is_unrecoverable() {
        for name in [
            ".env",
            ".env.local",
            "prod.env",
            ".env.production.local",
            "server.pem",
            "id_rsa",
            "id_ed25519",
            ".npmrc",
            "credentials",
        ] {
            assert_eq!(
                Kind::of_ignored_file(name),
                Some(Kind::Unrecoverable),
                "{name}"
            );
        }
    }

    #[test]
    fn a_name_nothing_will_miss_is_noise_whichever_way_the_system_spelled_it() {
        // Both systems that write these write them inconsistently, and the case is not
        // information — a `.DS_STORE` that read as "kind unknown" would be the same file
        // sorted somewhere else for no reason a reader could see.
        for name in [
            "build.log",
            ".DS_Store",
            ".ds_store",
            "Thumbs.db",
            "thumbs.db",
        ] {
            assert_eq!(Kind::of_ignored_file(name), Some(Kind::Noise), "{name}");
        }
    }

    #[test]
    fn a_name_that_could_be_read_either_way_is_read_as_the_expensive_one() {
        // `.env.log` matches both tables. Being wrong toward "noise" is the one mistake here
        // that cannot be undone by waiting for a rebuild, so the expensive end is asked first.
        assert_eq!(
            Kind::of_ignored_file(".env.log"),
            Some(Kind::Unrecoverable)
        );
    }

    #[test]
    fn a_name_that_says_nothing_gets_no_kind() {
        // The tier-two claim's own content, in a file's clothes: git knows it is disposable
        // and nothing knows what it is. Note `environment` and `logic`, which the substring
        // and suffix tests must not reach.
        for name in ["dump.sql", "environment", "logic", "scratch", "envoy.yaml"] {
            assert_eq!(Kind::of_ignored_file(name), None, "{name}");
        }
    }

    #[test]
    fn every_kind_says_what_losing_it_costs() {
        for kind in Kind::ALL {
            assert!(!kind.cost_said().is_empty(), "{kind}");
        }
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
