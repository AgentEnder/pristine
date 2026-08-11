//! Where each rectangle goes, and what it is allowed to claim about itself.
//!
//! Two separate problems, and the second one is the whole reason this task was a spike.
//!
//! # Squarifying
//!
//! [`squarify`] is Bruls, Huizing and van Wijk's algorithm: lay the weights out in rows
//! against the shorter side of what is left, closing a row the moment adding to it would make
//! the worst aspect ratio worse. Slice-and-dice — the obvious alternative — gives exact areas
//! too and gives them as slivers, and a sliver is a rectangle whose *area* nobody can read.
//!
//! # The honest map
//!
//! A treemap encodes one measure as area, and pristine does not have one measure. A claim is
//! published the moment it is judged and priced later (#618), so at any instant the tree holds
//! bytes for some claims and nothing at all for the rest — and a directory nobody has measured
//! is not worth zero bytes, it is worth an unknown number of them.
//!
//! Laying children out by priced bytes alone would draw a 40 GB `node_modules` nobody has
//! reached as a sliver, or as nothing. **That is the one lie this tool must not tell**, and it
//! is not a rounding error: a default `--breakdown` over one real `~/repos` publishes its last
//! claim at 7.5 s and finishes pricing at 63 s, so for a minute most of the map would be a
//! lie about most of the disk.
//!
//! So the map is split in two before anything is laid out. The **known** region is squarified
//! by priced bytes and every rectangle in it means exactly what it looks like. The
//! **unknown** region is squarified by *unpriced claim count*, drawn as texture rather than as
//! colour, and labelled in directories rather than in bytes.
//!
//! The unknown region's **area is not information and is not claimed to be** — there is no
//! honest area for "we have not looked", and the count in the label is the only thing in that
//! half of the picture that is a fact. What its area buys is presence: the region cannot be
//! zero-width while anything is unpriced ([`MIN_UNKNOWN`]), so a reader can never mistake a
//! half-measured tree for a measured one, and it shrinks as the pool catches up — which is
//! #621's rule that motion should be a value the view already holds, in the one place a
//! treemap can obey it.
//!
//! This is the spatial form of the answer `Roll::label` and #621's `> 4.2 GiB` already give in
//! text: say what is known, and say that the rest is not known, rather than averaging the two
//! into a number that is wrong in the direction a cleaner must never be wrong in.

use crate::size::human;
use crate::tree::NodeId;
use crate::tui::state::{Mark, View};

/// The smallest share of the map the unknown region takes while anything is unpriced.
///
/// A floor and not a fudge. One unpriced claim in a thousand is a genuinely tiny share of
/// what is unknown *by count*, and drawn to scale it would be a sub-pixel stripe — which is
/// visually identical to a map with nothing missing from it. The two states have to look
/// different, because one of them is complete and the other is not.
const MIN_UNKNOWN: f64 = 0.08;

/// How deep the map nests.
///
/// A backstop rather than the control: what actually stops the nesting is [`NEST`], the size
/// below which a rectangle has no room for a caption and two children — and an unlabelled
/// rectangle is a shape rather than an answer. The first render of this spike capped at two
/// and left a 14 GiB `packages` drawn as one blank box with plenty of room inside it, which
/// is the wrong reason to stop.
const MAX_DEPTH: usize = 3;

/// The narrowest and shortest a rectangle can be and still be worth nesting into, in pixels.
const NEST: (f64, f64) = (110.0, 54.0);

/// How much of a nested rectangle its own label and border take off the top.
const CAPTION: f64 = 13.0;

/// A rectangle, in image pixels.
///
/// Floating point all the way to the paint, and rounded once: rounding each level as it is
/// laid out accumulates, and a treemap whose children do not quite fill their parent has a
/// seam down it that reads as a boundary nobody put there.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Area {
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width.
    pub w: f64,
    /// Height.
    pub h: f64,
}

impl Area {
    /// A rectangle at the origin.
    #[must_use]
    pub fn of(w: f64, h: f64) -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            w,
            h,
        }
    }

    /// How much of the map this rectangle covers.
    #[must_use]
    pub fn size(&self) -> f64 {
        self.w.max(0.0) * self.h.max(0.0)
    }

    /// The same rectangle, pulled in by `by` on every side.
    #[must_use]
    fn inset(&self, by: f64) -> Self {
        Self {
            x: self.x + by,
            y: self.y + by,
            w: (self.w - 2.0 * by).max(0.0),
            h: (self.h - 2.0 * by).max(0.0),
        }
    }

    /// This rectangle with `off` taken off the top, for a caption.
    #[must_use]
    fn below(&self, off: f64) -> Self {
        Self {
            x: self.x,
            y: self.y + off,
            w: self.w,
            h: (self.h - off).max(0.0),
        }
    }

    /// The two halves this rectangle splits into, cut across its longer axis, with `share` of
    /// it going to the second.
    fn split(&self, share: f64) -> (Self, Self) {
        if self.w >= self.h {
            let second = (self.w * share).round();
            (
                Self {
                    w: self.w - second,
                    ..*self
                },
                Self {
                    x: self.x + self.w - second,
                    w: second,
                    ..*self
                },
            )
        } else {
            let second = (self.h * share).round();
            (
                Self {
                    h: self.h - second,
                    ..*self
                },
                Self {
                    y: self.y + self.h - second,
                    h: second,
                    ..*self
                },
            )
        }
    }
}

impl std::hash::Hash for Area {
    /// By bit pattern, because these are only ever hashed to answer "is this the same
    /// picture as last frame" — where two rectangles that differ in the last bit really are
    /// a different frame, and no rectangle is ever a NaN.
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        for side in [self.x, self.y, self.w, self.h] {
            side.to_bits().hash(state);
        }
    }
}

/// What a rectangle's area is made of, which decides how it is drawn and what it may say.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum Kind {
    /// Bytes somebody measured. The area is proportional to them and the label states them.
    Priced,
    /// Claims nobody has measured. The area stands for a *count*, the fill is texture rather
    /// than colour, and the label is in directories — never in bytes.
    Unpriced,
}

/// One rectangle of the map.
#[derive(Clone, Debug, Hash, PartialEq)]
pub struct Tile {
    /// Which directory.
    pub id: NodeId,
    /// Where it goes.
    pub area: Area,
    /// 1 for a child of the mapped directory, 2 for a grandchild.
    pub depth: usize,
    /// Whether its area is bytes or a count of things nobody has counted.
    pub kind: Kind,
    /// Whether the reader has marked this whole subtree.
    pub marked: bool,
    /// Whether the tree's cursor is on this directory — the map's half of "you are here".
    pub cursor: bool,
    /// Whether rectangles are drawn inside this one.
    ///
    /// The paint needs to know, because a nested rectangle owns everything below this one's
    /// caption strip: a label laid out as though it had the whole tile gets its second line
    /// half eaten by its own child, which is what the first render of this spike did.
    pub nested: bool,
    /// The directory's own name.
    pub name: String,
    /// What it is worth, in the unit its area is in.
    pub worth: String,
}

/// A whole map: the mapped directory, its rectangles, and where the two regions meet.
#[derive(Clone, Debug, Hash, PartialEq)]
pub struct Map {
    /// The directory the map is of.
    pub root: NodeId,
    /// Every rectangle, outermost first — so painting them in order draws children over the
    /// parent they are inside.
    pub tiles: Vec<Tile>,
    /// The unknown region, when anything under the mapped directory is unpriced.
    pub unknown: Option<Area>,
    /// The line across the top of the map.
    pub caption: String,
}

/// Which directory the map is of, given where the cursor is.
///
/// A row with children maps itself; a claim — which is a leaf — maps its **parent**, because
/// a treemap of one rectangle says nothing and what a reader on a leaf wants to know is how
/// it compares with what is beside it. So drilling in with `→` re-draws the map one level
/// down, and landing on a `node_modules` at the bottom shows the level it lives in.
#[must_use]
pub fn focus(view: &View) -> Option<NodeId> {
    let id = view.row()?.id;
    if view.tree().children(id).is_empty() {
        return view.tree().node(id).parent.or(Some(id));
    }
    Some(id)
}

/// The map of `root` inside `area`, or `None` when there is nothing honest to draw.
///
/// `None` rather than an empty rectangle in two cases, and they are different: a directory
/// with no children under the current filter has nothing to divide, and one whose whole
/// subtree is both unpriced and uncounted has nothing to divide it *by*.
#[must_use]
pub fn plan(view: &View, root: NodeId, area: Area) -> Option<Map> {
    let roll = view.roll(root);
    if roll.claims == 0 || area.w < 1.0 || area.h < 1.0 {
        return None;
    }
    // The split comes first, before a single rectangle is placed: what is unknown is not a
    // leftover of the layout, it is a share of the map reserved before the layout runs.
    let share = if roll.unpriced == 0 {
        0.0
    } else {
        #[expect(
            clippy::cast_precision_loss,
            reason = "claim counts are in the tens of thousands; the ratio is a fraction of a \
                      pane, not an accounting figure"
        )]
        let exact = roll.unpriced as f64 / roll.claims as f64;
        exact.clamp(MIN_UNKNOWN, 1.0)
    };
    let (known, unknown) = area.split(share);

    let mut tiles = Vec::new();
    if share < 1.0 {
        lay(view, root, known, 1, &mut tiles, Kind::Priced);
    }
    if share > 0.0 {
        lay(view, root, unknown, 1, &mut tiles, Kind::Unpriced);
    }
    if tiles.is_empty() {
        return None;
    }
    Some(Map {
        root,
        tiles,
        unknown: (share > 0.0).then_some(unknown),
        caption: caption(view, root),
    })
}

/// The line across the top: which directory this is a map of, and what it is worth.
///
/// Drawn by the *renderer* as ordinary terminal text above the image rather than painted
/// into it, so the one line a reader is most likely to want to copy is real text at the
/// terminal's own font size. The image below it carries no chrome of its own.
#[must_use]
pub fn caption(view: &View, root: NodeId) -> String {
    let roll = view.roll(root);
    let node = view.tree().node(root);
    let name = if node.parent.is_none() {
        node.path.display().to_string()
    } else {
        node.name.to_string_lossy().into_owned()
    };
    if roll.unpriced == 0 {
        return format!("{name} — {}", human(roll.bytes));
    }
    // The header's own `>`, for #621's reason: a total over a subtree that is still being
    // priced is a lower bound tightening rather than a figure, and saying so costs nothing.
    format!(
        "{name} — > {} · {} unpriced",
        human(roll.bytes),
        roll.unpriced
    )
}

/// The children of `parent` that carry any of the measure `kind` stands for.
///
/// Under a filter this is filter-aware by construction, because [`View::roll`] is: a
/// rectangle drawn for bytes the filter is hiding is a rectangle whose mark would delete
/// them, which is #602 finding 5 arriving in a second place.
fn weighted(view: &View, parent: NodeId, kind: Kind) -> Vec<(NodeId, u64)> {
    let mut children: Vec<(NodeId, u64)> = view
        .tree()
        .children(parent)
        .iter()
        .filter_map(|&id| {
            let roll = view.roll(id);
            let weight = match kind {
                Kind::Priced => roll.bytes,
                Kind::Unpriced => roll.unpriced as u64,
            };
            (weight > 0).then_some((id, weight))
        })
        .collect();
    // Descending, which is what squarifying assumes and also what a reader expects: the
    // biggest thing is in the corner the eye starts at. The id breaks ties so that two
    // equal siblings do not swap places every time a price lands somewhere else.
    children.sort_unstable_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    children
}

/// Follows a chain of only-children down, and says where it ended and what to call it.
///
/// `~/repos/a/node_modules` is three nodes and one fact. Nesting a rectangle inside a
/// rectangle of nearly the same size, with a caption bar between them, spends two labels and
/// most of the ink saying it twice — so a run of directories with one weighted child each is
/// one rectangle called `a/node_modules`. This is what makes the map denser than the tree
/// rather than a picture of it.
fn collapse(view: &View, id: NodeId, kind: Kind) -> (NodeId, Vec<NodeId>, String) {
    let mut chain = vec![id];
    let mut name = view.tree().node(id).name.to_string_lossy().into_owned();
    let mut at = id;
    while let [(only, _)] = weighted(view, at, kind).as_slice() {
        at = *only;
        chain.push(at);
        name.push('/');
        name.push_str(&view.tree().node(at).name.to_string_lossy());
    }
    (at, chain, name)
}

/// Squarifies `parent`'s children into `area` and recurses while there is room to.
fn lay(view: &View, parent: NodeId, area: Area, depth: usize, out: &mut Vec<Tile>, kind: Kind) {
    let children = weighted(view, parent, kind);
    if children.is_empty() {
        return;
    }
    let weights: Vec<u64> = children.iter().map(|(_, weight)| *weight).collect();
    for ((id, weight), placed) in children.iter().zip(squarify(area, &weights)) {
        if placed.size() < 1.0 {
            continue;
        }
        let (deepest, chain, name) = collapse(view, *id, kind);
        // Nesting is only ever worth it inside the known region: the unknown one is already
        // saying the only thing it knows, and dividing "we have not looked" into smaller
        // pieces of "we have not looked" adds nothing.
        let nested = kind == Kind::Priced
            && depth < MAX_DEPTH
            && placed.w >= NEST.0
            && placed.h >= NEST.1
            && !weighted(view, deepest, kind).is_empty();
        out.push(Tile {
            id: *id,
            area: placed,
            depth,
            kind,
            marked: view.mark_of(*id) == Mark::All,
            // Anywhere in the collapsed run, because the run is one rectangle: a cursor on
            // `a` and a cursor on `a/node_modules` are the same place on this picture.
            cursor: view.row().is_some_and(|row| chain.contains(&row.id)),
            nested,
            name,
            worth: match kind {
                Kind::Priced => human(*weight),
                Kind::Unpriced => format!("{weight} unpriced"),
            },
        });
        if nested {
            lay(
                view,
                deepest,
                placed.inset(2.0).below(CAPTION),
                depth + 1,
                out,
                kind,
            );
        }
    }
}

/// Bruls, Huizing and van Wijk's squarified treemap.
///
/// One rectangle per weight, in the order given — which the caller has already sorted
/// descending, because the algorithm's aspect-ratio bound assumes it. Weights of zero get an
/// empty rectangle rather than being dropped, so the two lists stay index-for-index.
///
/// The rectangles tile `area` exactly: each one's share of the total area equals its share of
/// the total weight, which is the whole claim a treemap makes and the one thing worth
/// asserting about it.
#[must_use]
pub fn squarify(area: Area, weights: &[u64]) -> Vec<Area> {
    let mut out = vec![Area::default(); weights.len()];
    let total: u128 = weights.iter().map(|&weight| u128::from(weight)).sum();
    if total == 0 || area.size() <= 0.0 {
        return out;
    }
    // Scaled once, against the *original* rectangle: the areas left to place then always sum
    // to exactly the free rectangle, so no rounding creeps in as rows are closed.
    #[expect(
        clippy::cast_precision_loss,
        reason = "byte totals reach terabytes; f64 carries 53 bits of mantissa, so the error \
                  is far below one pixel of a pane"
    )]
    let scale = area.size() / total as f64;
    #[expect(
        clippy::cast_precision_loss,
        reason = "as above — these are pixel areas, not ledgers"
    )]
    let sized: Vec<f64> = weights
        .iter()
        .map(|&weight| weight as f64 * scale)
        .collect();

    // The indices worth placing, biggest first as the caller ordered them.
    let order: Vec<usize> = (0..sized.len()).filter(|&at| sized[at] > 0.0).collect();
    let mut free = area;
    let mut next = 0;
    while next < order.len() {
        let short = free.w.min(free.h);
        if short <= 0.0 {
            break;
        }
        // Grow the row while doing so improves the worst aspect ratio in it.
        let mut end = next + 1;
        let mut row = sized[order[next]];
        let mut best = worst(row, row, row, short);
        while end < order.len() {
            let candidate = sized[order[end]];
            let grown = row + candidate;
            // The row is descending, so the newcomer is always the smallest in it.
            let ratio = worst(grown, sized[order[next]], candidate, short);
            if ratio > best {
                break;
            }
            best = ratio;
            row = grown;
            end += 1;
        }
        free = place(&sized, &order[next..end], row, free, &mut out);
        next = end;
    }
    out
}

/// The worst aspect ratio in a row of total area `row` laid against a side of length `short`.
fn worst(row: f64, largest: f64, smallest: f64, short: f64) -> f64 {
    if row <= 0.0 || smallest <= 0.0 {
        return f64::INFINITY;
    }
    let side = short * short;
    let sum = row * row;
    f64::max(side * largest / sum, sum / (side * smallest))
}

/// Lays one closed row along the short side of `free` and returns what is left.
fn place(sized: &[f64], row: &[usize], total: f64, free: Area, out: &mut [Area]) -> Area {
    let short = free.w.min(free.h);
    let thick = total / short;
    let mut along = 0.0;
    if free.w <= free.h {
        for &at in row {
            let width = sized[at] / thick;
            out[at] = Area {
                x: free.x + along,
                y: free.y,
                w: width,
                h: thick,
            };
            along += width;
        }
        Area {
            y: free.y + thick,
            h: free.h - thick,
            ..free
        }
    } else {
        for &at in row {
            let height = sized[at] / thick;
            out[at] = Area {
                x: free.x,
                y: free.y + along,
                w: thick,
                h: height,
            };
            along += height;
        }
        Area {
            x: free.x + thick,
            w: free.w - thick,
            ..free
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Area, Kind, MIN_UNKNOWN, focus, plan, squarify};
    use crate::fixture::{hit, priced};
    use crate::size::Size;
    use crate::tree::Tree;
    use crate::tui::keymap::{Action, Motion};
    use crate::tui::state::View;

    fn pane() -> Area {
        Area::of(400.0, 300.0)
    }

    /// Every rectangle's share of the area is its share of the weight, and none of them
    /// overlap. The whole claim a treemap makes, over a set with a hard spread.
    #[test]
    fn area_is_proportional_to_weight_and_nothing_overlaps() {
        let weights = [4096_u64, 2048, 1024, 900, 512, 64, 8, 1];
        let area = pane();
        let placed = squarify(area, &weights);

        #[expect(clippy::cast_precision_loss, reason = "a test fixture's totals")]
        let total = weights.iter().sum::<u64>() as f64;
        for (weight, rect) in weights.iter().zip(&placed) {
            #[expect(clippy::cast_precision_loss, reason = "a test fixture's totals")]
            let want = area.size() * (*weight as f64) / total;
            assert!(
                (rect.size() - want).abs() < 1e-6,
                "{weight} got {:?}, worth {want}",
                rect.size()
            );
            assert!(
                rect.x >= -1e-9
                    && rect.y >= -1e-9
                    && rect.x + rect.w <= area.w + 1e-9
                    && rect.y + rect.h <= area.h + 1e-9,
                "{rect:?} escaped {area:?}"
            );
        }
        for (at, first) in placed.iter().enumerate() {
            for second in &placed[at + 1..] {
                let across = (first.x + first.w).min(second.x + second.w) - first.x.max(second.x);
                let down = (first.y + first.h).min(second.y + second.h) - first.y.max(second.y);
                assert!(
                    across <= 1e-9 || down <= 1e-9,
                    "{first:?} overlaps {second:?}"
                );
            }
        }
    }

    /// The reason for squarifying rather than slicing. A sliver has an exact area nobody can
    /// read, so the bound on the aspect ratio is the feature.
    #[test]
    fn no_rectangle_comes_out_a_sliver() {
        let weights = [4096_u64, 2048, 1024, 900, 512, 64, 32, 16];
        for rect in squarify(pane(), &weights) {
            let ratio = (rect.w / rect.h).max(rect.h / rect.w);
            assert!(ratio < 8.0, "{rect:?} has an aspect ratio of {ratio}");
        }
    }

    #[test]
    fn one_weight_takes_the_whole_rectangle_and_no_weight_takes_none_of_it() {
        let whole = squarify(pane(), &[7]);
        assert!((whole[0].size() - pane().size()).abs() < 1e-6, "{whole:?}");
        assert_eq!(squarify(pane(), &[]), Vec::new());
        assert_eq!(squarify(pane(), &[0, 0]), vec![Area::default(); 2]);
        // A zero keeps its slot rather than shifting the rectangles after it onto the wrong
        // directories.
        let mixed = squarify(pane(), &[8, 0, 8]);
        assert_eq!(mixed[1], Area::default());
        assert!((mixed[0].size() - mixed[2].size()).abs() < 1e-6);
    }

    // ---- what the map is allowed to say -----------------------------------------------

    fn view() -> View {
        let mut tree = Tree::new("/scan");
        tree.insert(priced("/scan/a/node_modules", 8 * 1024 * 1024));
        tree.insert(priced("/scan/b/target", 2 * 1024 * 1024));
        View::new(tree)
    }

    #[test]
    fn a_priced_tree_is_a_plain_treemap_with_no_unknown_region() {
        let view = view();
        let map = plan(&view, view.tree().root(), pane()).unwrap();

        assert!(map.unknown.is_none(), "{:?}", map.unknown);
        assert!(map.tiles.iter().all(|tile| tile.kind == Kind::Priced));
        let a = map
            .tiles
            .iter()
            .find(|tile| tile.name == "a/node_modules")
            .unwrap_or_else(|| panic!("{:?}", map.tiles));
        let b = map
            .tiles
            .iter()
            .find(|tile| tile.name == "b/target")
            .unwrap();
        // Four times the bytes, four times the area — the claim the picture makes.
        assert!((a.area.size() / b.area.size() - 4.0).abs() < 1e-6);
        assert_eq!(a.worth, "8.0 MiB");
        assert!(map.caption.contains("/scan"), "{}", map.caption);
        assert!(!map.caption.contains("unpriced"), "{}", map.caption);
    }

    /// The finding this whole task turns on. A directory nobody has measured is not worth
    /// zero bytes, so it must never be drawn as a rectangle among ones that are.
    #[test]
    fn an_unpriced_claim_is_never_a_sliver_among_priced_ones() {
        let mut tree = Tree::new("/scan");
        tree.insert(priced("/scan/small/target", 1024));
        // The 40 GB `node_modules` nobody has reached. By bytes it weighs nothing.
        tree.insert(hit("/scan/huge/node_modules", Size::Unmeasured, 0));
        let view = View::new(tree);
        let map = plan(&view, view.tree().root(), pane()).unwrap();

        let huge = map
            .tiles
            .iter()
            .find(|tile| tile.name == "huge/node_modules")
            .unwrap_or_else(|| panic!("the unpriced subtree vanished: {:?}", map.tiles));
        assert_eq!(huge.kind, Kind::Unpriced);
        assert_eq!(huge.worth, "1 unpriced", "stated in bytes it does not have");
        assert!(map.unknown.is_some());
        // And it is a real part of the picture rather than a hairline: half the claims are
        // unpriced, so half the map is.
        assert!(
            huge.area.size() > pane().size() * 0.4,
            "{:?} of {:?}",
            huge.area,
            pane()
        );
    }

    #[test]
    fn one_unpriced_claim_in_a_hundred_is_still_visible() {
        let mut tree = Tree::new("/scan");
        for n in 0..99 {
            tree.insert(priced(&format!("/scan/p{n}/target"), 1024));
        }
        tree.insert(hit("/scan/late/node_modules", Size::Unmeasured, 0));
        let view = View::new(tree);
        let map = plan(&view, view.tree().root(), pane()).unwrap();

        // Drawn to scale this is 1% of the map, which at this size is a sub-pixel stripe —
        // visually identical to a tree with nothing missing from it. The two states have to
        // look different.
        let unknown = map.unknown.unwrap();
        assert!(
            unknown.size() >= pane().size() * MIN_UNKNOWN - 1.0,
            "{unknown:?}"
        );
    }

    #[test]
    fn a_wholly_unpriced_tree_is_wholly_texture_and_says_so() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/a/node_modules", Size::Unmeasured, 0));
        tree.insert(hit("/scan/b/target", Size::Unmeasured, 0));
        let view = View::new(tree);
        let map = plan(&view, view.tree().root(), pane()).unwrap();

        assert!(map.tiles.iter().all(|tile| tile.kind == Kind::Unpriced));
        assert!((map.unknown.unwrap().size() - pane().size()).abs() < 1e-6);
        assert!(map.caption.contains("2 unpriced"), "{}", map.caption);
        // `> 0 B` and not `0 B`: the map of a tree nobody has measured must not read as a map
        // of a tree with nothing in it.
        assert!(map.caption.contains("> "), "{}", map.caption);
    }

    #[test]
    fn a_partly_priced_directory_appears_in_both_regions() {
        let mut tree = Tree::new("/scan");
        tree.insert(priced("/scan/a/node_modules", 4 * 1024 * 1024));
        tree.insert(hit("/scan/a/target", Size::Unmeasured, 0));
        let view = View::new(tree);
        let map = plan(&view, view.tree().root(), pane()).unwrap();

        let named: Vec<Kind> = map
            .tiles
            .iter()
            .filter(|tile| tile.name.starts_with("a/"))
            .map(|tile| tile.kind)
            .collect();
        assert!(named.contains(&Kind::Priced), "{named:?}");
        assert!(named.contains(&Kind::Unpriced), "{named:?}");
    }

    #[test]
    fn the_map_follows_the_cursor_and_a_leaf_maps_the_level_it_lives_in() {
        let mut view = view();
        // Row 0 is the scan root, which has children, so it maps itself.
        assert_eq!(focus(&view), Some(view.tree().root()));

        view.apply(Action::Cursor(Motion::Down));
        view.apply(Action::Expand);
        let a = view.tree().find(std::path::Path::new("/scan/a")).unwrap();
        assert_eq!(focus(&view), Some(a));

        // …and on the claim itself, which is a leaf: a map of one rectangle says nothing, so
        // it shows the level the claim is in and marks where the cursor is.
        view.apply(Action::Cursor(Motion::Down));
        assert_eq!(focus(&view), Some(a));
        let claim = view
            .tree()
            .find(std::path::Path::new("/scan/a/node_modules"))
            .unwrap();
        let map = plan(&view, a, pane()).unwrap();
        assert!(
            map.tiles.iter().any(|tile| tile.id == claim && tile.cursor),
            "nothing on the map says where the cursor is: {:?}",
            map.tiles
        );
    }

    #[test]
    fn a_marked_subtree_is_marked_on_the_map_too() {
        let mut view = view();
        view.apply(Action::Cursor(Motion::Down));
        view.apply(Action::Mark);
        let map = plan(&view, view.tree().root(), pane()).unwrap();

        let marked: Vec<&str> = map
            .tiles
            .iter()
            .filter(|tile| tile.marked)
            .map(|tile| tile.name.as_str())
            .collect();
        assert_eq!(marked, ["a/node_modules"], "{:?}", map.tiles);
    }

    #[test]
    fn a_filter_maps_what_it_shows_and_not_what_is_there() {
        // The safety property from #602 finding 5, in a second place: a rectangle drawn for
        // bytes the filter is hiding is a rectangle whose mark would delete them.
        let mut view = view();
        view.apply(Action::OpenFilter);
        for character in "target".chars() {
            view.apply(Action::Type(character));
        }
        view.apply(Action::Submit);
        let map = plan(&view, view.tree().root(), pane()).unwrap();

        let names: Vec<&str> = map.tiles.iter().map(|tile| tile.name.as_str()).collect();
        assert_eq!(names, ["b/target"], "{:?}", map.tiles);
    }

    #[test]
    fn there_is_no_map_of_a_directory_with_nothing_under_it() {
        let empty = View::new(Tree::new("/scan"));
        assert_eq!(plan(&empty, empty.tree().root(), pane()), None);
        // …nor of a pane with no room in it, which is what a very narrow terminal gives.
        let view = view();
        assert_eq!(plan(&view, view.tree().root(), Area::of(0.0, 40.0)), None);
    }
}
