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
//! - **Kind** — the closed vocabulary #623 established and #652 extended at both ends, ordered
//!   by what it costs to lose: [`Kind::Unrecoverable`], [`Kind::Dependencies`], [`Kind::Build`],
//!   [`Kind::Cache`], [`Kind::Noise`]. "vendor" is a statement about this one.
//!
//! Modelled as four opaque modes, "show me every cache that a rule named" is not expressible
//! and never becomes expressible without a fifth mode. Modelled as two axes it already is, and
//! the presets are a convenience over the top rather than the vocabulary itself.
//!
//! # The presets are a path through the lattice, one axis per step
//!
//! `f`/`F` walk `default → dependencies → all-ignored → all`, and each step moves exactly one
//! axis while carrying the other forward — so `all-ignored` is "what I am looking at, **plus**
//! the gitignored tier" rather than a jump back to everything. That is what makes four names
//! four distinct views. See [`Preset`] for the table.
//!
//! # Expressible has to mean expressible BY A READER
//!
//! Two axes inside a struct that no key can move is a model with a claim it cannot cash. So the
//! presets are shortcuts and the axes have keys of their own: `t` moves the tier axis on its
//! own, and one key per [`Kind`] toggles that member of the other. Every combination of the two
//! is reachable — "every cache a rule named" is `f` to default, then `d` and `b` — and none of
//! them had to be anticipated as a mode.
//!
//! # `default` narrows, so the header says what it left out
//!
//! A run opens on [`Preset::Default`], which shows what rules named and hides the gitignore
//! fallback. That is a filter that is on without having been asked for, which is the shape the
//! age floor was resolved against — "silently keeps" is "silently deletes" seen from the other
//! side. What makes it honest rather than silent is that the count it hides is on the header,
//! beside the number it qualifies, from the first frame. See [`super::state::View::out_of_view`].
//!
//! # Gitignored FILES are a third axis, not a third tier
//!
//! A gitignored file is claimed by tier two, so the obvious home for it is a third value on the
//! tier axis — and that does not survive the presets. Every step of `f`'s cycle moves exactly
//! one axis, and `all` would then have to widen the tier axis *and* the kind axis in one step
//! to reach files. The `/` pattern already had this shape and was settled the same way: it
//! decides what is on screen, it is orthogonal to both axes, and no preset touches it.
//!
//! So files get an axis and a key of their own, `i`, and the request's actual sentence — that
//! ignored files be includable and excludable independently of ignored directories — falls
//! out of that rather than being arranged for. It starts **off**, because a real `~/repos`
//! holds tens of thousands of them; what makes that honest is the same thing that makes
//! `default` honest, a count of what is out of view on the header from the first frame.
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
    /// Which member of the tier axis judges this hit, or `None` when the **files** axis does.
    ///
    /// An `Option` rather than a third value, and that shape is the finding rather than a
    /// detail. A gitignored file is claimed by tier two, so a three-valued tier reads
    /// naturally — but it would then have to move when [`Preset::All`] widens, and every
    /// preset moves exactly one axis per step. Files are therefore orthogonal to both axes,
    /// exactly as the `/` pattern is, and this says so by declining to answer rather than by
    /// answering `Ignored` and leaving a caller to remember not to ask.
    ///
    /// It also no longer reads the *kind*, which it used to: a kind used to imply a rule, and
    /// since a gitignored file can carry one it does not.
    #[must_use]
    pub fn of(hit: &Hit) -> Option<Self> {
        if hit.is_ignored_file() {
            return None;
        }
        Some(match hit.rule() {
            Some(_) => Self::Named,
            None => Self::Ignored,
        })
    }
}

/// Which tiers are on screen. Both axes are sets, which is what makes an unanticipated
/// combination expressible without a new mode.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
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

    /// Every state this axis can be in, in the order `t` walks them.
    ///
    /// Three rather than four: the empty set shows no claims at all, which is not a view but a
    /// blank screen, so the key steps over it rather than offering it. Every *other* combination
    /// of the two axes is reachable, because the kind axis is toggled member by member.
    pub const ALL: [Self; 3] = [Self::named(), Self::both(), Self::ignored()];

    /// The next state of the axis.
    #[must_use]
    pub fn next(self) -> Self {
        let at = Self::ALL
            .iter()
            .position(|&other| other == self)
            .unwrap_or(0);
        Self::ALL[(at + 1) % Self::ALL.len()]
    }

    /// What the footer calls this state of the axis.
    #[must_use]
    pub fn label(self) -> &'static str {
        match (self.named, self.ignored) {
            (true, true) => "named + gitignored",
            (true, false) => "named",
            (false, true) => "gitignored",
            (false, false) => "no tier",
        }
    }
}

/// Which kinds are on screen.
///
/// Spelled as one named boolean per member rather than as a bitset, because the vocabulary is
/// closed and a member that arrived without being written out here is a member no key could
/// reach — which is #626's own finding, that a model whose expressiveness no interface exposes
/// has not been built yet.
// One bool per member of a closed vocabulary is the shape, not an accident of it: the lint is
// about a struct that has grown flags, and this is a set over five known things.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct Kinds {
    /// Things nothing brings back.
    pub unrecoverable: bool,
    /// Installed third-party code.
    pub dependencies: bool,
    /// Compiled output.
    pub build: bool,
    /// Regenerated automatically.
    pub cache: bool,
    /// Logs and the cruft an operating system leaves behind.
    pub noise: bool,
}

impl Kinds {
    /// Every kind.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            unrecoverable: true,
            dependencies: true,
            build: true,
            cache: true,
            noise: true,
        }
    }

    /// None of them, which is what a tier-two-only view wants: it is not narrowing the named
    /// claims, it is leaving them out.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            unrecoverable: false,
            dependencies: false,
            build: false,
            cache: false,
            noise: false,
        }
    }

    /// Just one.
    #[must_use]
    pub const fn only(kind: Kind) -> Self {
        Self {
            unrecoverable: matches!(kind, Kind::Unrecoverable),
            dependencies: matches!(kind, Kind::Dependencies),
            build: matches!(kind, Kind::Build),
            cache: matches!(kind, Kind::Cache),
            noise: matches!(kind, Kind::Noise),
        }
    }

    /// Whether this kind survives.
    #[must_use]
    pub const fn has(self, kind: Kind) -> bool {
        match kind {
            Kind::Unrecoverable => self.unrecoverable,
            Kind::Dependencies => self.dependencies,
            Kind::Build => self.build,
            Kind::Cache => self.cache,
            Kind::Noise => self.noise,
        }
    }

    /// The same set with one member turned on or off — one key, one axis member.
    #[must_use]
    pub const fn toggling(mut self, kind: Kind) -> Self {
        match kind {
            Kind::Unrecoverable => self.unrecoverable = !self.unrecoverable,
            Kind::Dependencies => self.dependencies = !self.dependencies,
            Kind::Build => self.build = !self.build,
            Kind::Cache => self.cache = !self.cache,
            Kind::Noise => self.noise = !self.noise,
        }
        self
    }

    /// The kinds on screen, named, or `none` when the axis is empty.
    ///
    /// The full set says `every kind` rather than listing five words, and that is the honest
    /// spelling as well as the short one: this line sits on a header and inside a confirmation
    /// warning, and an axis that is not narrowing anything should not take a line to say so.
    #[must_use]
    pub fn label(self) -> String {
        if self == Self::all() {
            return "every kind".to_owned();
        }
        let said: Vec<&str> = Kind::ALL
            .into_iter()
            .filter(|&kind| self.has(kind))
            .map(Kind::short)
            .collect();
        if said.is_empty() {
            return "none".to_owned();
        }
        said.join(" + ")
    }
}

/// A named point on the two axes, which is what one key cycles through.
///
/// # The cycle is a path through the lattice, one axis at a time
///
/// `default → dependencies → all-ignored → all`, and **each step moves exactly one axis while
/// retaining the other**:
///
/// | | tier | kind |
/// |---|---|---|
/// | `default` | named | every kind |
/// | `dependencies` | named | dependencies ← *kind narrows* |
/// | `all-ignored` | named + gitignored ← *tier widens* | dependencies |
/// | `all` | named + gitignored | every kind ← *kind widens* |
///
/// That is what makes them four distinct views rather than three wearing four names, and it is
/// what the asked-for order is *for*: read as a path, `all-ignored` is "the view I am on, plus
/// the gitignored tier", which is precisely the sentence the tier axis makes available.
///
/// An earlier pass mapped `all-ignored` and `all` to one point and told them apart by having
/// `all` clear the `/` pattern. Adversarial review rejected that and was right twice over: it
/// left a step of the cycle doing nothing to the axes, and it reached for the pattern — which is
/// **orthogonal to both axes** — to manufacture a difference. The pattern is never touched by a
/// preset now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Preset {
    /// What a rule could put a name to: tier one, every kind. Where a run starts.
    ///
    /// It hides the gitignored tier, so [`super::render`] says how many claims that is on the
    /// header — a view that narrows without saying what it dropped is the "silently keeps"
    /// failure the age floor was already resolved against.
    #[default]
    Default,
    /// Installed third-party code, and only that: npkill's `vendor`. The kind axis narrowed,
    /// the tier axis exactly as `default` had it.
    Dependencies,
    /// The step before it, **and also** the gitignore fallback: the tier axis widened, the kind
    /// narrowing retained.
    AllIgnored,
    /// Everything the scan found: both tiers, every kind.
    All,
}

impl Preset {
    /// Every preset, in the order `f` walks them — the order the request names them in.
    pub const ALL: [Self; 4] = [
        Self::Default,
        Self::Dependencies,
        Self::AllIgnored,
        Self::All,
    ];

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
            Self::Default => "default",
            Self::Dependencies => "dependencies",
            Self::AllIgnored => "all-ignored",
            Self::All => "all",
        }
    }

    /// The sentence the footer says when the view changes, which has to name what is now
    /// *missing*: a reader who cannot see a claim has no way to notice it was hidden rather
    /// than never found.
    #[must_use]
    pub fn what(self) -> &'static str {
        match self {
            Self::Default => "only what a rule named — the gitignored tier is hidden",
            Self::Dependencies => "only installed dependencies a rule named",
            Self::AllIgnored => "installed dependencies, and the gitignored tier beside them",
            // Not "everything the scan found", which it was and no longer is: gitignored files
            // are an axis no preset touches, so this is everything on the two axes it moves.
            // A view that overstated itself here would be the "silently keeps" failure wearing
            // the label of its opposite.
            Self::All => "every directory the scan found — `i` adds gitignored files",
        }
    }

    /// Where this preset sits on the two axes.
    ///
    /// One axis moves per step and the other is retained — see the type's own docs for the
    /// table, and note that no preset touches the `/` pattern.
    #[must_use]
    pub fn axes(self) -> (Tiers, Kinds) {
        match self {
            Self::Default => (Tiers::named(), Kinds::all()),
            Self::Dependencies => (Tiers::named(), Kinds::only(Kind::Dependencies)),
            Self::AllIgnored => (Tiers::both(), Kinds::only(Kind::Dependencies)),
            Self::All => (Tiers::both(), Kinds::all()),
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
    /// Whether gitignored **files** are on screen. See [`Lens::files`].
    files: bool,
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
            && self.files == other.files
            && self.pattern.as_ref().map(Regex::as_str) == other.pattern.as_ref().map(Regex::as_str)
    }
}

impl Eq for Lens {}

impl std::hash::Hash for Lens {
    /// Exactly what [`Lens::eq`] reads, in the same order, because a hash that disagreed with
    /// equality is a lens that changed without anybody watching it being told — and
    /// [`super::treemap`] watches this one to decide whether a picture is still the right
    /// one. The pattern goes in as its source text for equality's own reason.
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.tiers.hash(state);
        self.kinds.hash(state);
        self.files.hash(state);
        self.pattern.as_ref().map(Regex::as_str).hash(state);
    }
}

impl Lens {
    /// A lens on the two axes directly, which is the general door: [`Lens::showing`] is the
    /// preset shorthand over it.
    #[must_use]
    pub fn of(tiers: Tiers, kinds: Kinds) -> Self {
        Self {
            tiers,
            kinds,
            // Off, which is the request's own answer and the header's job to declare. A real
            // `~/repos` holds tens of thousands of gitignored files, and a sweep that showed
            // them unasked would bury the 40 GB `node_modules` this tool exists to find under
            // `.DS_Store` rows.
            files: false,
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

    /// Which tiers are on screen.
    #[must_use]
    pub fn tiers(&self) -> Tiers {
        self.tiers
    }

    /// Which kinds are.
    #[must_use]
    pub fn kinds(&self) -> Kinds {
        self.kinds
    }

    /// The same lens with the tier axis moved, and the kind axis untouched.
    ///
    /// The two editors exist so that the axes are independently reachable **by a reader**, not
    /// only inside this file: a model that can express "every cache a rule named" while no key
    /// can reach it is not expressible in any sense the request meant.
    #[must_use]
    pub fn with_tiers(mut self, tiers: Tiers) -> Self {
        self.tiers = tiers;
        self
    }

    /// The same lens with the kind axis moved, and the tier axis untouched.
    #[must_use]
    pub fn with_kinds(mut self, kinds: Kinds) -> Self {
        self.kinds = kinds;
        self
    }

    /// Whether gitignored files are on screen.
    ///
    /// # Why an axis of its own rather than a third tier
    ///
    /// A gitignored file *is* a tier-two claim, so a third value on the tier axis is the first
    /// idea and it does not survive the presets. Each step of `f`'s cycle moves exactly one
    /// axis, and `all` — "everything the scan found" — would have to widen the tier axis and
    /// the kind axis together to reach files. The `/` pattern already had this shape and was
    /// resolved the same way: it decides what is on screen, it is orthogonal to both axes, and
    /// **no preset touches it**. Files are the same, which also means turning them on is not
    /// undone by cycling the view.
    ///
    /// It is independently reachable, which is what the request asked for in so many words:
    /// `i` toggles this and nothing else, so gitignored files come and go without disturbing
    /// gitignored directories.
    #[must_use]
    pub fn files(&self) -> bool {
        self.files
    }

    /// The same lens showing, or not showing, gitignored files. Both other axes untouched.
    #[must_use]
    pub fn with_files(mut self, files: bool) -> Self {
        self.files = files;
        self
    }

    /// Which preset this lens sits on, if it sits on one.
    ///
    /// Unambiguous, because the four presets occupy four distinct points — which is what the
    /// footer needs in order to name a view honestly, and it is why nothing has to remember
    /// which step of the cycle the reader took to get here. A lens the axis keys built lands
    /// off all four and says so.
    ///
    /// The pattern is deliberately not consulted, and neither is the files axis: both narrow
    /// or widen *whatever the two axes left*, so `dependencies` with a pattern over it and
    /// files showing beside it is still `dependencies`.
    #[must_use]
    pub fn preset(&self) -> Option<Preset> {
        Preset::ALL
            .into_iter()
            .find(|preset| preset.axes() == (self.tiers, self.kinds))
    }

    /// How the footer spells the axes when the reader has moved off every preset.
    ///
    /// The files axis is named only when it is *on*, and that asymmetry is deliberate: off is
    /// where every view starts, so saying it everywhere would spend a third of the line on the
    /// absence of something. When it is on it changes what a row means, so it is said.
    #[must_use]
    pub fn axes_label(&self) -> String {
        let axes = format!("{} · {}", self.tiers.label(), self.kinds.label());
        if self.files {
            format!("{axes} · files")
        } else {
            axes
        }
    }

    /// The pattern in force, if any.
    #[must_use]
    pub fn pattern(&self) -> Option<&str> {
        self.pattern.as_ref().map(Regex::as_str)
    }

    /// Whether this lens hides nothing at all, which is the fast path the whole front end
    /// takes when a reader has not narrowed anything.
    ///
    /// Note that no preset reaches it: `all` widens both axes and leaves files where the
    /// reader put them, so a run that has never pressed `i` is narrowed however far `f` has
    /// been cycled. That is what the header's out-of-view count is for.
    #[must_use]
    pub fn is_everything(&self) -> bool {
        self.tiers == Tiers::both()
            && self.kinds == Kinds::all()
            && self.files
            && self.pattern.is_none()
    }

    /// Whether this claim is on screen.
    ///
    /// The three axes are visibly independent here, which is the whole reason for the shape.
    /// **Where a claim came from** decides one of them — a rule, the gitignore fallback, or the
    /// fallback on a file — and **what it is** decides the kind axis, whoever found it. That
    /// second half is what lets `.env` files be narrowed to on their own without a mode
    /// anybody had to anticipate, and it leaves the tier-two *directory* judged by the tier
    /// axis and never by the kind axis, since it has no kind to judge.
    #[must_use]
    pub fn matches(&self, hit: &Hit) -> bool {
        let by_source = match Tier::of(hit) {
            Some(tier) => self.tiers.has(tier),
            None => self.files,
        };
        let by_kind = hit.kind().is_none_or(|kind| self.kinds.has(kind));
        by_source && by_kind && self.says_yes_to(&hit.path.to_string_lossy())
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
        // The axes rather than a preset name, because this sentence has to be actionable: a
        // reader told "hidden by all-ignored" still has to work out what that leaves out, and a
        // reader told "named · dependencies + cache" does not.
        let view = self.axes_label();
        match self.pattern() {
            Some(pattern) => format!("{view} · /{pattern}"),
            None => view,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Kinds, Lens, Preset, Tier, Tiers};
    use crate::fixture::{gitignored, gitignored_file, of_kind};
    use crate::rules::Kind;
    use regex::Regex;

    /// One claim of each kind, plus one only git knows about.
    fn claims() -> (
        crate::walk::Hit,
        crate::walk::Hit,
        crate::walk::Hit,
        crate::walk::Hit,
    ) {
        (
            of_kind("/scan/a/node_modules", Kind::Dependencies),
            of_kind("/scan/a/dist", Kind::Build),
            of_kind("/scan/a/.nx/cache", Kind::Cache),
            gitignored("/scan/a/out"),
        )
    }

    #[test]
    fn the_presets_are_the_four_that_were_asked_for_in_the_order_they_were_asked_for() {
        assert_eq!(
            Preset::ALL.map(Preset::label),
            ["default", "dependencies", "all-ignored", "all"]
        );
        let (deps, build, cache, ignored) = claims();
        let shows = |preset: Preset| {
            let lens = Lens::showing(preset);
            [
                lens.matches(&deps),
                lens.matches(&build),
                lens.matches(&cache),
                lens.matches(&ignored),
            ]
        };

        // default: everything a rule could put a name to, and not the fallback tier.
        assert_eq!(shows(Preset::Default), [true, true, true, false]);
        // dependencies: the kind axis narrowed, the tier axis as default had it.
        assert_eq!(shows(Preset::Dependencies), [true, false, false, false]);
        // all-ignored: the tier axis widened, and the kind narrowing RETAINED — the fallback
        // tier alongside what the previous step was showing rather than instead of it.
        assert_eq!(shows(Preset::AllIgnored), [true, false, false, true]);
        // all: the kind axis widened too, which is everything.
        assert_eq!(shows(Preset::All), [true, true, true, true]);
    }

    #[test]
    fn the_four_presets_are_four_distinct_points_on_the_two_axes() {
        // Four names for three points would leave a step of the cycle doing nothing, and an
        // earlier pass had exactly that — told apart by clearing the `/` pattern, which is
        // orthogonal to both axes and so cannot carry a difference between two views.
        for (nth, preset) in Preset::ALL.into_iter().enumerate() {
            for other in Preset::ALL.into_iter().skip(nth + 1) {
                assert_ne!(preset.axes(), other.axes(), "{preset} and {other}");
            }
        }
    }

    #[test]
    fn every_step_of_the_cycle_moves_exactly_one_axis() {
        // What makes the asked-for order a *path* rather than four unrelated points: each key
        // press is one sentence about one axis, and the other is carried forward. That is what
        // lets `all-ignored` mean "what I am looking at, plus the gitignored tier".
        let mut at = Preset::Default;
        for _ in 1..Preset::ALL.len() {
            let next = at.next();
            let (tiers, kinds) = at.axes();
            let (moved_tiers, moved_kinds) = next.axes();
            assert_ne!(
                (tiers == moved_tiers, kinds == moved_kinds),
                (true, true),
                "{at} → {next} moved nothing"
            );
            assert!(
                (tiers == moved_tiers) || (kinds == moved_kinds),
                "{at} → {next} moved both axes at once"
            );
            at = next;
        }
    }

    #[test]
    fn a_preset_never_touches_the_pattern() {
        // The pattern narrows whatever the axes leave, so it is orthogonal to both of them and
        // no preset gets to mean anything by it. `dependencies` with a pattern over it is still
        // `dependencies`.
        let pattern = Regex::new("nx").expect("a literal pattern compiles");
        for preset in Preset::ALL {
            let lens = Lens::showing(preset).matching(Some(pattern.clone()));
            assert_eq!(lens.pattern(), Some("nx"), "{preset}");
            assert_eq!(lens.preset(), Some(preset), "{preset}");
        }
    }

    #[test]
    fn the_cycle_comes_back_round_and_goes_both_ways() {
        let mut at = Preset::Default;
        for _ in Preset::ALL {
            at = at.next();
        }
        assert_eq!(at, Preset::Default);
        assert_eq!(Preset::Default.prev(), Preset::All);
        assert_eq!(Preset::All.next(), Preset::Default);
    }

    #[test]
    fn the_axes_are_independent_of_each_other() {
        // The property the whole model exists for, and the one the presets alone cannot give:
        // narrowing the kind axis says nothing about the tier axis, so "every cache a rule
        // named" and "every cache, plus the gitignored tier" are both expressible without
        // either being a mode somebody had to anticipate. Neither is a preset.
        let caches = Lens::of(Tiers::named(), Kinds::only(Kind::Cache));
        let caches_and_ignored = Lens::of(Tiers::both(), Kinds::only(Kind::Cache));
        assert_eq!(caches.preset(), None);
        assert_eq!(caches_and_ignored.preset(), None);

        let (deps, _build, cache, ignored) = claims();

        assert!(caches.matches(&cache));
        assert!(!caches.matches(&deps));
        assert!(!caches.matches(&ignored));

        assert!(caches_and_ignored.matches(&cache));
        assert!(!caches_and_ignored.matches(&deps));
        assert!(caches_and_ignored.matches(&ignored));
    }

    #[test]
    fn each_axis_moves_without_disturbing_the_other() {
        // What the two editing keys are built on. Moving the tier axis must leave the kinds
        // exactly as they were and the other way round, or "independent" is a word rather than
        // a property.
        let start = Lens::of(Tiers::named(), Kinds::only(Kind::Cache));

        let widened = start.clone().with_tiers(Tiers::both());
        assert_eq!(widened.kinds(), start.kinds());
        assert_eq!(widened.tiers(), Tiers::both());

        let narrowed = start
            .clone()
            .with_kinds(start.kinds().toggling(Kind::Build));
        assert_eq!(narrowed.tiers(), start.tiers());
        assert!(narrowed.kinds().has(Kind::Build));
        assert!(narrowed.kinds().has(Kind::Cache));
        assert!(!narrowed.kinds().has(Kind::Dependencies));
    }

    #[test]
    fn the_tier_axis_walks_its_three_states_and_never_the_empty_one() {
        // An empty tier set shows no claims at all, which is a blank screen rather than a
        // view, so the key steps over it. Every other combination stays reachable, because the
        // kind axis is toggled member by member rather than cycled.
        let mut at = Tiers::named();
        let mut seen = Vec::new();
        for _ in Tiers::ALL {
            seen.push(at);
            at = at.next();
        }
        assert_eq!(seen, Tiers::ALL);
        assert_eq!(at, Tiers::named(), "the cycle does not come back round");
        for tiers in Tiers::ALL {
            assert!(tiers.named || tiers.ignored, "{tiers:?} shows nothing");
        }
    }

    #[test]
    fn a_tier_two_claim_is_judged_by_the_tier_axis_and_never_by_the_kind_axis() {
        // It has no kind to judge — that asymmetry is the tier's whole content — so narrowing
        // the kinds must not be able to hide it by accident, and widening them must not be able
        // to bring it back.
        let ignored = gitignored("/scan/a/out");
        let deps = of_kind("/scan/a/node_modules", Kind::Dependencies);

        let no_kinds_at_all = Lens::of(Tiers::both(), Kinds::none());
        assert!(no_kinds_at_all.matches(&ignored));
        assert!(!no_kinds_at_all.matches(&deps));

        let every_kind_but_no_fallback = Lens::of(Tiers::named(), Kinds::all());
        assert!(!every_kind_but_no_fallback.matches(&ignored));
        assert!(every_kind_but_no_fallback.matches(&deps));
    }

    #[test]
    fn a_pattern_narrows_whatever_the_axes_left() {
        let lens = Lens::showing(Preset::All)
            .matching(Some(Regex::new("nx").expect("a literal pattern compiles")));
        assert!(lens.matches(&gitignored("/scan/nx/dist")));
        assert!(!lens.matches(&gitignored("/scan/pua/dist")));
        assert!(!lens.is_everything());
        assert_eq!(lens.describe(), "named + gitignored · every kind · /nx");
    }

    #[test]
    fn two_lenses_are_the_same_when_they_show_the_same_things() {
        // Marks are deduplicated by lens, so this is load-bearing rather than a formality: a
        // lens that never compares equal to itself would leave a mark per keystroke.
        let one = Lens::showing(Preset::Default)
            .matching(Some(Regex::new("nx").expect("a literal pattern compiles")));
        let two = Lens::showing(Preset::Default)
            .matching(Some(Regex::new("nx").expect("a literal pattern compiles")));
        assert_eq!(one, two);
        assert_ne!(one, Lens::showing(Preset::Default));
    }

    #[test]
    fn a_hits_tier_is_read_off_whether_a_rule_named_it_and_never_off_its_kind() {
        // It used to be read off the kind, and it cannot be any more: a gitignored FILE
        // carries one. Reading the claim is what keeps `.env` out of the *named* tier while
        // still letting the kind axis narrow to it.
        assert_eq!(
            Tier::of(&of_kind("/scan/a/target", Kind::Build)),
            Some(Tier::Named)
        );
        assert_eq!(Tier::of(&gitignored("/scan/a/dist")), Some(Tier::Ignored));
        assert_eq!(
            Tier::of(&gitignored_file("/scan/a/.env", Some(Kind::Unrecoverable))),
            None,
            "a file is judged by the files axis, and the tier axis declines to answer"
        );
    }

    #[test]
    fn a_gitignored_file_is_off_screen_until_its_own_key_says_otherwise() {
        // The request's whole sentence, as one assertion: includable and excludable
        // independently of ignored directories. A real `~/repos` holds tens of thousands of
        // these, so no preset may drag them in and none may push them out.
        let env = gitignored_file("/scan/a/.env", Some(Kind::Unrecoverable));
        let dir = gitignored("/scan/a/dist");

        for preset in Preset::ALL {
            assert!(
                !Lens::showing(preset).matches(&env),
                "{preset} showed a gitignored file"
            );
        }

        let showing = Lens::showing(Preset::Default).with_files(true);
        assert!(showing.matches(&env));
        assert!(
            !showing.matches(&dir),
            "turning files on must not turn the gitignored TIER on"
        );

        let tier_only = Lens::of(Tiers::ignored(), Kinds::none());
        assert!(tier_only.matches(&dir));
        assert!(
            !tier_only.matches(&env),
            "turning the gitignored tier on must not turn files on"
        );
    }

    #[test]
    fn the_files_axis_is_carried_through_every_preset_the_way_the_pattern_is() {
        // Orthogonal means orthogonal: `f` moves the two axes and leaves this alone, exactly
        // as it leaves the `/` pattern alone. Anything else and turning files on would be
        // undone by a keystroke about something else.
        let showing = Lens::showing(Preset::Default).with_files(true);
        assert!(showing.files());
        for preset in Preset::ALL {
            let cycled = Lens::showing(preset).with_files(true);
            assert!(cycled.files(), "{preset}");
            assert_eq!(cycled.preset(), Some(preset), "{preset}");
        }
    }

    #[test]
    fn the_kind_axis_judges_a_file_because_a_file_has_a_kind() {
        // What the vocabulary's two new members are *for*: "show me every unrecoverable file"
        // is a question the axes can already answer, without a mode anybody had to anticipate.
        let env = gitignored_file("/scan/a/.env", Some(Kind::Unrecoverable));
        let log = gitignored_file("/scan/a/build.log", Some(Kind::Noise));
        let scratch = gitignored_file("/scan/a/dump.sql", None);

        let precious = Lens::of(Tiers::named(), Kinds::only(Kind::Unrecoverable)).with_files(true);
        assert!(precious.matches(&env));
        assert!(!precious.matches(&log));
        assert!(
            precious.matches(&scratch),
            "a file with no kind has nothing for the kind axis to refuse, exactly as a \
             tier-two directory does not"
        );
    }

    #[test]
    fn nothing_is_everything_until_the_files_axis_is_on_too() {
        // The fast path has to mean what it says. `all` is not everything any more, because
        // an axis it does not touch is still narrowing.
        assert!(!Lens::showing(Preset::All).is_everything());
        assert!(Lens::showing(Preset::All).with_files(true).is_everything());
    }

    #[test]
    fn the_footer_says_the_files_axis_only_when_it_is_on() {
        let off = Lens::of(Tiers::named(), Kinds::only(Kind::Cache));
        assert_eq!(off.axes_label(), "named · cache");
        assert_eq!(off.with_files(true).axes_label(), "named · cache · files");
    }
}
