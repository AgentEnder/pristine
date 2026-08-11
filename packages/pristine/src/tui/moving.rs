//! What is moving on the screen right now, and why each thing is allowed to.
//!
//! # Motion is information wearing a costume
//!
//! Nothing here spins to prove the program is running. Every moving thing below is a fact the
//! view already holds, drawn *over time* rather than all at once, so the rate of a change is
//! legible as well as its result:
//!
//! - a rolled-up total that climbs shows the rate claims are arriving at, which is the one
//!   thing a count of directories cannot say;
//! - a row that is lit is a directory the walk found since the last frame;
//! - a shimmer through a dash is a claim a pricing thread is inside **at this instant** — the
//!   pool has N threads, so exactly N rows shimmer, and that is honest rather than decorative;
//! - a mark running up the ancestors is the subtree operation that just happened, shown
//!   instead of inferred;
//! - a row emptying is bytes leaving the disk.
//!
//! The rule that follows from it, and the one worth keeping: **if an effect cannot be derived
//! from something true, it does not move.** There is no spinner in this file and no place to
//! put one.
//!
//! # Whimsy before the point of no return, gravity after it
//!
//! Everything above belongs to finding and waiting. Past the confirmation the only thing that
//! moves is the pair of counters in [`Chase`] — reclaimable going down, freed coming up — and
//! that is the whole payoff. A deletion is a thing that might have been a mistake, so it gets
//! no celebration.
//!
//! # It is bounded by the viewport, not by the tree
//!
//! [`Moving::advance`] is handed the rows that are actually drawn, and it forgets every entry
//! it was not handed this frame. So the per-frame cost is the height of the pane whatever the
//! tree is doing — one real home directory is 22,765 directories and 16,013 claims, and none
//! of that is touched here. A row scrolled away and back has no state to inherit, and starts
//! showing the truth immediately, which is right: nobody watched it change.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::tree::NodeId;

/// Roughly how long a number takes to reach the one behind it.
///
/// Long enough to read as movement, short enough that a reader who looks at a row and then
/// acts on it is acting on the true figure. See [`Chase::advance`] for what "roughly" means.
pub const COUNT_UP: Duration = Duration::from_millis(200);

/// How long a newly arrived row stays lit.
///
/// About a second, because the eye has to be *drawn* to it rather than merely able to find it,
/// and because the alternative — a scrolling log of what was found — is the thing a tree
/// exists to avoid.
pub const ARRIVAL: Duration = Duration::from_millis(900);

/// How long one rung of a mark cascade stays lit.
pub const FLASH: Duration = Duration::from_millis(160);

/// How much later each rung above the marked row lights up.
///
/// The stagger is the whole message: the mark is seen *travelling* outwards, which is what
/// makes "this took everything underneath" a thing the screen said rather than a thing the
/// reader worked out.
pub const RUNG: Duration = Duration::from_millis(45);

/// How long a removed row keeps its place, emptying, before it collapses away.
///
/// The directory is already gone from the disk when this starts — the deleter only reports a
/// target once it is finished with it — so this is not a delay before something happens. It is
/// the row spending its last third of a second saying what happened to it.
pub const DRAIN: Duration = Duration::from_millis(350);

/// How long the pricing shimmer takes to cross its column once.
pub const SHIMMER: Duration = Duration::from_millis(700);

/// One number on its way to another.
///
/// Exponential rather than linear, for a reason that is about streaming rather than about
/// taste: the target moves. A claim lands, then another, then a price — a linear tween would
/// have to be restarted on each one and would visibly stutter, where an approach simply has a
/// new gap to close and keeps its speed continuous.
#[derive(Clone, Copy, Debug)]
pub struct Chase {
    shown: f64,
    /// The frame this was last advanced on. Doubles as the mark that keeps it alive: see
    /// [`Moving::advance`].
    at: Instant,
    settled: bool,
}

impl Chase {
    /// A number that is already where it belongs.
    #[must_use]
    pub fn new(value: u64, now: Instant) -> Self {
        Self {
            #[expect(
                clippy::cast_precision_loss,
                reason = "a byte count large enough to lose precision here is 4 petabytes, and \
                          the value is on its way to a display rounded to one decimal place"
            )]
            shown: value as f64,
            at: now,
            settled: true,
        }
    }

    /// Moves toward `target` by however much time has passed, and says where it got to.
    ///
    /// The time constant is a third of [`COUNT_UP`], so about 95% of the gap is closed in that
    /// long — which is what "roughly 200ms" means for a curve that never formally arrives.
    /// Formally never arriving is also why the snap below is not optional.
    pub fn advance(&mut self, target: u64, now: Instant) -> u64 {
        #[expect(
            clippy::cast_precision_loss,
            reason = "as in `new`: the display this is bound for has one decimal place"
        )]
        let target = target as f64;
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.at = now;
        let closed = 1.0 - (-elapsed * 3.0 / COUNT_UP.as_secs_f64()).exp();
        self.shown += (target - self.shown) * closed;
        // Snapped once the remaining gap is below what the column can print — a tenth of a
        // percent is a whole digit of `1023.9 GiB`, so half of that is invisible by
        // construction. Without it the value approaches forever and the view never reports
        // itself still, which is what the frame rate is chosen from.
        if (target - self.shown).abs() <= (target.abs() * 0.0005).max(1.0) {
            self.shown = target;
            self.settled = true;
        } else {
            self.settled = false;
        }
        self.value()
    }

    /// Puts the number somewhere without easing toward it.
    ///
    /// For a value that is already being interpolated by something else — a row emptying on a
    /// ramp — so that the chase takes over seamlessly when that ends rather than resuming from
    /// wherever it was left standing.
    pub fn jam(&mut self, value: u64, now: Instant) {
        *self = Self::new(value, now);
    }

    /// What to draw.
    #[must_use]
    pub fn value(&self) -> u64 {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the chase only ever runs between two byte counts, so it is bounded by \
                      them; a negative is arithmetically unreachable and saturates to zero \
                      rather than wrapping"
        )]
        let value = self.shown.max(0.0).round() as u64;
        value
    }

    /// Whether it has arrived, which is what "nothing is moving" is made of.
    #[must_use]
    pub fn settled(&self) -> bool {
        self.settled
    }
}

/// Everything the view is in the middle of showing.
#[derive(Debug)]
pub struct Moving {
    /// One chase per row that was drawn last frame. Bounded by the pane.
    rows: HashMap<NodeId, Chase>,
    /// When each row appeared in the tree, while that is still recent.
    arrived: HashMap<NodeId, Instant>,
    /// When each rung of the last mark cascade lights up. Ancestors only, so it is bounded by
    /// the depth of the tree — ten, on a real home directory.
    cascade: HashMap<NodeId, Instant>,
    /// Rows the deleter has finished with, still emptying. The view reads the deadline off
    /// this and takes the row out of the tree for real when it passes.
    draining: HashMap<NodeId, Instant>,
    /// Claims a pricing thread is inside at this instant. Exactly as many as the pool has
    /// threads, which is the fact the shimmer is drawing.
    hot: HashSet<NodeId>,
    /// What the session has given back, climbing.
    freed: Chase,
    now: Instant,
}

impl Moving {
    /// Nothing moving, as of `now`.
    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self {
            rows: HashMap::new(),
            arrived: HashMap::new(),
            cascade: HashMap::new(),
            draining: HashMap::new(),
            hot: HashSet::new(),
            freed: Chase::new(0, now),
            now,
        }
    }

    /// Moves the clock on, before anything asks a question whose answer depends on it.
    ///
    /// Split out from [`Moving::advance`] because the caller has work to do *between* the two
    /// — working out what each row is worth given what is draining away under it, which is a
    /// question about this frame's instant and not the last one's.
    pub fn tick(&mut self, now: Instant) {
        self.now = now;
    }

    /// Advances every drawn row toward what it is really worth, and forgets the rest.
    ///
    /// `rows` is the viewport's worth of `(row, what it is worth now, is that exact)`, so a row
    /// that scrolled off loses its state and a row that scrolls back on starts at the truth.
    /// That is the whole of the cost story: this is O(rows on screen), never O(tree).
    ///
    /// **Exact** means the caller is already interpolating that value itself and the chase must
    /// not add a second, slower opinion on top — which is what a row emptying is. Jammed rather
    /// than skipped, so that when the drain ends the chase carries on from where the ramp left
    /// off instead of from wherever it was standing when the drain began.
    pub fn advance(&mut self, now: Instant, rows: &[(NodeId, u64, bool)], freed: u64) {
        self.now = now;
        for &(id, target, exact) in rows {
            let chase = self
                .rows
                .entry(id)
                .or_insert_with(|| Chase::new(target, now));
            if exact {
                chase.jam(target, now);
            } else {
                chase.advance(target, now);
            }
        }
        // A chase stamped with any earlier frame belongs to a row nobody is drawing.
        self.rows.retain(|_, chase| chase.at == now);
        self.freed.advance(freed, now);
        self.arrived
            .retain(|_, at| now.saturating_duration_since(*at) < ARRIVAL);
        // The stamp is when a rung *lights*, which for the outer ones is still in the future —
        // and `saturating_duration_since` reads a future instant as no time at all, so a rung
        // waiting its turn is kept by the same condition that keeps a lit one.
        self.cascade
            .retain(|_, at| now.saturating_duration_since(*at) < FLASH);
    }

    /// What a row draws, which is the truth once it has caught up with it.
    #[must_use]
    pub fn shown(&self, id: NodeId, truth: u64) -> u64 {
        self.rows.get(&id).map_or(truth, Chase::value)
    }

    /// What the session has given back so far, climbing.
    #[must_use]
    pub fn freed(&self) -> u64 {
        self.freed.value()
    }

    /// Notes a directory that has just appeared in the tree.
    pub fn arrived(&mut self, id: NodeId, now: Instant) {
        self.arrived.insert(id, now);
    }

    /// How lit a newly arrived row is: 1.0 the moment it lands, 0.0 once it is old news.
    #[must_use]
    pub fn freshness(&self, id: NodeId) -> f64 {
        let Some(at) = self.arrived.get(&id) else {
            return 0.0;
        };
        let elapsed = self.now.saturating_duration_since(*at).as_secs_f64();
        (1.0 - elapsed / ARRIVAL.as_secs_f64()).clamp(0.0, 1.0)
    }

    /// Lights a mark running outwards through `ancestors`, nearest first.
    pub fn cascade(&mut self, ancestors: &[NodeId], now: Instant) {
        for (rung, &id) in ancestors.iter().enumerate() {
            self.cascade
                .insert(id, now + RUNG * u32::try_from(rung).unwrap_or(u32::MAX));
        }
    }

    /// Whether this row's rung of the cascade is lit at this instant.
    ///
    /// A rung that has not come round yet is not lit either, which is what makes the mark
    /// travel rather than all of it flashing at once.
    #[must_use]
    pub fn is_cascading(&self, id: NodeId) -> bool {
        self.cascade
            .get(&id)
            .is_some_and(|at| *at <= self.now && self.now.saturating_duration_since(*at) < FLASH)
    }

    /// Notes that a pricing thread has gone into this claim.
    pub fn heats(&mut self, id: NodeId) {
        self.hot.insert(id);
    }

    /// Notes that it has come back out, with a price or without one.
    pub fn cools(&mut self, id: NodeId) {
        self.hot.remove(&id);
    }

    /// Forgets every claim that was being priced, for the end of the walk: a pool that has
    /// stopped leaves nothing hot behind, and a row shimmering for a thread that no longer
    /// exists would be the one moving thing here that says nothing.
    pub fn cooled(&mut self) {
        self.hot.clear();
    }

    /// Whether a pricing thread is inside this claim right now.
    #[must_use]
    pub fn is_hot(&self, id: NodeId) -> bool {
        self.hot.contains(&id)
    }

    /// Every claim currently being priced, so the view can drop the ones that have since gone.
    pub fn hot(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.hot.iter().copied()
    }

    /// Which cell of a `width`-wide shimmer is lit.
    ///
    /// One phase for the whole screen rather than one per row: the reader is being told how
    /// many rows are hot, and rows that pulse together are countable at a glance where rows
    /// each doing their own thing are not.
    #[must_use]
    pub fn shimmer(&self, width: usize, epoch: Instant) -> usize {
        if width == 0 {
            return 0;
        }
        let step = SHIMMER.as_millis().max(1) / width as u128;
        let elapsed = self.now.saturating_duration_since(epoch).as_millis();
        usize::try_from(elapsed / step.max(1) % width as u128).unwrap_or(0)
    }

    /// Starts a removed row emptying.
    pub fn drains(&mut self, id: NodeId, now: Instant) {
        self.draining.entry(id).or_insert(now);
    }

    /// Whether this row is on its way out.
    #[must_use]
    pub fn is_draining(&self, id: NodeId) -> bool {
        self.draining.contains_key(&id)
    }

    /// Every row on its way out, for the counters that have to leave them out.
    pub fn draining(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.draining.keys().copied()
    }

    /// How much of a draining row is left, from 1.0 down to 0.0 over [`DRAIN`].
    ///
    /// A straight ramp, and it is the one number here that is **not** a [`Chase`]: an
    /// exponential approach never formally arrives, so a row easing toward zero would still be
    /// showing a few hundred kilobytes at the moment it collapsed away. This one reaches zero
    /// exactly when the row goes, which is the whole claim the effect makes — the row emptied,
    /// and then it was gone.
    #[must_use]
    pub fn draining_share(&self, id: NodeId) -> Option<f64> {
        let at = self.draining.get(&id)?;
        let elapsed = self.now.saturating_duration_since(*at).as_secs_f64();
        Some((1.0 - elapsed / DRAIN.as_secs_f64()).clamp(0.0, 1.0))
    }

    /// The rows whose drain has run its course, which the view then takes out of the tree for
    /// real. Forgotten here in the same breath, so each is handed over exactly once.
    pub fn drained(&mut self, now: Instant) -> Vec<NodeId> {
        let due: Vec<NodeId> = self
            .draining
            .iter()
            .filter(|(_, at)| now.saturating_duration_since(**at) >= DRAIN)
            .map(|(&id, _)| id)
            .collect();
        for id in &due {
            self.draining.remove(id);
        }
        due
    }

    /// Whether anything at all is still in motion.
    ///
    /// What the event loop reads to decide how often to repaint: a view with something moving
    /// in it earns a smooth frame rate, and a view a reader is sitting and thinking in front
    /// of does not.
    #[must_use]
    pub fn is_moving(&self) -> bool {
        !self.hot.is_empty()
            || !self.draining.is_empty()
            || !self.cascade.is_empty()
            || !self.arrived.is_empty()
            || !self.freed.settled()
            || self.rows.values().any(|chase| !chase.settled())
    }
}

#[cfg(test)]
mod tests {
    use super::{ARRIVAL, COUNT_UP, Chase, DRAIN, FLASH, Moving, RUNG};
    use std::time::{Duration, Instant};

    #[test]
    fn a_chase_climbs_toward_its_target_and_arrives_at_it_exactly() {
        let start = Instant::now();
        let mut chase = Chase::new(0, start);

        // Part way there is genuinely part way: the point of the effect is that the reader
        // sees the number move rather than appear.
        let half = chase.advance(1_000_000, start + COUNT_UP / 2);
        assert!(half > 0 && half < 1_000_000, "{half}");
        assert!(!chase.settled());

        // …and it lands on the true number rather than approaching it forever, which is what
        // lets the view report itself still.
        let landed = chase.advance(1_000_000, start + COUNT_UP * 4);
        assert_eq!(landed, 1_000_000);
        assert!(chase.settled());
    }

    #[test]
    fn a_chase_runs_downwards_as_readily_as_up() {
        let start = Instant::now();
        let mut chase = Chase::new(1_000_000, start);
        // Which is what a deletion is: the same mechanism, and no second one to keep in step
        // with this one.
        let draining = chase.advance(0, start + COUNT_UP / 2);
        assert!(draining > 0 && draining < 1_000_000, "{draining}");
        // Zero is the one target an approach takes a while over, because the snap is the
        // absolute byte the number is finally within rather than a share of a target that is
        // itself nothing. It is also the reason a *row* emptying is a ramp and not one of
        // these — see [`Moving::draining_share`].
        assert_eq!(chase.advance(0, start + COUNT_UP * 8), 0);
    }

    #[test]
    fn a_target_that_moves_mid_flight_is_chased_rather_than_restarted() {
        let start = Instant::now();
        let mut chase = Chase::new(0, start);
        let first = chase.advance(100, start + COUNT_UP / 4);
        // A claim lands while the previous one is still being counted up to. Nothing resets:
        // the gap is simply bigger now, which is what makes a stream of arrivals read as one
        // continuous climb rather than as a stutter per claim.
        let second = chase.advance(200, start + COUNT_UP / 2);
        assert!(second > first, "{first} -> {second}");
        assert!(second < 200);
    }

    #[test]
    fn a_row_that_scrolled_off_the_screen_is_forgotten_rather_than_animated() {
        let start = Instant::now();
        let mut moving = Moving::new(start);
        moving.advance(start, &[(1, 100, false), (2, 200, false)], 0);
        moving.advance(start + COUNT_UP, &[(1, 100, false)], 0);

        // The cost story: one entry per row the pane drew, whatever the tree is doing.
        assert_eq!(
            moving.shown(2, 999),
            999,
            "a row nobody drew kept its state"
        );
        assert_eq!(moving.shown(1, 100), 100);
    }

    #[test]
    fn a_newly_arrived_row_is_lit_and_the_light_decays() {
        let start = Instant::now();
        let mut moving = Moving::new(start);
        moving.arrived(7, start);

        moving.advance(start, &[], 0);
        assert!((moving.freshness(7) - 1.0).abs() < f64::EPSILON);
        moving.advance(start + ARRIVAL / 2, &[], 0);
        assert!(
            (0.4..0.6).contains(&moving.freshness(7)),
            "{}",
            moving.freshness(7)
        );
        moving.advance(start + ARRIVAL * 2, &[], 0);
        assert!(moving.freshness(7).abs() < f64::EPSILON);
        assert!(
            !moving.is_moving(),
            "a light nobody can see is still animating"
        );
    }

    #[test]
    fn a_cascade_lights_each_rung_later_than_the_one_below_it() {
        let start = Instant::now();
        let mut moving = Moving::new(start);
        // The chain a mark on a deep row runs through: the row, then its parent, then the root.
        moving.cascade(&[10, 11, 12], start);

        moving.advance(start, &[], 0);
        assert!(moving.is_cascading(10));
        assert!(!moving.is_cascading(12), "the whole chain flashed at once");

        moving.advance(start + RUNG * 2, &[], 0);
        assert!(moving.is_cascading(12), "the mark never reached the root");

        moving.advance(start + RUNG * 2 + FLASH, &[], 0);
        assert!(!moving.is_cascading(12));
        assert!(!moving.is_moving());
    }

    #[test]
    fn a_draining_row_is_handed_back_once_and_only_once() {
        let start = Instant::now();
        let mut moving = Moving::new(start);
        moving.drains(3, start);
        assert!(moving.is_draining(3));

        assert!(moving.drained(start + DRAIN / 2).is_empty());
        assert_eq!(moving.drained(start + DRAIN), vec![3]);
        // Handed over twice, the view would try to remove the same claim from the tree twice —
        // and the second removal would be refused, silently, which is the shape of bug this
        // whole file has to avoid.
        assert!(moving.drained(start + DRAIN * 2).is_empty());
        assert!(!moving.is_draining(3));
    }

    #[test]
    fn the_shimmer_travels_and_comes_round() {
        let start = Instant::now();
        let mut moving = Moving::new(start);
        moving.advance(start, &[], 0);
        let first = moving.shimmer(5, start);
        moving.advance(start + super::SHIMMER / 5, &[], 0);
        let second = moving.shimmer(5, start);
        assert_ne!(first, second, "the shimmer stood still");
        moving.advance(start + super::SHIMMER, &[], 0);
        assert_eq!(moving.shimmer(5, start), first, "it never came round");
    }

    #[test]
    fn a_view_with_nothing_happening_in_it_reports_itself_still() {
        let start = Instant::now();
        let mut moving = Moving::new(start);
        moving.advance(start, &[(1, 100, false)], 0);
        assert!(!moving.is_moving());

        // A claim being priced is the one kind of motion with no clock on it: it runs until
        // the pool comes back, however long that takes.
        moving.heats(1);
        assert!(moving.is_moving());
        moving.cools(1);
        assert!(!moving.is_moving());

        moving.advance(start + Duration::from_millis(1), &[(1, 100_000, false)], 0);
        assert!(moving.is_moving(), "a number in flight is motion");
    }
}
