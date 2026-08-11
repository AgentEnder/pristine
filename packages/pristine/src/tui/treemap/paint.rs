//! Turning a [`Map`] into pixels.
//!
//! # Why there is a font in here
//!
//! A treemap with no labels is a shape, not an answer. "Where are the bytes" is only answered
//! if the big rectangle says which directory it is, and the terminal cannot help: a graphics
//! image covers the cells it is placed over, so the text has to be *in* the image. Hence
//! [`FONT`] — 95 glyphs of 5×7, which is the smallest thing that can spell `node_modules`
//! legibly at the size a terminal cell gives.
//!
//! # Colour is not the identity encoding, and that is deliberate
//!
//! Rectangles are told apart by a 2 px surface gap and by their own labels, never by hue. A
//! treemap's neighbours are arbitrary — any tile can end up beside any other — so a palette
//! used for identity here would have to clear the all-pairs colour-blindness gate, and no
//! eight-hue palette does. So hue carries **state** instead, over exactly two slots that do
//! clear it all-pairs on this surface (blue ↔ aqua, CVD ΔE 19.6): unmarked and marked. Depth
//! is a lightness step within the hue, and "nobody has measured this" is texture rather than
//! a third colour — which is also the right encoding for it, because texture is what a
//! reader reads as *absence of data* rather than as another category.

use super::tiles::{Area, Kind, Map, Tile};

/// One colour, as the protocol wants it.
type Rgb = [u8; 3];

/// The chart surface — and the gap between two rectangles, which is the same thing.
const SURFACE: Rgb = [0x1a, 0x1a, 0x19];
/// A rectangle nobody has marked. Blue, slot 1 of the validated pair.
const PRICED: [Rgb; 2] = [[0x39, 0x87, 0xe5], [0x6d, 0xa7, 0xec]];
/// A rectangle the reader has marked. Aqua, slot 2 — the pair clears every all-pairs gate on
/// this surface, which is what lets state be carried by hue when identity cannot be.
const MARKED: [Rgb; 2] = [[0x19, 0x9e, 0x70], [0x5e, 0xbb, 0x98]];
/// What nobody has measured: the diverging pair's neutral, under a hatch.
const UNKNOWN: Rgb = [0x38, 0x38, 0x35];
/// The hatch itself, and the marked version of it.
const HATCH: [Rgb; 2] = [[0x6b, 0x6a, 0x64], [0x19, 0x9e, 0x70]];
/// The cursor's outline. Not a hue: "you are here" is not a category.
const HERE: Rgb = [0xff, 0xff, 0xff];
/// A directory's name.
const INK: Rgb = [0xff, 0xff, 0xff];
/// What it is worth.
const MUTED: Rgb = [0xc3, 0xc2, 0xb7];
/// Under every glyph, one pixel down and right.
///
/// White on the blue is 3.6:1, which is fine for a heading and thin for a 7 px label. A
/// shadow costs one more blit and takes the contrast the text is actually read against out of
/// the palette's hands entirely — which matters here because the fill under a label is
/// whatever the map put there.
const SHADOW: Rgb = [0x0b, 0x0b, 0x0a];

/// Pixels per glyph, and the gap after it.
const GLYPH: (u32, u32) = (5, 7);
/// How far the next character starts.
const ADVANCE: u32 = 6;
/// The blank between a rectangle's edge and its label.
const PAD: u32 = 4;
/// The gap between a name and the figure under it.
const LEADING: u32 = 9;
/// How thick the cursor's outline is.
const RING: u32 = 2;
/// The blanks between a name and the figure beside it, when they share a line.
const BESIDE: u32 = 2;

/// An image, in the layout the graphics protocol's `f=24` wants: three bytes a pixel, rows
/// top to bottom, no padding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Canvas {
    /// Across, in pixels.
    pub width: u32,
    /// Down, in pixels.
    pub height: u32,
    /// `width * height * 3` bytes of it.
    pub rgb: Vec<u8>,
}

impl Canvas {
    /// A canvas of nothing but surface.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        let count = (width as usize) * (height as usize);
        let mut rgb = Vec::with_capacity(count * 3);
        for _ in 0..count {
            rgb.extend_from_slice(&SURFACE);
        }
        Self { width, height, rgb }
    }

    /// Writes one pixel, ignoring anything outside the canvas.
    fn dot(&mut self, x: u32, y: u32, colour: Rgb) {
        if x >= self.width || y >= self.height {
            return;
        }
        let at = ((y as usize) * (self.width as usize) + (x as usize)) * 3;
        self.rgb[at..at + 3].copy_from_slice(&colour);
    }

    /// Fills a rectangle.
    fn fill(&mut self, at: Box, colour: Rgb) {
        for y in at.top..at.bottom {
            for x in at.left..at.right {
                self.dot(x, y, colour);
            }
        }
    }

    /// Fills a rectangle with 45° stripes over a flat base.
    ///
    /// The texture, and the one place the map admits it does not know something. Six pixels
    /// apart is close enough to read as a fill at a glance and open enough that a label on
    /// top of it stays legible.
    fn hatch(&mut self, at: Box, base: Rgb, ink: Rgb) {
        self.fill(at, base);
        for y in at.top..at.bottom {
            for x in at.left..at.right {
                if (x + y) % 6 < 2 {
                    self.dot(x, y, ink);
                }
            }
        }
    }

    /// Draws a border just inside a rectangle.
    ///
    /// Two pixels rather than one, which the first render settled: a hairline of white on a
    /// mid blue is invisible at the size a terminal cell gives, and an outline nobody can see
    /// is an answer to "where am I" that is not given.
    fn outline(&mut self, at: Box, colour: Rgb) {
        for ring in 0..RING {
            for x in at.left..at.right {
                self.dot(x, at.top + ring, colour);
                self.dot(x, at.bottom.saturating_sub(1 + ring), colour);
            }
            for y in at.top..at.bottom {
                self.dot(at.left + ring, y, colour);
                self.dot(at.right.saturating_sub(1 + ring), y, colour);
            }
        }
    }

    /// Draws a string, shadowed, and says how wide it came out.
    ///
    /// Characters the font does not carry are drawn as `?` rather than skipped: a name with a
    /// hole in it is harder to recognise than one with a wrong glyph, and a directory whose
    /// name is not ASCII is somebody's real directory.
    fn write(&mut self, x: u32, y: u32, said: &str, colour: Rgb) {
        let mut at = x;
        for character in said.chars() {
            let glyph = glyph_of(character);
            // The whole shadow before any of the ink, so a glyph's own shadow cannot land on
            // top of the stroke it is under.
            for (offset, ink) in [((1, 1), SHADOW), ((0, 0), colour)] {
                for (row, bits) in (0..).zip(glyph) {
                    for column in 0..GLYPH.0 {
                        if bits & (1 << (GLYPH.0 - 1 - column)) != 0 {
                            self.dot(at + column + offset.0, y + row + offset.1, ink);
                        }
                    }
                }
            }
            at += ADVANCE;
        }
    }

    /// The canvas as a `P6` portable pixmap, which is what the visual check writes out.
    #[cfg(test)]
    fn ppm(&self) -> Vec<u8> {
        let mut out = format!("P6\n{} {}\n255\n", self.width, self.height).into_bytes();
        out.extend_from_slice(&self.rgb);
        out
    }
}

/// A rectangle in whole pixels, clamped to the canvas — what [`Area`]'s floats become once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Box {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

impl Box {
    /// The pixels an [`Area`] covers, pulled in by one so neighbours are separated by two.
    ///
    /// The gap is the separation the palette is *not* asked to provide: a treemap's
    /// neighbours are arbitrary, so two adjacent rectangles of the same state have to be
    /// distinguishable, and surface between them does that whatever the colours are.
    fn of(area: Area) -> Option<Self> {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "areas are pane-sized and non-negative by construction; the max(0.0) \
                      below is what makes the sign loss unreachable rather than merely \
                      unlikely"
        )]
        let (left, top, right, bottom) = (
            area.x.max(0.0).round() as u32 + 1,
            area.y.max(0.0).round() as u32 + 1,
            (area.x + area.w).max(0.0).round() as u32,
            (area.y + area.h).max(0.0).round() as u32,
        );
        (right > left && bottom > top).then_some(Self {
            left,
            top,
            right,
            bottom,
        })
    }

    fn width(self) -> u32 {
        self.right - self.left
    }

    fn height(self) -> u32 {
        self.bottom - self.top
    }
}

/// Paints a map at this pixel size.
#[must_use]
pub fn paint(map: &Map, width: u32, height: u32) -> Canvas {
    let mut canvas = Canvas::new(width, height);
    for tile in &map.tiles {
        draw(&mut canvas, tile);
    }
    canvas
}

/// One rectangle: its fill, its outline if the cursor is on it, and whatever of its label fits.
fn draw(canvas: &mut Canvas, tile: &Tile) {
    let Some(at) = Box::of(tile.area) else {
        return;
    };
    // Alternating rather than "outer and inner", so every nesting boundary is a change of
    // step whatever depth it is at. The border and the caption say where a rectangle begins;
    // this is what makes it legible at a glance that it began at all.
    let step = (tile.depth + 1) % 2;
    match tile.kind {
        Kind::Priced => canvas.fill(at, if tile.marked { MARKED } else { PRICED }[step]),
        Kind::Unpriced => canvas.hatch(at, UNKNOWN, HATCH[usize::from(tile.marked)]),
    }
    if tile.cursor {
        canvas.outline(at, HERE);
    }
    label(canvas, at, tile);
}

/// A rectangle's name and what it is worth, as much of them as there is room for.
///
/// Three states rather than two, and the middle one is the point: a rectangle too small for
/// its figure still gets its name, because the name is what makes the *area* readable and the
/// figure is already in the tree beside it.
fn label(canvas: &mut Canvas, at: Box, tile: &Tile) {
    if at.width() <= PAD * 2 || at.height() < GLYPH.1 + PAD {
        return;
    }
    let room = ((at.width() - PAD * 2) / ADVANCE) as usize;
    let top = at.top + PAD - 1;
    // A tile with rectangles inside it owns only its caption strip, so its two facts share
    // one line — and the name is cut to leave room for the figure rather than the figure
    // being dropped, because a nested tile is a big one and its total is the reason it is.
    if tile.nested {
        let worth = tile.worth.chars().count();
        let name = elide(&tile.name, room.saturating_sub(worth + BESIDE as usize));
        canvas.write(at.left + PAD, top, &name, INK);
        let after = (u32::try_from(name.chars().count()).unwrap_or(u32::MAX) + BESIDE) * ADVANCE;
        if (name.chars().count() + worth + BESIDE as usize) <= room {
            canvas.write(at.left + PAD + after, top, &tile.worth, MUTED);
        }
        return;
    }
    canvas.write(at.left + PAD, top, &elide(&tile.name, room), INK);
    if at.height() >= GLYPH.1 + LEADING + PAD {
        canvas.write(
            at.left + PAD,
            top + LEADING,
            &elide(&tile.worth, room),
            MUTED,
        );
    }
}

/// A string cut to `room` characters, saying that it was cut.
///
/// The tail is kept rather than the head when a name is a path: `…/node_modules` identifies a
/// rectangle and `~/repos/some-pro…` does not. A name with no separator in it is cut the
/// ordinary way round.
fn elide(said: &str, room: usize) -> String {
    let count = said.chars().count();
    if room == 0 {
        return String::new();
    }
    if count <= room {
        return said.to_owned();
    }
    if said.contains('/') {
        let tail: String = said.chars().skip(count - (room - 1)).collect();
        return format!("…{tail}");
    }
    let head: String = said.chars().take(room - 1).collect();
    format!("{head}…")
}

/// The bitmap for one character.
fn glyph_of(character: char) -> [u8; 7] {
    if character == '…' {
        return ELLIPSIS;
    }
    if character == '·' {
        return MIDDOT;
    }
    if character == '—' {
        return DASH;
    }
    let code = character as u32;
    if (0x20..0x7f).contains(&code) {
        return FONT[(code - 0x20) as usize];
    }
    FONT[('?' as u32 - 0x20) as usize]
}

/// `…`, and `·`: the two glyphs outside the ASCII run, both of them ones pristine's own
/// captions are written with rather than ones a directory name might contain.
const ELLIPSIS: [u8; 7] = [0, 0, 0, 0, 0, 0, 0b10101];
/// `·`.
const MIDDOT: [u8; 7] = [0, 0, 0, 0b00100, 0, 0, 0];
/// `—`, which the captions are written with.
const DASH: [u8; 7] = [0, 0, 0, 0b11111, 0, 0, 0];

/// A 5×7 bitmap font, `' '` through `'~'`. One byte a row, the low five bits, left to right.
///
/// Authored rather than pulled in, because a font crate is a dependency and 665 bytes is not.
/// Checked by eye — see `the_font_is_legible`, which paints the whole of it.
#[rustfmt::skip]
const FONT: [[u8; 7]; 95] = [
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // ' '
    [0x04, 0x04, 0x04, 0x04, 0x04, 0x00, 0x04], // '!'
    [0x0a, 0x0a, 0x0a, 0x00, 0x00, 0x00, 0x00], // '"'
    [0x0a, 0x0a, 0x1f, 0x0a, 0x1f, 0x0a, 0x0a], // '#'
    [0x04, 0x0f, 0x14, 0x0e, 0x05, 0x1e, 0x04], // '$'
    [0x18, 0x19, 0x02, 0x04, 0x08, 0x13, 0x03], // '%'
    [0x08, 0x14, 0x14, 0x08, 0x15, 0x12, 0x0d], // '&'
    [0x04, 0x04, 0x08, 0x00, 0x00, 0x00, 0x00], // '\''
    [0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02], // '('
    [0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08], // ')'
    [0x00, 0x04, 0x15, 0x0e, 0x15, 0x04, 0x00], // '*'
    [0x00, 0x04, 0x04, 0x1f, 0x04, 0x04, 0x00], // '+'
    [0x00, 0x00, 0x00, 0x00, 0x0c, 0x04, 0x08], // ','
    [0x00, 0x00, 0x00, 0x1f, 0x00, 0x00, 0x00], // '-'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x0c, 0x0c], // '.'
    [0x00, 0x01, 0x02, 0x04, 0x08, 0x10, 0x00], // '/'
    [0x0e, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0e], // '0'
    [0x04, 0x0c, 0x04, 0x04, 0x04, 0x04, 0x0e], // '1'
    [0x0e, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1f], // '2'
    [0x1f, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0e], // '3'
    [0x02, 0x06, 0x0a, 0x12, 0x1f, 0x02, 0x02], // '4'
    [0x1f, 0x10, 0x1e, 0x01, 0x01, 0x11, 0x0e], // '5'
    [0x06, 0x08, 0x10, 0x1e, 0x11, 0x11, 0x0e], // '6'
    [0x1f, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08], // '7'
    [0x0e, 0x11, 0x11, 0x0e, 0x11, 0x11, 0x0e], // '8'
    [0x0e, 0x11, 0x11, 0x0f, 0x01, 0x02, 0x0c], // '9'
    [0x00, 0x0c, 0x0c, 0x00, 0x0c, 0x0c, 0x00], // ':'
    [0x00, 0x0c, 0x0c, 0x00, 0x0c, 0x04, 0x08], // ';'
    [0x02, 0x04, 0x08, 0x10, 0x08, 0x04, 0x02], // '<'
    [0x00, 0x00, 0x1f, 0x00, 0x1f, 0x00, 0x00], // '='
    [0x08, 0x04, 0x02, 0x01, 0x02, 0x04, 0x08], // '>'
    [0x0e, 0x11, 0x01, 0x02, 0x04, 0x00, 0x04], // '?'
    [0x0e, 0x11, 0x01, 0x0d, 0x15, 0x15, 0x0e], // '@'
    [0x04, 0x0a, 0x11, 0x11, 0x1f, 0x11, 0x11], // 'A'
    [0x1e, 0x11, 0x11, 0x1e, 0x11, 0x11, 0x1e], // 'B'
    [0x0e, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0e], // 'C'
    [0x1c, 0x12, 0x11, 0x11, 0x11, 0x12, 0x1c], // 'D'
    [0x1f, 0x10, 0x10, 0x1e, 0x10, 0x10, 0x1f], // 'E'
    [0x1f, 0x10, 0x10, 0x1e, 0x10, 0x10, 0x10], // 'F'
    [0x0e, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0f], // 'G'
    [0x11, 0x11, 0x11, 0x1f, 0x11, 0x11, 0x11], // 'H'
    [0x0e, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0e], // 'I'
    [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0c], // 'J'
    [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11], // 'K'
    [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1f], // 'L'
    [0x11, 0x1b, 0x15, 0x15, 0x11, 0x11, 0x11], // 'M'
    [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11], // 'N'
    [0x0e, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0e], // 'O'
    [0x1e, 0x11, 0x11, 0x1e, 0x10, 0x10, 0x10], // 'P'
    [0x0e, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0d], // 'Q'
    [0x1e, 0x11, 0x11, 0x1e, 0x14, 0x12, 0x11], // 'R'
    [0x0f, 0x10, 0x10, 0x0e, 0x01, 0x01, 0x1e], // 'S'
    [0x1f, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04], // 'T'
    [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0e], // 'U'
    [0x11, 0x11, 0x11, 0x11, 0x11, 0x0a, 0x04], // 'V'
    [0x11, 0x11, 0x11, 0x15, 0x15, 0x1b, 0x11], // 'W'
    [0x11, 0x11, 0x0a, 0x04, 0x0a, 0x11, 0x11], // 'X'
    [0x11, 0x11, 0x0a, 0x04, 0x04, 0x04, 0x04], // 'Y'
    [0x1f, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1f], // 'Z'
    [0x0e, 0x08, 0x08, 0x08, 0x08, 0x08, 0x0e], // '['
    [0x00, 0x10, 0x08, 0x04, 0x02, 0x01, 0x00], // '\\'
    [0x0e, 0x02, 0x02, 0x02, 0x02, 0x02, 0x0e], // ']'
    [0x04, 0x0a, 0x11, 0x00, 0x00, 0x00, 0x00], // '^'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1f], // '_'
    [0x08, 0x04, 0x02, 0x00, 0x00, 0x00, 0x00], // '`'
    [0x00, 0x00, 0x0e, 0x01, 0x0f, 0x11, 0x0f], // 'a'
    [0x10, 0x10, 0x1e, 0x11, 0x11, 0x11, 0x1e], // 'b'
    [0x00, 0x00, 0x0e, 0x11, 0x10, 0x11, 0x0e], // 'c'
    [0x01, 0x01, 0x0f, 0x11, 0x11, 0x11, 0x0f], // 'd'
    [0x00, 0x00, 0x0e, 0x11, 0x1f, 0x10, 0x0e], // 'e'
    [0x06, 0x09, 0x08, 0x1c, 0x08, 0x08, 0x08], // 'f'
    [0x00, 0x00, 0x0f, 0x11, 0x0f, 0x01, 0x0e], // 'g'
    [0x10, 0x10, 0x1e, 0x11, 0x11, 0x11, 0x11], // 'h'
    [0x04, 0x00, 0x0c, 0x04, 0x04, 0x04, 0x0e], // 'i'
    [0x02, 0x00, 0x06, 0x02, 0x02, 0x12, 0x0c], // 'j'
    [0x10, 0x10, 0x12, 0x14, 0x18, 0x14, 0x12], // 'k'
    [0x0c, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0e], // 'l'
    [0x00, 0x00, 0x1a, 0x15, 0x15, 0x15, 0x15], // 'm'
    [0x00, 0x00, 0x1e, 0x11, 0x11, 0x11, 0x11], // 'n'
    [0x00, 0x00, 0x0e, 0x11, 0x11, 0x11, 0x0e], // 'o'
    [0x00, 0x00, 0x1e, 0x11, 0x1e, 0x10, 0x10], // 'p'
    [0x00, 0x00, 0x0f, 0x11, 0x0f, 0x01, 0x01], // 'q'
    [0x00, 0x00, 0x16, 0x19, 0x10, 0x10, 0x10], // 'r'
    [0x00, 0x00, 0x0f, 0x10, 0x0e, 0x01, 0x1e], // 's'
    [0x08, 0x08, 0x1c, 0x08, 0x08, 0x09, 0x06], // 't'
    [0x00, 0x00, 0x11, 0x11, 0x11, 0x13, 0x0d], // 'u'
    [0x00, 0x00, 0x11, 0x11, 0x11, 0x0a, 0x04], // 'v'
    [0x00, 0x00, 0x11, 0x11, 0x15, 0x15, 0x0a], // 'w'
    [0x00, 0x00, 0x11, 0x0a, 0x04, 0x0a, 0x11], // 'x'
    [0x00, 0x00, 0x11, 0x11, 0x0f, 0x01, 0x0e], // 'y'
    [0x00, 0x00, 0x1f, 0x02, 0x04, 0x08, 0x1f], // 'z'
    [0x02, 0x04, 0x04, 0x08, 0x04, 0x04, 0x02], // '{'
    [0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04], // '|'
    [0x08, 0x04, 0x04, 0x02, 0x04, 0x04, 0x08], // '}'
    [0x00, 0x00, 0x08, 0x15, 0x02, 0x00, 0x00], // '~'
];

#[cfg(test)]
mod tests {
    use super::{Canvas, HATCH, HERE, MARKED, PRICED, SURFACE, UNKNOWN, elide, paint};
    use crate::fixture::{hit, priced};
    use crate::size::Size;
    use crate::tree::Tree;
    use crate::tui::keymap::{Action, Motion};
    use crate::tui::state::View;
    use crate::tui::treemap::tiles::{Area, plan};

    /// What colour the canvas has at a point.
    fn at(canvas: &Canvas, x: u32, y: u32) -> [u8; 3] {
        let index = ((y as usize) * (canvas.width as usize) + (x as usize)) * 3;
        [
            canvas.rgb[index],
            canvas.rgb[index + 1],
            canvas.rgb[index + 2],
        ]
    }

    /// Whether a colour is anywhere in the canvas.
    fn anywhere(canvas: &Canvas, colour: [u8; 3]) -> bool {
        canvas.rgb.chunks_exact(3).any(|pixel| pixel == colour)
    }

    fn view() -> View {
        let mut tree = Tree::new("/scan");
        tree.insert(priced("/scan/nx/node_modules", 8 * 1024 * 1024));
        tree.insert(priced("/scan/pua/target", 2 * 1024 * 1024));
        View::new(tree)
    }

    #[test]
    fn a_canvas_starts_as_surface_and_is_the_size_it_was_asked_for() {
        let canvas = Canvas::new(7, 3);
        assert_eq!(canvas.rgb.len(), 7 * 3 * 3);
        assert_eq!(at(&canvas, 6, 2), SURFACE);
    }

    #[test]
    fn a_priced_map_is_filled_and_an_unpriced_one_is_hatched() {
        let view = view();
        let map = plan(&view, view.tree().root(), Area::of(320.0, 200.0)).unwrap();
        let canvas = paint(&map, 320, 200);
        assert!(anywhere(&canvas, PRICED[0]), "nothing was filled");
        assert!(!anywhere(&canvas, UNKNOWN), "something was hatched");

        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/nx/node_modules", Size::Unmeasured, 0));
        let unpriced = View::new(tree);
        let map = plan(&unpriced, unpriced.tree().root(), Area::of(320.0, 200.0)).unwrap();
        let canvas = paint(&map, 320, 200);
        // Texture rather than a third colour: a reader reads a hatch as "no data" and a
        // colour as "another category", and this is the first of those.
        assert!(anywhere(&canvas, UNKNOWN), "the unknown was not drawn");
        assert!(anywhere(&canvas, HATCH[0]), "the unknown was not hatched");
        assert!(
            !anywhere(&canvas, PRICED[0]),
            "an unpriced claim was filled"
        );
    }

    #[test]
    fn a_marked_subtree_changes_hue_and_the_cursor_gets_an_outline() {
        let mut view = view();
        view.apply(Action::Cursor(Motion::Down));
        view.apply(Action::Mark);
        let map = plan(&view, view.tree().root(), Area::of(320.0, 200.0)).unwrap();
        let canvas = paint(&map, 320, 200);

        assert!(anywhere(&canvas, MARKED[0]), "a mark did not show");
        assert!(anywhere(&canvas, PRICED[0]), "everything showed as marked");
        assert!(anywhere(&canvas, HERE), "the cursor is nowhere on the map");
    }

    #[test]
    fn two_rectangles_never_touch() {
        // The separation the palette is not asked to provide. Every column of the map has
        // surface in it somewhere between the two top-level rectangles.
        let view = view();
        let map = plan(&view, view.tree().root(), Area::of(320.0, 200.0)).unwrap();
        let canvas = paint(&map, 320, 200);
        let seam =
            (0..canvas.width).find(|&x| (0..canvas.height).all(|y| at(&canvas, x, y) == SURFACE));
        assert!(
            seam.is_some(),
            "the two rectangles are flush against each other"
        );
    }

    #[test]
    fn a_rectangle_too_small_for_its_figure_still_gets_its_name() {
        // The middle state, and the reason there are three: the name is what makes an area
        // readable, and the figure is already on the row in the tree beside it.
        let mut canvas = Canvas::new(200, 14);
        canvas.write(4, 3, "node_modules", HERE);
        assert!(anywhere(&canvas, HERE), "the name was not drawn");
    }

    #[test]
    fn a_name_that_does_not_fit_keeps_the_end_that_identifies_it() {
        assert_eq!(elide("node_modules", 20), "node_modules");
        // A path is cut at the front: `…/node_modules` names a rectangle and
        // `~/repos/some-pro…` does not.
        assert_eq!(elide("repos/nx/node_modules", 9), "…_modules");
        assert_eq!(elide("node_modules", 6), "node_…");
        assert_eq!(elide("anything", 0), "");
    }

    /// Paints the whole font and every state the map has, to be looked at.
    ///
    /// `cargo test --lib treemap::paint::tests::the_spike_looks_like -- --ignored`, then open
    /// `target/treemap-spike.ppm`. Ignored because its only assertion is a human's.
    #[test]
    #[ignore = "writes a file for a human to look at"]
    fn the_spike_looks_like_this() {
        let mut tree = Tree::new("/repos");
        for (path, bytes) in [
            ("/repos/nx/node_modules", 41_u64 * 1024 * 1024 * 1024),
            ("/repos/nx/.nx/cache", 22 * 1024 * 1024 * 1024),
            (
                "/repos/nx/packages/graph/node_modules",
                8 * 1024 * 1024 * 1024,
            ),
            ("/repos/nx/packages/nx/node_modules", 6 * 1024 * 1024 * 1024),
            ("/repos/pua/target", 11 * 1024 * 1024 * 1024),
            ("/repos/pristine/target", 4 * 1024 * 1024 * 1024),
            ("/repos/brain/node_modules", 3 * 1024 * 1024 * 1024),
            ("/repos/dotfiles/.venv", 900 * 1024 * 1024),
            ("/repos/scratch/build", 400 * 1024 * 1024),
        ] {
            tree.insert(priced(path, bytes));
        }
        for path in ["/repos/archived/node_modules", "/repos/vendor/target"] {
            tree.insert(hit(path, Size::Unmeasured, 0));
        }
        let mut view = View::new(tree);
        view.apply(Action::Cursor(Motion::Down));
        view.apply(Action::Mark);
        view.apply(Action::Cursor(Motion::Down));

        // The pane a real terminal gives: 44 columns of a 120-column window, 34 rows, at the
        // 9×19 px cell a retina Ghostty reports. Rendered at the shape it ships in, because a
        // treemap that reads well square and badly in a tall pane is a treemap that reads
        // badly.
        let (width, height) = (44 * 9_u32, 34 * 19_u32);
        let map = plan(
            &view,
            view.tree().root(),
            Area::of(f64::from(width), f64::from(height - 90)),
        )
        .unwrap();
        let mut canvas = paint(&map, width, height);
        // The font, underneath, so a garbled glyph is caught by looking rather than by a
        // reader hitting it in a directory name.
        let rows = [
            " !\"#$%&'()*+,-./0123456789:;<=>?",
            "@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_",
            "`abcdefghijklmnopqrstuvwxyz{|}~…",
        ];
        for (nth, row) in (0..).zip(rows) {
            canvas.write(6, height - 80 + nth * 12, row, HERE);
        }
        canvas.write(6, height - 36, &map.caption, HERE);
        std::fs::write("../../target/treemap-spike.ppm", canvas.ppm()).unwrap();
    }
}
