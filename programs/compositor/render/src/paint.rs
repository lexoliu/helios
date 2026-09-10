//! Colours, and the two things the compositor does with them.
//!
//! Every surface on this desktop is `bgrx8888`: four bytes a pixel,
//! blue first, and a fourth byte the display engine ignores. A colour is
//! therefore three channels and the arithmetic to mix them, and nothing
//! here knows about a format the display engine does not latch
//! directly.

/// One opaque colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Colour {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl Colour {
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    /// Write this colour into one four-byte pixel.
    ///
    /// # Panics
    ///
    /// Panics on a slice shorter than a pixel, which would mean the
    /// caller computed a strip that does not hold whole pixels.
    pub const fn write(self, pixel: &mut [u8]) {
        pixel[0] = self.blue;
        pixel[1] = self.green;
        pixel[2] = self.red;
        // The byte the display engine ignores. Written all-ones rather
        // than left alone, so a format that reads it as alpha shows an
        // opaque desktop instead of an invisible one.
        pixel[3] = 0xff;
    }
}

/// Mix `over` into `under` at `alpha` parts in 255.
///
/// Integer arithmetic on purpose: the same inputs produce the same byte
/// on every target, so a host reading a capture back can compute the
/// expected pixel itself.
pub const fn blend(under: Colour, over: Colour, alpha: u8) -> Colour {
    const fn channel(under: u8, over: u8, alpha: u8) -> u8 {
        let under = under as u16;
        let over = over as u16;
        let alpha = alpha as u16;
        // Rounded rather than truncated: the halfway mix of two
        // channels is the value between them, not the one below it.
        (((over * alpha) + (under * (255 - alpha)) + 127) / 255) as u8
    }
    Colour::new(
        channel(under.red, over.red, alpha),
        channel(under.green, over.green, alpha),
        channel(under.blue, over.blue, alpha),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pixel_is_written_blue_first_with_the_ignored_byte_set() {
        let mut pixel = [0_u8; 4];
        Colour::new(0x10, 0x20, 0x30).write(&mut pixel);
        assert_eq!(pixel, [0x30, 0x20, 0x10, 0xff]);
    }

    #[test]
    fn the_ends_of_a_blend_are_the_two_colours_themselves() {
        let under = Colour::new(0, 0, 0);
        let over = Colour::new(255, 255, 255);
        assert_eq!(blend(under, over, 0), under);
        assert_eq!(blend(under, over, 255), over);
    }

    #[test]
    fn a_half_blend_is_halfway_between_and_the_same_on_every_target() {
        let mixed = blend(Colour::new(0, 100, 200), Colour::new(200, 100, 0), 128);
        assert_eq!(mixed, Colour::new(100, 100, 100));
    }
}
