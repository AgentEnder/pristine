//! The treemap pane — **a spike**, and its escape hatch is [`Maps`] not being [`Maps::Can`].
//!
//! Reclaimable space is spatial, and a treemap answers "where are the bytes" in a way a
//! sorted list structurally cannot: `~/repos/archived` being two thirds of the picture is one
//! glance, where the list makes it one number to compare against forty others. kondo, npkill
//! and `dua` all render rows and nothing else, so this is also the one thing on the table
//! that would make pristine visibly *unlike* them rather than better along an axis they
//! already occupy.
//!
//! # It degrades to nothing, three times over
//!
//! 1. **The terminal has to be known to speak the protocol**, from the environment and
//!    nothing else — see [`super::chrome::Decor`], whose table this reads a column of. There
//!    is a documented "do you speak this" query and it is a round trip with no bound on the
//!    silence, which is exactly the blocking probe the chrome refuses to make.
//! 2. **The terminal has to report a pixel size.** `TIOCGWINSZ` carries one and costs no
//!    round trip, so a terminal that fills it in with zeros — which is most of them, and
//!    every terminal seen through tmux — gets no map rather than a guess at its own cell
//!    size.
//! 3. **The pane has to fit.** Below [`MIN_WIDTH`] columns the map would cost the tree more
//!    than it is worth, and the tree alone is the complete interface.
//!
//! **The first two are one answer, [`Maps`], and that is #656's whole lesson.** They were two
//! answers taken at two different times — the allowlist before the layout, the pixel size at
//! the draw — so a terminal that passed one and failed the other cost the tree columns that
//! nothing was ever drawn in. Both are now folded into the predicate the layout reads, and it
//! is re-asked every frame, because a window can lose its pixel fields while a run is going.
//!
//! Nothing above is a flag the reader has to find. What *is* a key is `m`, which turns the
//! pane off on a terminal that could have one — and on a terminal that could not, says which
//! of the two reasons it is, because an enhancement you cannot dismiss is not an enhancement
//! and a rectangle that declines without a word is worse than no rectangle.
//!
//! # What is expensive, and what is done about it
//!
//! A pane of 44×40 cells on a retina terminal is about 900 kB of RGB, which is 1.2 MB of
//! base64 down the pty. At the 100 ms frame rate that would be 12 MB/s to say nothing new, so
//! the image is emitted only when the picture actually **changes** — and the two kinds of
//! change are treated differently, because they have different deadlines:
//!
//! - **Steering** — the cursor moving, a drill-in, a mark, a filter, the pane resizing — is
//!   the reader's own hand and is redrawn on the next frame, always.
//! - **Arriving** — a price landing, a claim appearing, a row being deleted — happens
//!   hundreds of times a second during a breakdown and is redrawn at most every
//!   [`SETTLE`]. A map that repaints 10 times a second while 16,013 prices land is a map
//!   nobody can read anyway.
//!
//! **"Has it changed" is answered from what the map is made of, never from the map.** The
//! spike asked it by squarifying the whole thing and comparing, which cost 467 µs on a frame
//! where nothing had happened — 200× what the animation beside it spends to answer the same
//! question, paid forever, on a pane showing the picture it showed last frame. Reading the
//! inputs instead costs **50 ns**: a [`View::map_stamp`](super::state::View::map_stamp) for
//! the mapped subtree, and a hash of the handful of values the reader controls.
//!
//! That stamp is **lens-aware**, and it has to be. A run opens on a view that hides the
//! gitignored tier, so tier-two claims stream in under the very directory the map is of while
//! changing not one rectangle; answering each of those with the tree's own stamp would be a
//! megabyte down the pty to redraw the picture already on it.
//!
//! Taking the picture **down** is not a redraw and is never throttled — see
//! [`tiles::mappable`].
//!
//! See the note in brain — `areas/pristine/design/2026-08-11-treemap-spike.md` — for what
//! this measured out at, and for the verdict.

pub mod kitty;
pub mod paint;
pub mod tiles;

use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Write};
use std::time::{Duration, Instant};

use ratatui::layout::Rect;

use super::state::View;
use kitty::Image;
use tiles::Area;

/// The narrowest terminal that gets a map.
///
/// The pane costs the tree the columns it takes, and below this the tree is left too narrow
/// to read a path in — which is the thing the map is an enhancement *to*.
pub const MIN_WIDTH: u16 = 100;

/// The shortest pane worth drawing rectangles in.
pub const MIN_HEIGHT: u16 = 12;

/// How wide the pane is, as a share of the terminal, and the bounds on that.
const SHARE: (f32, u16, u16) = (0.36, 32, 56);

/// How long a map has to have been up before something *arriving* redraws it.
///
/// Not a frame-rate cap: steering ignores it entirely. It bounds only the redraws nobody
/// asked for, which during a breakdown is all of them.
const SETTLE: Duration = Duration::from_millis(250);

/// Whether a map can appear in this terminal right now, and when it cannot, why.
///
/// **One answer, because there were two and they were asked at different times.** The
/// allowlist was read before the layout and the pixel size at the draw, so a terminal that
/// passed the first and failed the second cost the tree its columns and then had nothing drawn
/// in them. Neither gate was wrong; only one of them was visible to the layout.
///
/// The case that separates them is a multiplexer carrying the outer terminal's `TERM` through
/// — tmux with `default-terminal "xterm-ghostty"`, which is what a workspace manager sets up.
/// [`super::chrome::Decor`] then reads Ghostty and says yes, and tmux forwards no pixel fields
/// at all, so the winsize reads zero. A bare `TERM=tmux-256color` never got this far: the
/// allowlist refuses it, which is [`Maps::Unread`] and a different sentence.
///
/// So this is the *only* predicate the layout reads, and it carries the reason with it,
/// because a bool would take the pane away and leave nobody able to say why it went.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Maps {
    /// The terminal reads the protocol and says how big its cells are.
    Can,
    /// It is not known to read the graphics protocol. The default, because a view nobody has
    /// told anything to has not been told this either.
    #[default]
    Unread,
    /// It reads the protocol but reports no pixel size.
    ///
    /// Named for [`crate::size::Size::Unmeasured`] and for the same reason: there is no
    /// honest cell size to be had here, and the answer to an absent measurement is to say it
    /// is absent rather than to assume 8×16 and draw an image at the wrong scale over the
    /// text it is meant to sit beside.
    Unmeasured,
}

impl Maps {
    /// Both gates in one answer: what the terminal is, and what it has just said about its
    /// own window. Reached through [`Screen::mapping`], which is the only caller that holds
    /// both halves.
    fn of(reads: bool, cell: Option<(u16, u16)>) -> Self {
        match (reads, cell) {
            (false, _) => Self::Unread,
            (true, None) => Self::Unmeasured,
            (true, Some(_)) => Self::Can,
        }
    }

    /// Whether a map can be drawn.
    #[must_use]
    pub fn can(self) -> bool {
        matches!(self, Self::Can)
    }

    /// The one line for a reader who wanted a map there cannot be, or `None` when there can.
    ///
    /// Two sentences and not one, because the two refusals have different answers: a terminal
    /// off the allowlist is the wrong terminal, where a terminal reporting no pixel size is
    /// usually the right one with a multiplexer in between — and that is something the reader
    /// can act on.
    #[must_use]
    pub fn why(self) -> Option<&'static str> {
        match self {
            Self::Can => None,
            Self::Unread => {
                Some("this terminal does not read the graphics protocol, so there is no map")
            }
            Self::Unmeasured => Some(
                "this terminal reports no pixel size — tmux and screen do not pass one on — \
                 so there is no map",
            ),
        }
    }
}

/// What a call to [`Screen::show`] left on the terminal.
///
/// An answer rather than `Ok(())`, because "there is no picture" and "the picture is already
/// right" are the same silence otherwise — which is exactly how #656 went unreported for as
/// long as it did. The caller can see which it got, and say so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drawn {
    /// A map is on the terminal: written by this call, or still right from an earlier one.
    Map,
    /// None, because there is nothing under the cursor worth dividing into rectangles — an
    /// empty directory, or one whose whole subtree the lens hides. Not a fault in anything.
    Nothing,
    /// None, because this terminal cannot have one — and which of the two reasons it is.
    ///
    /// **The layout should never have reserved a pane, and this is how the caller finds out
    /// that it did.** A run that reaches this has [`Maps`] and the pane it was handed
    /// disagreeing, which is #656's shape returning; the caller's job is to believe the
    /// screen, which has actually tried, over the layout, which has only asked.
    ///
    /// The reason travels with it rather than being assumed at the other end, because the
    /// caller assuming would be the wrong sentence under the pane on the terminal where the
    /// other reason was the true one.
    Cannot(Maps),
}

/// The pane the map goes in, in cells and in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pane {
    /// Where the image goes, in terminal cells.
    pub cells: Rect,
    /// How many pixels one cell is, across and down.
    pub cell: (u16, u16),
}

impl Pane {
    /// The pane's size in pixels, or `None` if the terminal reports no pixel size.
    ///
    /// A terminal that answers `TIOCGWINSZ` with zeros is one that does not know how big its
    /// own cells are, and an image sized from a guess is an image that does not line up with
    /// the text beside it.
    #[must_use]
    pub fn pixels(&self) -> Option<(u32, u32)> {
        if self.cell.0 == 0 || self.cell.1 == 0 {
            return None;
        }
        let across = u32::from(self.cells.width) * u32::from(self.cell.0);
        let down = u32::from(self.cells.height) * u32::from(self.cell.1);
        (across > 0 && down > 0).then_some((across, down))
    }

    /// How wide a map pane should be beside a tree in a terminal `width` columns across, or
    /// `None` when there is not room for both.
    #[must_use]
    pub fn width_in(width: u16) -> Option<u16> {
        if width < MIN_WIDTH {
            return None;
        }
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a share of a terminal width, clamped into u16 bounds either side"
        )]
        let want = (f32::from(width) * SHARE.0) as u16;
        Some(want.clamp(SHARE.1, SHARE.2).min(width / 2))
    }
}

/// The image on the terminal, and the promise to take it back.
///
/// Generic over its sink for [`super::chrome::Chrome`]'s reason, which is sharper here: every
/// byte this writes is invisible to every other kind of test, and the one that matters most —
/// an image left in the terminal's memory after the process has gone — is invisible to the
/// *reader* too.
#[derive(Debug)]
pub struct Screen<W: Write> {
    out: W,
    /// Whether this terminal is known to read the protocol at all.
    ///
    /// Half of [`Screen::mapping`]'s answer and the last gate rather than the only one: the
    /// view holds the reader's `m` and the renderer holds whether there is room, and both of
    /// those ask that first. Checked again in [`Screen::show`] because it is the one that must
    /// never be got wrong — a byte of this written to a terminal that cannot decode it is
    /// base64 in somebody's scrollback.
    allowed: bool,
    /// Whether there is an image on the terminal right now.
    up: bool,
    /// What the reader's own hand had set when the picture went up: which directory, which
    /// row, what is marked, what the filter shows, how big the pane. Changes to this are
    /// never throttled.
    steering: u64,
    /// What the mapped subtree was showing when the picture went up. See
    /// [`View::map_stamp`].
    arriving: u64,
    /// When the image was last written.
    since: Option<Instant>,
}

impl<W: Write> Screen<W> {
    /// A screen that will draw if `allowed`, and never otherwise.
    pub fn new(out: W, allowed: bool) -> Self {
        Self {
            out,
            allowed,
            up: false,
            steering: 0,
            arriving: 0,
            since: None,
        }
    }

    /// Whether a map could appear at all, given what the terminal has just said one cell
    /// measures — `None` when it will not say.
    ///
    /// The one predicate, asked here rather than in two places: this is the half of the
    /// answer only the screen knows, and the cell size is the half only the terminal knows,
    /// and #656 was those two halves being combined nowhere. Asked **every frame**, because
    /// only one of them is a constant: a window can lose its pixel fields without the program
    /// at the other end changing — a tmux client attaching, a pane moving between displays.
    #[must_use]
    pub fn mapping(&self, cell: Option<(u16, u16)>) -> Maps {
        Maps::of(self.allowed, cell)
    }

    /// Draws the map of whatever the cursor is on, if anything has changed since the last one.
    ///
    /// Answers what it left on the screen rather than `Ok(())`: see [`Drawn`], and #656 for
    /// what a silent decline costs.
    ///
    /// # Errors
    ///
    /// Anything the terminal refuses.
    pub fn show(&mut self, view: &View, pane: Pane, now: Instant) -> io::Result<Drawn> {
        if !self.allowed {
            return Ok(Drawn::Cannot(Maps::Unread));
        }
        let Some((width, height)) = pane.pixels() else {
            self.hide()?;
            return Ok(Drawn::Cannot(Maps::Unmeasured));
        };
        let Some(root) = tiles::focus(view) else {
            self.hide()?;
            return Ok(Drawn::Nothing);
        };
        let area = Area::of(f64::from(width), f64::from(height));
        // Asked on every frame and never throttled, because it is not a redraw: a map of a
        // directory the deleter has just emptied is a picture of something that is no longer
        // there, and on this tool that is a picture of what was about to be deleted. It is
        // also free — see [`tiles::mappable`].
        if !tiles::mappable(view, root, area) {
            self.hide()?;
            return Ok(Drawn::Nothing);
        }

        // Two fingerprints, because the two kinds of change have different deadlines. The
        // reader's hand is answered on the next frame; the pool's arrivals wait for `SETTLE`,
        // which is what stops 16,013 prices from each buying a megabyte of redraw.
        //
        // Both are taken from what the map is made **of** rather than from the map, and
        // that is the whole of this. Asking "did the picture change" by building the picture
        // and comparing costs a squarify, a collapse and two strings per rectangle on every
        // frame forever — 467 µs against the 2 µs the animation beside it spends to answer
        // the same question, on a pane showing what it showed last frame.
        //
        // The inputs are: which directory is mapped, where the cursor is, what is marked,
        // what the lens shows, how big the pane is, and whether anything under the mapped
        // directory has moved. Nothing else reaches [`tiles::plan`] — the order the tree
        // holds its children in does not, because the map sorts its own rectangles by weight.
        let steering = fingerprint(&(
            root,
            // By `NodeId` and never by row index: rows re-sort as prices land, so an index
            // that stayed the same names a different directory, and one that changed names
            // the same one.
            view.row().map(|row| row.id),
            view.mark_stamp(),
            // The whole lens and not just its `/` pattern: the tier and kind axes decide what
            // [`View::roll`] counts, so a rectangle's area is as much theirs as the pattern's.
            view.lens(),
            pane.cells,
            pane.cell,
        ));
        // The map's own stamp and not the tree's: the tree's is lens-blind, and a run opens on
        // a view that hides a whole tier. See [`View::map_stamp`].
        let arriving = view.map_stamp(root);
        let steered = steering != self.steering;
        let settled = self
            .since
            .is_none_or(|last| now.saturating_duration_since(last) >= SETTLE);
        // Nothing the map is drawn from has moved, so what is on the terminal is still the
        // right picture — and it is the still frame, which is nearly all of them.
        if self.up && !steered && arriving == self.arriving {
            return Ok(Drawn::Map);
        }
        if self.up && !steered && !settled {
            return Ok(Drawn::Map);
        }

        // Only now, once something is known to have changed, is the map worth laying out.
        let Some(map) = tiles::plan(view, root, area) else {
            self.hide()?;
            return Ok(Drawn::Nothing);
        };
        let canvas = paint::paint(&map, width, height);
        let at = (pane.cells.y + 1, pane.cells.x + 1);
        let cells = (pane.cells.width, pane.cells.height);
        // Armed **before** the write, which is [`super::chrome::Chrome::enter`]'s rule and
        // is load-bearing here for a sharper version of its reason. `put` writes the whole
        // image and then flushes, so a flush that fails has left a megabyte in the
        // terminal's memory — and a flag set afterwards would still say there is nothing to
        // take back. Of the two ways to be wrong, deleting an image that never landed costs
        // twenty bytes at a terminal already being restored, while skipping the delete
        // leaves the picture there after this process has gone, with nothing alive to notice.
        self.up = true;
        self.put(&Image::shown(&canvas, at, cells))?;
        // The fingerprints only afterwards, and that is the other half: a write that failed
        // has left the screen in a state this cannot describe, so the next `show` has to
        // treat it as a picture it has not drawn and send it again.
        self.steering = steering;
        self.arriving = arriving;
        self.since = Some(now);
        Ok(Drawn::Map)
    }

    /// Takes the image down, if one is up.
    ///
    /// Called whenever the map cannot be right: an overlay is over it, the terminal has no
    /// room, or the reader turned it off. Forgetting the fingerprint with it is the part that
    /// is easy to miss — a map hidden behind the help page and then unhidden is the *same*
    /// picture, so a `show` that only compared fingerprints would leave the pane blank for
    /// as long as nothing else changed.
    ///
    /// # Errors
    ///
    /// Anything the terminal refuses.
    pub fn hide(&mut self) -> io::Result<()> {
        if !self.up {
            return Ok(());
        }
        self.steering = 0;
        self.arriving = 0;
        self.since = None;
        self.put(&Image::gone())?;
        // Cleared only once the delete has actually gone out, which is the mirror of the
        // arming in [`Screen::show`]. A terminal that refused thirty bytes once will often
        // take them a moment later, and this runs twice by design — the ordinary way out and
        // then the guard's `Drop` — so the second pass is a free retry. Clearing the flag
        // first would spend it on a delete that never left.
        self.up = false;
        Ok(())
    }

    /// Puts back everything this took, and can be called twice.
    ///
    /// The state being given back here lives in **another program's memory**: an image is
    /// stored by the terminal, and a process that exits without deleting one leaves a
    /// megabyte behind with nothing alive to notice. #619's rule — a state that cannot be
    /// given back is a state you do not take — with the same answer, an id and a delete.
    ///
    /// # Errors
    ///
    /// Anything the terminal refuses.
    pub fn restore(&mut self) -> io::Result<()> {
        self.hide()
    }

    fn put(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.out.flush()
    }

    /// What has been written, for the tests that are about exactly that.
    #[cfg(test)]
    pub(crate) fn sink(&self) -> &W {
        &self.out
    }
}

/// One number standing for a value, for "has this changed since last frame".
fn fingerprint(of: &impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    of.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::{Drawn, MIN_WIDTH, Maps, Pane, SETTLE, Screen, kitty, paint, tiles};
    use crate::fixture::{gitignored, hit, priced};
    use crate::size::Size;
    use crate::tree::Tree;
    use crate::tui::keymap::{Action, Motion};
    use crate::tui::state::View;
    use crate::tui::treemap::tiles::Area;
    use ratatui::layout::Rect;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// A view with the map turned on, which is the only kind that draws one.
    ///
    /// Said out loud in every fixture here rather than defaulted, because it is what decides
    /// whether [`View::map_stamp`] has its lens-aware table behind it or falls back to the
    /// tree's lens-blind one — so a test that left it off would be asserting about a run
    /// nobody has.
    fn view() -> View {
        let mut tree = Tree::new("/scan");
        tree.insert(priced("/scan/nx/node_modules", 8 * 1024 * 1024));
        tree.insert(priced("/scan/pua/target", 2 * 1024 * 1024));
        let mut view = View::new(tree);
        view.allow_maps(Maps::Can);
        view.sync();
        view
    }

    fn pane() -> Pane {
        Pane {
            cells: Rect::new(60, 1, 40, 30),
            cell: (9, 19),
        }
    }

    fn screen() -> Screen<Vec<u8>> {
        Screen::new(Vec::new(), true)
    }

    fn written(screen: &Screen<Vec<u8>>) -> String {
        String::from_utf8_lossy(screen.sink()).into_owned()
    }

    #[test]
    fn a_terminal_that_is_not_known_to_read_the_protocol_is_written_nothing() {
        // The property the whole module rests on, and the one the task made a hard
        // requirement: not one byte reaches a terminal that might not understand it.
        let mut screen = Screen::new(Vec::new(), false);
        let view = view();
        screen.show(&view, pane(), Instant::now()).unwrap();
        screen.hide().unwrap();
        screen.restore().unwrap();

        assert_eq!(written(&screen), "", "an escape reached a terminal");
        // …and a pixel size it *does* report changes nothing: the allowlist is the gate this
        // one fails, and the answer names which.
        assert_eq!(screen.mapping(Some((9, 19))), Maps::Unread);
    }

    #[test]
    fn a_terminal_that_reports_no_pixel_size_gets_no_map() {
        // Most terminals answer `TIOCGWINSZ` with zeros for the pixel fields. An image sized
        // from a guess at the cell size is one that does not line up with the text beside it.
        let mut screen = screen();
        let view = view();
        let drawn = screen
            .show(
                &view,
                Pane {
                    cell: (0, 0),
                    ..pane()
                },
                Instant::now(),
            )
            .unwrap();
        assert_eq!(written(&screen), "");
        // #656's second half: the caller is told it drew nothing. This returning `Ok(())` was
        // indistinguishable from the still frame below, which is why a pane that never got a
        // picture looked exactly like one that did not need a new one.
        assert_eq!(drawn, Drawn::Cannot(Maps::Unmeasured));
    }

    #[test]
    fn the_pixel_size_is_the_same_gate_as_the_allowlist_and_not_a_later_one() {
        // #656. The allowlist is a fact about the program at the other end and the pixel size
        // is a fact about its window, and the bug was that only the first reached the layout:
        // inside tmux the outer terminal passes the allowlist while the winsize pixel fields
        // are zero, so the tree paid 36 columns for a picture the draw then refused to make.
        //
        // One predicate now answers both, and it says *which* — because taking the pane away
        // silently is the same failure in the other direction.
        let screen = screen();
        assert_eq!(screen.mapping(Some((9, 19))), Maps::Can);
        assert_eq!(screen.mapping(None), Maps::Unmeasured);
        assert!(Maps::Can.can());
        assert!(!Maps::Unmeasured.can() && !Maps::Unread.can());

        // Two reasons and two sentences: a reader inside tmux has something to act on, and a
        // reader on a terminal that will never read the protocol does not.
        let unmeasured = Maps::Unmeasured.why().unwrap();
        assert!(unmeasured.contains("pixel size"), "{unmeasured}");
        assert_ne!(unmeasured, Maps::Unread.why().unwrap());
        assert_eq!(Maps::Can.why(), None);
        // A view nobody has told anything to has not been told this either.
        assert_eq!(Maps::default(), Maps::Unread);
    }

    #[test]
    fn a_still_frame_says_the_map_is_up_rather_than_saying_nothing_at_all() {
        // The distinction [`Drawn`] exists for. Both of these wrote no bytes, and until they
        // answered they were the same `Ok(())`: one is the picture already being right, the
        // other is there being no picture at all.
        let mut screen = screen();
        let view = view();
        let now = Instant::now();
        assert_eq!(screen.show(&view, pane(), now).unwrap(), Drawn::Map);
        let first = screen.sink().len();
        assert_eq!(screen.show(&view, pane(), now).unwrap(), Drawn::Map);
        assert_eq!(
            screen.sink().len(),
            first,
            "the map was redrawn for nothing"
        );

        // And the third answer, which is neither: the terminal could draw one and the tree has
        // nothing to divide into rectangles. Not a fault in anything, so the caller must not
        // read it as the pane declining.
        let empty = View::new(Tree::new("/scan"));
        assert_eq!(screen.show(&empty, pane(), now).unwrap(), Drawn::Nothing);
    }

    #[test]
    fn the_pane_gives_way_to_the_tree_rather_than_the_other_way_round() {
        assert_eq!(Pane::width_in(MIN_WIDTH - 1), None);
        // Wide enough for both, and never more than half — the tree is the interface and
        // this is an enhancement to it.
        let wide = Pane::width_in(200).unwrap();
        assert!(wide <= 56, "{wide} columns of a 200-column terminal");
        assert!(Pane::width_in(MIN_WIDTH).unwrap() <= MIN_WIDTH / 2);
    }

    #[test]
    fn a_map_is_drawn_once_and_not_again_until_the_picture_changes() {
        let mut screen = screen();
        let mut view = view();
        let now = Instant::now();
        screen.show(&view, pane(), now).unwrap();
        let first = screen.sink().len();
        assert!(first > 1000, "nothing was drawn");

        // A frame in which nothing happened. The whole reason this is affordable at 10 fps.
        screen.show(&view, pane(), now).unwrap();
        assert_eq!(
            screen.sink().len(),
            first,
            "the map was redrawn for nothing"
        );

        // The reader's own hand, which is never throttled: the same instant, and it redraws.
        view.apply(Action::Cursor(Motion::Down));
        screen.show(&view, pane(), now).unwrap();
        assert!(screen.sink().len() > first, "steering did not redraw");
    }

    #[test]
    fn a_price_landing_waits_for_the_map_to_settle_and_the_cursor_never_does() {
        let mut tree = Tree::new("/scan");
        tree.insert(priced("/scan/nx/node_modules", 8 * 1024 * 1024));
        tree.insert(hit("/scan/pua/target", Size::Unmeasured, 0));
        let mut view = View::new(tree);
        view.allow_maps(Maps::Can);
        view.sync();
        let mut screen = screen();
        let now = Instant::now();
        screen.show(&view, pane(), now).unwrap();
        let first = screen.sink().len();

        // 16,013 of these arrive over a minute during a breakdown. Answering each one costs
        // a megabyte to say something the reader cannot read at that rate anyway.
        view.priced(
            std::path::Path::new("/scan/pua/target"),
            Size::Measured(4096),
        );
        view.sync();
        screen
            .show(&view, pane(), now + Duration::from_millis(30))
            .unwrap();
        assert_eq!(screen.sink().len(), first, "an arrival redrew immediately");

        screen.show(&view, pane(), now + SETTLE).unwrap();
        assert!(screen.sink().len() > first, "the map never caught up");
    }

    #[test]
    fn the_map_of_a_directory_that_has_been_deleted_comes_down_without_waiting_to_settle() {
        // [`SETTLE`] holds back *redraws*, and taking the picture down is not one. The
        // difference is the whole of what the pane is for: a map that is 250 ms late is a map
        // nobody notices, and a map of a directory that is no longer on the disk is a picture
        // of what was about to be deleted, still up after it has gone.
        let mut tree = Tree::new("/scan");
        tree.insert(priced("/scan/only/node_modules", 8 * 1024 * 1024));
        let mut view = View::new(tree);
        view.allow_maps(Maps::Can);
        view.sync();
        let mut screen = screen();
        let now = Instant::now();
        view.animate(now);
        screen.show(&view, pane(), now).unwrap();
        let before = screen.sink().len();

        view.removed(
            std::path::Path::new("/scan/only/node_modules"),
            8 * 1024 * 1024,
            true,
        );
        let later = now + crate::tui::moving::DIM;
        view.animate(later);

        // Well inside the settle, which is what an arrival would be made to wait for.
        assert!(crate::tui::moving::DIM < SETTLE);
        screen.show(&view, pane(), later).unwrap();
        let said = written(&screen);
        assert!(
            said.len() > before && said.ends_with("d=I,i=1976622,q=2\x1b\\"),
            "the map outlived the directory it was of"
        );
    }

    #[test]
    fn an_arrival_outside_the_mapped_directory_is_not_a_redraw() {
        // The property that makes this affordable at all, and the one a fingerprint taken
        // over the whole view rather than over the mapped subtree would lose: during a
        // breakdown 16,013 prices land, and the reader is looking at one directory. A
        // megabyte spent redrawing a picture that did not change is the cost this whole pane
        // is arguing with.
        let mut tree = Tree::new("/scan");
        tree.insert(priced("/scan/here/one/node_modules", 4 * 1024 * 1024));
        tree.insert(priced("/scan/here/two/node_modules", 2 * 1024 * 1024));
        tree.insert(priced("/scan/there/target", 1024));
        let mut view = View::new(tree);
        view.allow_maps(Maps::Can);
        view.sync();
        view.apply(Action::Cursor(Motion::Down));
        let here = view
            .tree()
            .find(std::path::Path::new("/scan/here"))
            .unwrap();
        assert_eq!(tiles::focus(&view), Some(here));

        let mut screen = screen();
        let now = Instant::now();
        screen.show(&view, pane(), now).unwrap();
        let first = screen.sink().len();

        // Big enough to sort above the mapped directory, so the row the cursor is on lands at
        // a different index: rows are named by `NodeId` and never by where they happen to be
        // this frame, here as everywhere else.
        view.found(priced("/scan/there/huge/node_modules", 64 * 1024 * 1024));
        view.sync();
        assert_eq!(
            tiles::focus(&view),
            Some(here),
            "the map moved off its own directory"
        );

        screen.show(&view, pane(), now + SETTLE).unwrap();
        assert_eq!(
            screen.sink().len(),
            first,
            "a claim landing somewhere else redrew a picture that did not change"
        );
    }

    #[test]
    fn a_mark_is_the_readers_own_hand_rather_than_an_arrival_and_is_not_made_to_wait() {
        let mut screen = screen();
        let mut view = view();
        let now = Instant::now();
        view.apply(Action::Cursor(Motion::Down));
        screen.show(&view, pane(), now).unwrap();
        let before = screen.sink().len();

        // A mark moves no bytes and no claims — it turns a rectangle aqua — so nothing the
        // tree reports would say the picture changed. It is still the reader's own hand, and
        // [`SETTLE`] is for the arrivals nobody asked for.
        view.apply(Action::Mark);
        screen.show(&view, pane(), now).unwrap();
        assert!(
            screen.sink().len() > before,
            "a mark waited for a settle that is not for it"
        );
    }

    #[test]
    fn a_filter_redraws_the_map_it_narrows_at_once() {
        let mut screen = screen();
        let mut view = view();
        let now = Instant::now();
        screen.show(&view, pane(), now).unwrap();
        let before = screen.sink().len();

        // Half the rectangles stop existing and the tree behind them never moved. The reader
        // typed this, so it is answered on the next frame.
        view.apply(Action::OpenFilter);
        for character in "target".chars() {
            view.apply(Action::Type(character));
        }
        view.apply(Action::Submit);
        screen.show(&view, pane(), now).unwrap();
        assert!(
            screen.sink().len() > before,
            "the map went on showing what the filter took away"
        );
    }

    #[test]
    fn a_claim_the_view_hides_arriving_under_the_mapped_directory_is_not_a_redraw() {
        // The lens-blind half of the tree's own stamp, and the case a run meets from its
        // first frame: `default` hides the gitignored tier, so tier-two claims stream in
        // under the very directory the map is of while changing not one rectangle. Answering
        // each of those is a megabyte down the pty to redraw the picture already on it.
        let mut tree = Tree::new("/scan");
        tree.insert(priced("/scan/here/one/node_modules", 4 * 1024 * 1024));
        tree.insert(priced("/scan/here/two/node_modules", 2 * 1024 * 1024));
        let mut view = View::new(tree);
        view.allow_maps(Maps::Can);
        view.sync();
        view.apply(Action::Cursor(Motion::Down));
        let here = view
            .tree()
            .find(std::path::Path::new("/scan/here"))
            .unwrap();
        assert_eq!(tiles::focus(&view), Some(here));

        let mut screen = screen();
        let now = Instant::now();
        screen.show(&view, pane(), now).unwrap();
        let first = screen.sink().len();
        let was = view.roll(here);

        // Inside the mapped directory, and enormous — but the view a run opens on does not
        // show the gitignored tier, so the map has nothing to say about it.
        let mut unseen = gitignored("/scan/here/three/vendor");
        unseen.size = Size::Measured(64 * 1024 * 1024);
        view.found(unseen);
        view.sync();
        assert_eq!(
            view.roll(here),
            was,
            "the fixture no longer makes the point — the claim has to be invisible"
        );

        screen.show(&view, pane(), now + SETTLE).unwrap();
        assert_eq!(
            screen.sink().len(),
            first,
            "a claim the view hides redrew a map that cannot draw it"
        );

        // …and the moment the reader widens the view to include it, it is a new picture.
        view.apply(Action::CycleTiers);
        screen.show(&view, pane(), now + SETTLE).unwrap();
        assert!(
            screen.sink().len() > first,
            "the map never caught up with the view widening"
        );
    }

    #[test]
    fn narrowing_the_view_by_kind_redraws_the_map_it_narrows() {
        // The lens is more than its `/` pattern: what [`View::roll`] counts is the tier and
        // kind axes as well, so a rectangle's area is theirs too. A fingerprint that watched
        // only the pattern would leave `b` on a map of dependencies.
        let mut tree = Tree::new("/scan");
        tree.insert(priced("/scan/a/node_modules", 8 * 1024 * 1024));
        tree.insert(priced("/scan/b/target", 2 * 1024 * 1024));
        let mut view = View::new(tree);
        view.allow_maps(Maps::Can);
        view.sync();
        let mut screen = screen();
        let now = Instant::now();
        screen.show(&view, pane(), now).unwrap();
        let before = screen.sink().len();

        view.apply(Action::ToggleKind(crate::rules::Kind::Build));
        screen.show(&view, pane(), now).unwrap();
        assert!(
            screen.sink().len() > before,
            "the map went on drawing what the view stopped showing"
        );
    }

    #[test]
    fn a_claim_arriving_as_another_leaves_is_a_new_picture_even_though_the_totals_match() {
        // The trap in deriving the fingerprint from what the map is made *of*: the obvious
        // cheap summary — this subtree's bytes, claims and unpriced count — is three numbers
        // that a deletion and an arrival in the same frame put back exactly where they were.
        // A false "unchanged" here is a stale picture of a tree that has moved, which on this
        // tool is a stale picture of what is about to be deleted.
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/going/node_modules", Size::Unmeasured, 0));
        tree.insert(hit("/scan/staying/target", Size::Unmeasured, 0));
        let mut view = View::new(tree);
        view.allow_maps(Maps::Can);
        view.sync();
        let mut screen = screen();
        let now = Instant::now();
        view.animate(now);
        screen.show(&view, pane(), now).unwrap();
        let before = screen.sink().len();
        let totals = view.total();

        view.removed(std::path::Path::new("/scan/going/node_modules"), 0, true);
        // The drained row leaves the tree here, and the walk finds another in the same frame.
        let later = now + crate::tui::moving::DIM;
        view.animate(later);
        view.found(hit("/scan/arrived/node_modules", Size::Unmeasured, 0));
        view.sync();

        assert_eq!(
            view.total(),
            totals,
            "the fixture no longer makes the point — the totals have to be identical"
        );
        screen.show(&view, pane(), later + SETTLE).unwrap();
        assert!(
            screen.sink().len() > before,
            "the map is still drawing a directory that has been deleted"
        );
    }

    #[test]
    fn cycling_the_sort_is_not_a_new_picture() {
        // The one input left out of the fingerprint on purpose, so it is asserted rather than
        // assumed: the map orders its own rectangles by weight with the id breaking ties, so
        // what order the tree holds its children in is not something the picture can see.
        let mut screen = screen();
        let mut view = view();
        let now = Instant::now();
        screen.show(&view, pane(), now).unwrap();
        let before = screen.sink().len();

        view.apply(Action::CycleSort);
        view.sync();
        screen.show(&view, pane(), now + SETTLE).unwrap();
        assert_eq!(
            screen.sink().len(),
            before,
            "re-sorting the tree spent a megabyte on the same picture"
        );
    }

    /// A terminal that takes every byte and then refuses to flush them.
    ///
    /// The narrowest injection that reaches the finding: `write_all` succeeds, so the image
    /// really is in the terminal's memory, and only the flush fails.
    struct Unflushable {
        written: Vec<u8>,
        refusing: Arc<AtomicBool>,
    }

    impl std::io::Write for Unflushable {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            if self.refusing.load(Ordering::SeqCst) {
                return Err(std::io::Error::other("the terminal stopped listening"));
            }
            Ok(())
        }
    }

    #[test]
    fn an_image_the_terminal_took_but_would_not_flush_is_still_taken_back() {
        // The one failure that would leave a megabyte in somebody's terminal after this
        // process has gone. `write_all` puts the whole image there and the flush then fails,
        // so a `show` that armed the cleanup *after* the write would report the error and
        // leave `restore` with nothing to undo — the image outliving the run that made it,
        // with nothing alive to notice.
        let refusing = Arc::new(AtomicBool::new(true));
        let mut screen = Screen::new(
            Unflushable {
                written: Vec::new(),
                refusing: Arc::clone(&refusing),
            },
            true,
        );
        let view = view();

        assert!(
            screen.show(&view, pane(), Instant::now()).is_err(),
            "the flush was supposed to fail"
        );
        let sent = String::from_utf8_lossy(&screen.sink().written).into_owned();
        assert!(
            sent.contains("\x1b_Ga=T,"),
            "the image never reached the terminal, so there is nothing to prove"
        );

        // The way out, while the terminal is still refusing: the delete is attempted and
        // reported rather than swallowed, and — the half that matters — it is not written off
        // as done. This runs twice by design, so a refusal on the first pass has to leave
        // something for the second.
        assert!(
            screen.restore().is_err(),
            "a refused delete was called done"
        );

        // The terminal comes back, which is the ordinary shape of a transient write failure
        // on a pty whose buffer was full for a moment.
        refusing.store(false, Ordering::SeqCst);
        screen.restore().unwrap();

        let said = String::from_utf8_lossy(&screen.sink().written).into_owned();
        assert!(
            said.ends_with("d=I,i=1976622,q=2\x1b\\"),
            "the image was left in the terminal: {:?}",
            &said[said.len() - 60..]
        );
        // One in the placement's own prologue, then the two attempts on the way out.
        assert_eq!(said.matches("a=d,d=I").count(), 3, "unbalanced");
        // …and once it has genuinely gone, saying so again writes nothing.
        let settled = screen.sink().written.len();
        screen.restore().unwrap();
        assert_eq!(screen.sink().written.len(), settled, "deleted twice");
    }

    #[test]
    fn the_image_is_taken_back_on_the_way_out_and_hiding_it_forgets_what_was_up() {
        let mut screen = screen();
        let view = view();
        let now = Instant::now();
        screen.show(&view, pane(), now).unwrap();
        screen.restore().unwrap();
        assert!(
            written(&screen).ends_with("d=I,i=1976622,q=2\x1b\\"),
            "left behind"
        );

        let after = screen.sink().len();
        // Idempotent, because it runs from two places by design — the ordinary way out and
        // the guard that owns it being dropped by a `?` or a panic.
        screen.restore().unwrap();
        assert_eq!(screen.sink().len(), after, "taken back twice");

        // …and the same picture is drawn again afterwards. A `show` that only compared
        // fingerprints would leave the pane blank behind a closed help page.
        screen.show(&view, pane(), now).unwrap();
        assert!(screen.sink().len() > after, "the map never came back");
    }

    /// What one map costs, end to end, at the size a real terminal gives.
    ///
    /// `cargo test --release --lib measure_one_map -- --ignored --nocapture`. Kept as a test
    /// rather than written down once, because the number that decides whether this feature
    /// can live inside a 100 ms frame is a number the next person has to be able to re-take.
    #[test]
    #[ignore = "a measurement rather than an assertion; timings are not a pass or a fail"]
    fn measure_one_map() {
        // #602's own fixture shape, cut to the part a map ever looks at: the map draws one
        // level and what nests inside it, never the 22,765 rows behind it.
        let mut tree = Tree::new("/home");
        for repo in 0..300 {
            for pkg in 0..20_u64 {
                tree.insert(priced(
                    &format!("/home/repos/r{repo}/packages/p{pkg}/node_modules"),
                    4096 * (pkg + 1),
                ));
            }
        }
        for n in 0..8_660 {
            tree.insert(priced(&format!("/home/types/p{n}/node_modules"), 1024));
        }
        for n in 0..1_353 {
            tree.insert(hit(
                &format!("/home/cache/e{n}/target"),
                Size::Unmeasured,
                0,
            ));
        }
        let mut view = View::new(tree);
        view.allow_maps(Maps::Can);
        view.sync();
        view.viewport(50);
        // As a run that draws one has it, so the still frame below is measured against the
        // lens-aware stamp rather than the fallback. See [`View::map_stamp`].
        view.allow_maps(Maps::Can);
        view.sync();
        println!("claims: {}", view.total().claims);

        // 44 columns of a 120-column window, 34 rows, at a retina Ghostty's 9×19 px cell.
        let pane = Pane {
            cells: Rect::new(76, 2, 44, 34),
            cell: (9, 19),
        };
        let (width, height) = pane.pixels().unwrap();
        println!("pane: {width}×{height} px");

        let started = Instant::now();
        let root = tiles::focus(&view).unwrap();
        let map = tiles::plan(&view, root, Area::of(f64::from(width), f64::from(height))).unwrap();
        println!(
            "plan:            {:?} -> {} rectangles",
            started.elapsed(),
            map.tiles.len()
        );

        let started = Instant::now();
        let canvas = paint::paint(&map, width, height);
        println!(
            "paint:           {:?} -> {} px",
            started.elapsed(),
            canvas.rgb.len() / 3
        );

        let started = Instant::now();
        let bytes = kitty::Image::shown(&canvas, (3, 77), (44, 34));
        println!(
            "encode:          {:?} -> {} bytes down the pty",
            started.elapsed(),
            bytes.len()
        );

        // The frame that matters most: the one where nothing happened. A map redrawn ten
        // times a second to say nothing is the thing that would sink this.
        let mut screen = screen();
        let now = Instant::now();
        screen.show(&view, pane, now).unwrap();
        let started = Instant::now();
        for _ in 0..100 {
            screen.show(&view, pane, now).unwrap();
        }
        println!("100 still frames: {:?}", started.elapsed());

        // And the one where the reader moved.
        let started = Instant::now();
        view.apply(Action::Cursor(Motion::Down));
        screen.show(&view, pane, now).unwrap();
        println!("one steer:       {:?}", started.elapsed());
    }

    #[test]
    fn a_view_with_nothing_in_it_takes_the_map_down_rather_than_drawing_an_empty_one() {
        let mut screen = screen();
        let view = view();
        screen.show(&view, pane(), Instant::now()).unwrap();
        let before = screen.sink().len();

        // The cursor is on the scan root, which is a directory like any other — so the map
        // is refused by `plan` having nothing to divide rather than by there being nowhere
        // to point at. Both roads end at the image coming down.
        let empty = View::new(Tree::new("/scan"));
        assert_eq!(tiles::focus(&empty), Some(empty.tree().root()));
        screen.show(&empty, pane(), Instant::now()).unwrap();
        let said = written(&screen);
        assert!(said.len() > before, "the map was left showing a stale tree");
        assert!(
            said.ends_with("d=I,i=1976622,q=2\x1b\\"),
            "{}",
            &said[said.len() - 40..]
        );
    }
}
