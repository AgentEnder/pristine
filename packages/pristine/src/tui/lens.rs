//! What is on screen: two independent axes, with presets on top.
//!
//! # Two axes, not four modes
//!
//! The set of views a reader asked for was "default, all-ignored, vendor, all", and those four
//! names mix two things that vary independently:
//!
//! - **Tier** — whether a claim is one a *rule* named ([`Tier::Named`]) or one only the
//!   gitignore fallback found ([`Tier::Ignored`]). "all-ignored" is a statement about this axis
//!   and nothing else.
//! - **Kind** — the closed vocabulary #623 established: [`Kind::Dependencies`],
//!   [`Kind::Build`], [`Kind::Cache`]. "vendor" is a statement about this one.
//!
//! Modelled as four opaque modes, "show me every cache that a rule named" is not expressible
//! and never becomes expressible without a fifth mode. Modelled as two axes it already is, and
//! the presets are a convenience over the top rather than the vocabulary itself.
//!
//! # There are three preset states wearing four names
//!
//! The asked-for cycle was `default → dependencies → all-ignored → all`, and separating the
//! axes is what shows that `default` and `all` are the same view: a filter that is **on**
//! without having been asked for is the failure the age floor was already resolved against —
//! "silently keeps" is "silently deletes" seen from the other side, and both make the screen
//! disagree with what the reader asked for. So the cycle starts at [`Preset::All`], which hides
//! nothing, and the fourth distinct state is the one the original list left implicit:
//! [`Preset::Named`], everything a rule could put a name to.
//!
//! # The pattern is part of the lens
//!
//! `/`'s regex is a third thing that decides whether a claim is on screen, so it lives here
//! rather than beside here. That matters because a **mark stores the lens it was made
//! through** ([`super::state::View`]), and a mark made under `/nx` has to keep meaning
//! `/nx` when the pattern is cleared, exactly as a mark made under Dependencies keeps meaning
//! Dependencies.

use std::fmt;

use regex::Regex;

use crate::rules::Kind;
use crate::walk::Hit;

/// Which tier claimed a directory.
///
/// The asymmetry is information rather than an accident of implementation: a named row is a
/// directory whose cost to lose is known, and an unnamed one is a leap. That is worth being
/// able to filter on in both directions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// A rule named it, so it carries an ecosystem and a [`Kind`].
    Named,
    /// Only the gitignore fallback found it. Nothing knows what it is.
    Ignored,
}

impl Tier {
    /// Which tier claimed this hit.
    #[must_use]
    pub fn of(hit: &Hit) -> Self {
        match hit.kind() {
            Some(_) => Self::Named,
            None => Self::Ignored,
        }
    }
}

/// Which tiers are on screen. Both axes are sets, which is what makes an unanticipated
/// combination expressible without a new mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tiers {
    /// Whether rule-named claims are shown.
    pub named: bool,
    /// Whether gitignore-fallback claims are shown.
    pub ignored: bool,
}

impl Tiers {
    /// Both.
    #[must_use]
    pub const fn both() -> Self {
        Self {
            named: true,
            ignored: true,
        }
    }

    /// Only what a rule named.
    #[must_use]
    pub const fn named() -> Self {
        Self {
            named: true,
            ignored: false,
        }
    }

    /// Only what the gitignore fallback found.
    #[must_use]
    pub const fn ignored() -> Self {
        Self {
            named: false,
            ignored: true,
        }
    }

    /// Whether this tier survives.
    #[must_use]
    pub const fn has(self, tier: Tier) -> bool {
        match tier {
            Tier::Named => self.named,
            Tier::Ignored => self.ignored,
        }
    }
}

/// Which kinds are on screen.
///
/// Spelled as three named booleans rather than as a bitset, because there are exactly three of
/// them and a closed vocabulary is the one place where writing the members out is shorter than
/// the machinery for not writing them out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kinds {
    /// Installed third-party code.
    pub dependencies: bool,
    /// Compiled output.
    pub build: bool,
    /// Regenerated automatically.
    pub cache: bool,
}

impl Kinds {
    /// Every kind.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            dependencies: true,
            build: true,
            cache: true,
        }
    }

    /// None of them, which is what a tier-two-only view wants: it is not narrowing the named
    /// claims, it is leaving them out.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            dependencies: false,
            build: false,
            cache: false,
        }
    }

    /// Just one.
    #[must_use]
    pub const fn only(kind: Kind) -> Self {
        Self {
            dependencies: matches!(kind, Kind::Dependencies),
            build: matches!(kind, Kind::Build),
            cache: matches!(kind, Kind::Cache),
        }
    }

    /// Whether this kind survives.
    #[must_use]
    pub const fn has(self, kind: Kind) -> bool {
        match kind {
            Kind::Dependencies => self.dependencies,
            Kind::Build => self.build,
            Kind::Cache => self.cache,
        }
    }
}

/// A named point on the two axes, which is what a key cycles through.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Preset {
    /// Everything the scan found. Where a run starts, because a view that is narrowed without
    /// having been asked for is a tool disagreeing with its own report.
    #[default]
    All,
    /// Every claim a rule could put a name to, and none of the gitignore fallback's.
    Named,
    /// Installed third-party code, and only that: npkill's `vendor`.
    Dependencies,
    /// The gitignore fallback on its own — the tier no other tool has, and the one whose rows
    /// say only that git knows about them.
    Ignored,
}

impl Preset {
    /// Every preset, in the order the key cycles through them: widest first, then narrowing.
    pub const ALL: [Self; 4] = [Self::All, Self::Named, Self::Dependencies, Self::Ignored];

    /// The next one round.
    #[must_use]
    pub fn next(self) -> Self {
        Self::step(self, 1)
    }

    /// The one before.
    #[must_use]
    pub fn prev(self) -> Self {
        Self::step(self, Self::ALL.len() - 1)
    }

    fn step(self, by: usize) -> Self {
        let at = Self::ALL
            .iter()
            .position(|&other| other == self)
            .unwrap_or(0);
        Self::ALL[(at + by) % Self::ALL.len()]
    }

    /// What the footer calls this view.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Named => "named",
            Self::Dependencies => "dependencies",
            Self::Ignored => "gitignored",
        }
    }

    /// The sentence the footer says when the view changes, which has to name what is now
    /// *missing*: a reader who cannot see a claim has no way to notice it was hidden rather
    /// than never found.
    #[must_use]
    pub fn what(self) -> &'static str {
        match self {
            Self::All => "everything the scan found",
            Self::Named => "only what a rule named — the gitignored tier is hidden",
            Self::Dependencies => "only installed dependencies",
            Self::Ignored => "only the gitignored tier, which nothing has named",
        }
    }

    /// Where this preset sits on the two axes.
    #[must_use]
    pub fn axes(self) -> (Tiers, Kinds) {
        match self {
            Self::All => (Tiers::both(), Kinds::all()),
            Self::Named => (Tiers::named(), Kinds::all()),
            Self::Dependencies => (Tiers::named(), Kinds::only(Kind::Dependencies)),
            Self::Ignored => (Tiers::ignored(), Kinds::none()),
        }
    }
}

impl fmt::Display for Preset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Everything that decides whether a claim is on screen.
///
/// Cloned into every mark, which is why it is a value rather than a handle into the view: a
/// mark has to keep meaning what the reader could see when they made it, and a mark holding a
/// reference to the *current* view would mean the opposite of that.
#[derive(Clone, Debug)]
pub struct Lens {
    tiers: Tiers,
    kinds: Kinds,
    /// The `/` prompt's regex over the whole path, when there is one.
    pattern: Option<Regex>,
}

impl Default for Lens {
    fn default() -> Self {
        Self::showing(Preset::default())
    }
}

impl PartialEq for Lens {
    /// By what it shows. [`Regex`] has no equality of its own, and the pattern is the only
    /// honest stand-in — two engines compiled from one string accept the same paths.
    fn eq(&self, other: &Self) -> bool {
        self.tiers == other.tiers
            && self.kinds == other.kinds
            && self.pattern.as_ref().map(Regex::as_str) == other.pattern.as_ref().map(Regex::as_str)
    }
}

impl Eq for Lens {}

impl Lens {
    /// A lens on the two axes directly, which is the general door: [`Lens::showing`] is the
    /// preset shorthand over it.
    #[must_use]
    pub fn of(tiers: Tiers, kinds: Kinds) -> Self {
        Self {
            tiers,
            kinds,
            pattern: None,
        }
    }

    /// The lens a preset names.
    #[must_use]
    pub fn showing(preset: Preset) -> Self {
        let (tiers, kinds) = preset.axes();
        Self::of(tiers, kinds)
    }

    /// The same lens with `pattern` over it, or with none.
    #[must_use]
    pub fn matching(mut self, pattern: Option<Regex>) -> Self {
        self.pattern = pattern;
        self
    }

    /// Which preset this lens is, if it is one of them.
    ///
    /// Derived rather than remembered, so a lens that arrived by any other door still names
    /// itself the way the footer names it, and there is one statement of where each preset
    /// sits.
    #[must_use]
    pub fn preset(&self) -> Option<Preset> {
        Preset::ALL
            .into_iter()
            .find(|preset| preset.axes() == (self.tiers, self.kinds))
    }

    /// The pattern in force, if any.
    #[must_use]
    pub fn pattern(&self) -> Option<&str> {
        self.pattern.as_ref().map(Regex::as_str)
    }

    /// Whether this lens hides nothing at all, which is the fast path the whole front end
    /// takes when a reader has not narrowed anything.
    #[must_use]
    pub fn is_everything(&self) -> bool {
        self.tiers == Tiers::both() && self.kinds == Kinds::all() && self.pattern.is_none()
    }

    /// Whether this claim is on screen.
    ///
    /// The two axes are visibly independent here, which is the whole reason for the shape: a
    /// hit's kind decides which axis judges it, and neither axis knows about the other.
    #[must_use]
    pub fn matches(&self, hit: &Hit) -> bool {
        let by_axes = match hit.kind() {
            Some(kind) => self.tiers.has(Tier::Named) && self.kinds.has(kind),
            None => self.tiers.has(Tier::Ignored),
        };
        by_axes && self.says_yes_to(&hit.path.to_string_lossy())
    }

    /// Whether the pattern, if there is one, accepts this path.
    fn says_yes_to(&self, path: &str) -> bool {
        self.pattern
            .as_ref()
            .is_none_or(|pattern| pattern.is_match(path))
    }

    /// How the confirmation names this lens, when it has to say what a hidden entry is hidden
    /// *by*. Both halves, because either can be the one doing the hiding.
    #[must_use]
    pub fn describe(&self) -> String {
        let view = self
            .preset()
            .map_or_else(|| "a custom view".to_owned(), |preset| preset.to_string());
        match self.pattern() {
            Some(pattern) => format!("{view} · /{pattern}"),
            None => view,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Kinds, Lens, Preset, Tier, Tiers};
    use crate::fixture::{gitignored, of_kind};
    use crate::rules::Kind;
    use regex::Regex;

    #[test]
    fn the_axes_are_independent_of_each_other() {
        // The property the whole model exists for: narrowing the kind axis says nothing about
        // the tier axis, so "every cache a rule named" and "every cache, plus the gitignored
        // tier" are both expressible without either being a mode somebody had to anticipate.
        let caches = Lens::of(Tiers::named(), Kinds::only(Kind::Cache));
        let caches_and_ignored = Lens::of(Tiers::both(), Kinds::only(Kind::Cache));

        let cache = of_kind("/scan/a/.nx/cache", Kind::Cache);
        let deps = of_kind("/scan/a/node_modules", Kind::Dependencies);
        let ignored = gitignored("/scan/a/dist");

        assert!(caches.matches(&cache));
        assert!(!caches.matches(&deps));
        assert!(!caches.matches(&ignored));

        assert!(caches_and_ignored.matches(&cache));
        assert!(!caches_and_ignored.matches(&deps));
        assert!(caches_and_ignored.matches(&ignored));
    }

    #[test]
    fn a_tier_two_claim_is_judged_by_the_tier_axis_and_never_by_the_kind_axis() {
        // It has no kind to judge — that asymmetry is the tier's whole content — so the kind
        // axis must not be able to hide it by accident. `Preset::Ignored` narrows the kinds to
        // nothing precisely because kinds are not what it is about.
        let ignored = gitignored("/scan/a/dist");
        assert!(Lens::showing(Preset::Ignored).matches(&ignored));
        assert!(
            !Lens::showing(Preset::Ignored)
                .matches(&of_kind("/scan/a/node_modules", Kind::Dependencies))
        );
    }

    #[test]
    fn the_default_preset_hides_nothing() {
        // A filter that is on without being asked for is the failure the age floor was already
        // resolved against. The reader's first frame has to be the whole scan.
        let lens = Lens::default();
        assert!(lens.is_everything());
        assert_eq!(lens.preset(), Some(Preset::All));
        for kind in [Kind::Dependencies, Kind::Build, Kind::Cache] {
            assert!(lens.matches(&of_kind("/scan/a/x", kind)), "{kind}");
        }
        assert!(lens.matches(&gitignored("/scan/a/x")));
    }

    #[test]
    fn every_preset_is_a_view_no_other_preset_is() {
        // Four names for three states was the flaw in the asked-for set, and it is the one
        // thing separating the axes was supposed to expose. If two presets ever collapse onto
        // one point again, the cycle has a step that does nothing.
        for (nth, preset) in Preset::ALL.into_iter().enumerate() {
            for other in Preset::ALL.into_iter().skip(nth + 1) {
                assert_ne!(preset.axes(), other.axes(), "{preset} and {other}");
            }
        }
    }

    #[test]
    fn the_cycle_comes_back_round_and_goes_both_ways() {
        let mut at = Preset::All;
        for _ in Preset::ALL {
            at = at.next();
        }
        assert_eq!(at, Preset::All);
        assert_eq!(Preset::All.prev(), Preset::Ignored);
        assert_eq!(Preset::Ignored.next(), Preset::All);
    }

    #[test]
    fn a_pattern_narrows_whatever_the_axes_left() {
        let lens = Lens::showing(Preset::All)
            .matching(Some(Regex::new("nx").expect("a literal pattern compiles")));
        assert!(lens.matches(&gitignored("/scan/nx/dist")));
        assert!(!lens.matches(&gitignored("/scan/pua/dist")));
        assert!(!lens.is_everything());
        // …and the preset it is built on is still what the footer names, because the pattern
        // is a third narrowing rather than a fifth mode.
        assert_eq!(lens.preset(), Some(Preset::All));
        assert_eq!(lens.describe(), "all · /nx");
    }

    #[test]
    fn two_lenses_are_the_same_when_they_show_the_same_things() {
        // Marks are deduplicated by lens, so this is load-bearing rather than a formality: a
        // lens that never compares equal to itself would leave a mark per keystroke.
        let one = Lens::showing(Preset::Named)
            .matching(Some(Regex::new("nx").expect("a literal pattern compiles")));
        let two = Lens::showing(Preset::Named)
            .matching(Some(Regex::new("nx").expect("a literal pattern compiles")));
        assert_eq!(one, two);
        assert_ne!(one, Lens::showing(Preset::Named));
    }

    #[test]
    fn a_hits_tier_is_read_off_whether_anything_named_it() {
        assert_eq!(
            Tier::of(&of_kind("/scan/a/target", Kind::Build)),
            Tier::Named
        );
        assert_eq!(Tier::of(&gitignored("/scan/a/dist")), Tier::Ignored);
    }
}
