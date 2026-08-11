//! The treemap pane — **a spike**, and its escape hatch is [`Screen::allowed`] being false.
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
//!    round trip, so a terminal that fills it in with zeros — which is most of them — gets no
//!    map rather than a guess at its own cell size.
//! 3. **The pane has to fit.** Below [`MIN_WIDTH`] columns the map would cost the tree more
//!    than it is worth, and the tree alone is the complete interface.
//!
//! Nothing above is a flag the reader has to find. What *is* a key is `m`, which turns the
//! pane off on a terminal that could have one — because an enhancement you cannot dismiss is
//! not an enhancement.
//!
//! # What is expensive, and what is done about it
//!
//! A pane of 44×40 cells on a retina terminal is about 900 kB of RGB, which is 1.2 MB of
//! base64 down the pty. At the 100 ms frame rate that would be 12 MB/s to say nothing new, so
//! the image is emitted only when the picture actually **changes** — and the two kinds of
//! change are treated differently, because they have different deadlines:
//!
//! - **Steering** — the cursor moving, a drill-in, the pane resizing — is the reader's own
//!   hand and is redrawn on the next frame, always.
//! - **Arriving** — a price landing, a claim appearing, a row being deleted — happens
//!   hundreds of times a second during a breakdown and is redrawn at most every
//!   [`SETTLE`]. A map that repaints 10 times a second while 16,013 prices land is a map
//!   nobody can read anyway.
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
    /// The last gate rather than the only one: the view holds the reader's `m` and the
    /// renderer holds whether there is room, and both of those ask this first. Checked again
    /// here because it is the one that must never be got wrong — a byte of this written to a
    /// terminal that cannot decode it is base64 in somebody's scrollback.
    allowed: bool,
    /// Whether there is an image on the terminal right now.
    up: bool,
    /// What the picture on screen is of — the whole map, so anything that changes it shows.
    picture: u64,
    /// What the reader's own hand has set: which directory, which row, how big the pane.
    /// Changes to this are never throttled.
    steering: u64,
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
            picture: 0,
            steering: 0,
            since: None,
        }
    }

    /// Whether a map could appear at all in this terminal.
    #[must_use]
    pub fn allowed(&self) -> bool {
        self.allowed
    }

    /// Draws the map of whatever the cursor is on, if anything has changed since the last one.
    ///
    /// # Errors
    ///
    /// Anything the terminal refuses.
    pub fn show(&mut self, view: &View, pane: Pane, now: Instant) -> io::Result<()> {
        if !self.allowed {
            return Ok(());
        }
        let Some((width, height)) = pane.pixels() else {
            return self.hide();
        };
        let Some(root) = tiles::focus(view) else {
            return self.hide();
        };
        let Some(map) = tiles::plan(view, root, Area::of(f64::from(width), f64::from(height)))
        else {
            return self.hide();
        };

        // Two fingerprints, because the two kinds of change have different deadlines. The
        // reader's hand is answered on the next frame; the pool's arrivals wait for `SETTLE`,
        // which is what stops 16,013 prices from each buying a megabyte of redraw.
        let steering = fingerprint(&(root, view.cursor(), pane.cells, pane.cell));
        let picture = fingerprint(&map);
        let steered = steering != self.steering;
        let settled = self
            .since
            .is_none_or(|last| now.saturating_duration_since(last) >= SETTLE);
        if self.up && picture == self.picture {
            return Ok(());
        }
        if self.up && !steered && !settled {
            return Ok(());
        }

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
        self.picture = picture;
        self.steering = steering;
        self.since = Some(now);
        Ok(())
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
        self.picture = 0;
        self.steering = 0;
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
    use super::{MIN_WIDTH, Pane, SETTLE, Screen, kitty, paint, tiles};
    use crate::fixture::{hit, priced};
    use crate::size::Size;
    use crate::tree::Tree;
    use crate::tui::keymap::{Action, Motion};
    use crate::tui::state::View;
    use crate::tui::treemap::tiles::Area;
    use ratatui::layout::Rect;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    fn view() -> View {
        let mut tree = Tree::new("/scan");
        tree.insert(priced("/scan/nx/node_modules", 8 * 1024 * 1024));
        tree.insert(priced("/scan/pua/target", 2 * 1024 * 1024));
        View::new(tree)
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
        assert!(!screen.allowed());
    }

    #[test]
    fn a_terminal_that_reports_no_pixel_size_gets_no_map() {
        // Most terminals answer `TIOCGWINSZ` with zeros for the pixel fields. An image sized
        // from a guess at the cell size is one that does not line up with the text beside it.
        let mut screen = screen();
        let view = view();
        screen
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
        screen
            .show(&view, pane(), now + Duration::from_millis(30))
            .unwrap();
        assert_eq!(screen.sink().len(), first, "an arrival redrew immediately");

        screen.show(&view, pane(), now + SETTLE).unwrap();
        assert!(screen.sink().len() > first, "the map never caught up");
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
        view.viewport(50);
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
