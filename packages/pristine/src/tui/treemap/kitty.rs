//! The kitty graphics protocol, in the two sequences this needs.
//!
//! # What is sent
//!
//! An `APC` — `ESC _ G <keys> ; <payload> ESC \` — carrying the canvas as base64 RGB, split
//! into chunks of at most [`CHUNK`] payload bytes with `m=1` on every one but the last. The
//! keys say what the bytes are (`f=24`, `s`, `v`), where they go (`a=T`, `c`, `r`) and how to
//! behave (`i`, `q=2`, `C=1`).
//!
//! # The three keys that are not about the picture
//!
//! - **`q=2` suppresses the terminal's reply**, and it is not an optimisation. Without it the
//!   terminal answers every transmission with `ESC _ G i=… ; OK ESC \` *on stdin*, which
//!   arrives in the event loop as a burst of keystrokes — an `i`, an `=`, a `;`, an `O`, a
//!   `K`. Half of those are bound. A picture that types into the tree it is a picture of is
//!   not a degradation, it is a hazard.
//! - **`C=1` stops the cursor moving.** The default is for the cursor to end up after the
//!   image, which ratatui does not know about and would then draw from.
//! - **`i=`** names the image so it can be taken back. See [`Image::gone`]: a graphics image
//!   is *stored by the terminal*, not by this process, so leaving one behind is leaving a
//!   megabyte in somebody's terminal after pristine has exited — #619's "a state that cannot
//!   be given back is a state you do not take", in the one place here where the state lives
//!   in another program's memory.
//!
//! # Nothing here asks the terminal a question
//!
//! There is a documented query for "do you speak this protocol" and it is a round trip: write
//! a probe, then read the answer, with no bound on how long a terminal that does not speak it
//! takes to not answer. That is exactly the blocking probe [`super::super::chrome`] refuses to
//! make, so this is allowlisted from the environment on the same terms as everything else —
//! see [`super::Graphics`].

use super::paint::Canvas;

/// The most base64 one chunk may carry, from the protocol's own limit.
const CHUNK: usize = 4096;

/// An image this process has given the terminal, and the id it can take it back by.
///
/// The number is arbitrary but must not be one another program is using; the protocol's own
/// advice is to pick a random one, and a constant is fine here because two pristines sharing
/// a terminal would each be drawing over the other's screen anyway.
pub const ID: u32 = 1_976_622;

/// Everything this sends the terminal, as bytes ready to be written.
pub struct Image;

impl Image {
    /// Puts `canvas` on the screen at `at`, a one-based (row, column) cell, filling
    /// `cells` (columns, rows) of the grid.
    ///
    /// The cursor is saved and put back around the placement, because the terminal's cursor
    /// belongs to ratatui: a frame drawn from wherever an image happened to leave it is a
    /// frame drawn in the wrong place.
    #[must_use]
    pub fn shown(canvas: &Canvas, at: (u16, u16), cells: (u16, u16)) -> Vec<u8> {
        let payload = base64(&canvas.rgb);
        let mut out = Vec::with_capacity(payload.len() + payload.len() / CHUNK * 32 + 64);
        out.extend_from_slice(b"\x1b7");
        out.extend_from_slice(format!("\x1b[{};{}H", at.0, at.1).as_bytes());
        // Replaced rather than overwritten: the same id twice is defined to replace, and
        // saying so costs twenty bytes against a payload of a megabyte.
        out.extend_from_slice(&Self::gone());
        let chunks = payload.as_bytes().chunks(CHUNK);
        let last = chunks.len().saturating_sub(1);
        for (nth, chunk) in chunks.enumerate() {
            out.extend_from_slice(b"\x1b_G");
            if nth == 0 {
                out.extend_from_slice(
                    format!(
                        "a=T,q=2,C=1,i={ID},f=24,s={},v={},c={},r={},",
                        canvas.width, canvas.height, cells.0, cells.1
                    )
                    .as_bytes(),
                );
            }
            out.extend_from_slice(if nth == last { b"m=0;" } else { b"m=1;" });
            out.extend_from_slice(chunk);
            out.extend_from_slice(b"\x1b\\");
        }
        out.extend_from_slice(b"\x1b8");
        out
    }

    /// Takes the image back, data and all.
    ///
    /// `d=I` rather than `d=i`: the lower case one removes the *placement* and leaves the
    /// pixels in the terminal's memory, which is a leak that outlives the process.
    #[must_use]
    pub fn gone() -> Vec<u8> {
        format!("\x1b_Ga=d,d=I,i={ID},q=2\x1b\\").into_bytes()
    }
}

/// Standard base64, padded.
///
/// Hand-rolled rather than a dependency, because it is twenty lines and the alternative is a
/// crate in the tree of a tool whose whole pitch is that it is one binary.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let packed = u32::from(group[0]) << 16
            | u32::from(group.get(1).copied().unwrap_or(0)) << 8
            | u32::from(group.get(2).copied().unwrap_or(0));
        for shift in [18, 12, 6, 0] {
            out.push(char::from(ALPHABET[((packed >> shift) & 0x3f) as usize]));
        }
        // The padding is over the *characters* that stood for bytes nobody sent.
        let missing = 3 - group.len();
        out.truncate(out.len() - missing);
        for _ in 0..missing {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{CHUNK, ID, Image, base64};
    use crate::tui::treemap::paint::Canvas;

    fn said(bytes: &[u8]) -> String {
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[test]
    fn base64_agrees_with_the_standard_on_every_length_of_tail() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // The bytes an image is actually made of, which are not text.
        assert_eq!(base64(&[0x00, 0xff, 0x1a]), "AP8a");
    }

    #[test]
    fn one_placement_says_what_the_bytes_are_where_they_go_and_to_keep_quiet() {
        let canvas = Canvas::new(2, 2);
        let out = said(&Image::shown(&canvas, (3, 41), (20, 10)));

        assert!(out.starts_with("\x1b7\x1b[3;41H"), "{out:?}");
        assert!(
            out.ends_with("\x1b8"),
            "the cursor was not put back: {out:?}"
        );
        assert!(out.contains("f=24,s=2,v=2,c=20,r=10,"), "{out:?}");
        // The one that is a hazard rather than an optimisation: without it the terminal
        // answers on stdin and the answer arrives as keystrokes the tree is bound to.
        assert!(
            out.contains("q=2"),
            "the terminal was not told to keep quiet"
        );
        assert!(
            out.contains("C=1"),
            "the cursor would be left after the image"
        );
        assert!(out.contains(&format!("i={ID}")), "{out:?}");
        assert!(out.contains("m=0;"), "no chunk was marked as the last");
    }

    #[test]
    fn a_payload_too_big_for_one_chunk_is_split_and_only_the_last_says_so() {
        // 64×64 of RGB is 12 KiB, which is four chunks of base64.
        let canvas = Canvas::new(64, 64);
        let out = said(&Image::shown(&canvas, (1, 1), (8, 4)));

        assert!(
            out.matches("m=1;").count() >= 3,
            "not chunked: {}",
            out.len()
        );
        assert_eq!(out.matches("m=0;").count(), 1, "more than one last chunk");
        // Every chunk's payload is inside the protocol's limit, which is what the terminal
        // enforces by dropping the image rather than by complaining.
        // Every chunk that carries one — the delete at the front has no payload and so no
        // `;` either, which is the protocol's own shape rather than an omission.
        let mut counted = 0;
        for chunk in out.split("\x1b_G").skip(1) {
            let Some((_, rest)) = chunk.split_once(';') else {
                continue;
            };
            let payload = rest.split_once('\x1b').unwrap().0;
            assert!(payload.len() <= CHUNK, "a chunk of {}", payload.len());
            counted += 1;
        }
        assert!(counted >= 4, "only {counted} chunks");
        // Only the first carries the keys; repeating them would be a second image.
        assert_eq!(out.matches("a=T").count(), 1);
    }

    #[test]
    fn the_image_can_be_taken_back_with_its_pixels() {
        let gone = said(&Image::gone());
        // Upper case: the lower case one leaves the pixels in the terminal's memory after
        // this process has exited, which is a leak nothing is left alive to notice.
        assert!(gone.contains("d=I"), "{gone}");
        assert!(gone.contains(&format!("i={ID}")), "{gone}");
        assert!(gone.contains("q=2"), "{gone}");
        // …and every placement begins by taking back the one before it.
        let shown = said(&Image::shown(&Canvas::new(2, 2), (1, 1), (1, 1)));
        assert!(
            shown.contains(&gone),
            "a placement left the last one behind"
        );
    }
}
