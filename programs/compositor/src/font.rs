//! The glyphs the terminal draws with.
//!
//! Noto Sans Mono, pre-rasterised, from the `noto-sans-mono-bitmap`
//! crate: one byte of coverage per pixel, no allocation and no floating
//! point, which is what makes the same glyph come out of a wasm program
//! and out of a host tool checking a capture of it. Nothing here draws a
//! character table by hand, so there is no second definition of what an
//! `h` looks like for the two to disagree about.
//!
//! The cell is the font's own: every glyph in a monospace face advances
//! the same width, and the raster's height is the line height. A
//! terminal grid is that cell repeated, so the pixel a character lands
//! on is arithmetic rather than layout.

use noto_sans_mono_bitmap::{
    FontWeight, RasterHeight, RasterizedChar, get_raster, get_raster_width,
};

/// The weight the desktop draws text at.
pub const WEIGHT: FontWeight = FontWeight::Regular;

/// The raster the desktop draws text at.
pub const HEIGHT: RasterHeight = RasterHeight::Size16;

/// How many pixels one character cell advances.
pub const CELL_WIDTH: usize = get_raster_width(WEIGHT, HEIGHT);

/// How many pixels tall one character cell is.
pub const CELL_HEIGHT: usize = HEIGHT.val();

/// What a character with no raster in this face is drawn as.
///
/// A terminal shows the bytes it was given; one outside the range this
/// build of the face covers is still a character the user typed or the
/// shell printed, and drawing nothing would lose it silently. This is
/// the same substitution every terminal makes, not a fallback around a
/// defect.
const MISSING: char = '?';

/// The raster of `character`, or of the substitute when the face has
/// none.
///
/// # Panics
///
/// Panics when the face has no raster for the substitute either, which
/// would mean this build of the font covers no basic Latin at all and
/// the terminal could draw nothing whatever it was given.
pub fn glyph(character: char) -> RasterizedChar {
    get_raster(character, WEIGHT, HEIGHT).unwrap_or_else(|| {
        get_raster(MISSING, WEIGHT, HEIGHT)
            .expect("the bundled face covers basic Latin, which is what a terminal draws")
    })
}

/// Whether a character occupies a cell of its own.
///
/// Control bytes are handled by the terminal rather than drawn, so the
/// grid never holds one.
pub const fn is_printable(character: char) -> bool {
    !character.is_control()
}
