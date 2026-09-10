//! Check that the text a desktop capture shows is the text the guest was
//! typed.
//!
//! A scanout capture proves a frame reached the display engine; it says
//! nothing about what the frame said. What says that is the pixels of a
//! character cell, and the only way to know what those should be is to
//! rasterise the same glyph with the same face at the same size and
//! blend it over the same background — which is why this links the
//! compositor's own modules rather than describing them again. A check
//! that carried its own idea of what an `h` looks like would pass a
//! desktop that drew a `b`.
//!
//! ```text
//! check-desktop-text --capture desktop.png --origin 34,34 \
//!     --expect 0,0=echo hello --expect 1,0=hello
//! ```
//!
//! The origin is where the terminal's top-left character cell sits, which
//! the compositor prints on the way up as
//! `compositor:terminal … origin=x,y`, so nothing here has to know how
//! the desktop lays itself out.

use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;

use clap::Parser;
use helios_compositor::desktop::{TERMINAL_BACKGROUND, TERMINAL_FOREGROUND};
use helios_compositor::font::{CELL_HEIGHT, CELL_WIDTH};
use helios_compositor::term::Terminal;
use thiserror::Error;

/// Bytes one pixel of a rendered strip occupies.
const BYTES_PER_PIXEL: usize = 4;

/// How far a channel may sit from what the compositor wrote.
///
/// Zero would be right for a capture of the guest's own frame buffer, and
/// two is what absorbs the display frontend's conversion out of
/// `bgrx8888` without letting a different glyph through: a wrong
/// character differs by whole channels over whole runs of pixels, not by
/// one step.
const DEFAULT_TOLERANCE: u8 = 2;

#[derive(Debug, Error)]
enum CheckError {
    #[error("failed to open {path}: {source}")]
    Open {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to decode {path} as a PNG: {source}")]
    Decode {
        path: String,
        #[source]
        source: png::DecodingError,
    },
    #[error(
        "{path} is {colour_type:?} at {depth:?}; a QEMU screendump is eight-bit RGB or RGBA and \
         nothing here converts a palette"
    )]
    UnsupportedFormat {
        path: String,
        colour_type: png::ColorType,
        depth: png::BitDepth,
    },
    #[error("expected `{0}`, which is not `<row>,<column>=<text>`")]
    Expectation(String),
    #[error("origin `{0}` is not `<x>,<y>`")]
    Origin(String),
    #[error(
        "the strip for row {row} column {column} runs to {right}x{bottom}, past the {width}x{height} capture"
    )]
    OutOfCapture {
        row: usize,
        column: usize,
        right: usize,
        bottom: usize,
        width: usize,
        height: usize,
    },
    #[error(
        "row {row} column {column} does not read `{text}`: {mismatched} of {total} pixels differ, \
         the worst by {worst} at {x},{y} (capture {captured:02x?}, expected {expected:02x?})"
    )]
    Mismatch {
        row: usize,
        column: usize,
        text: String,
        mismatched: usize,
        total: usize,
        worst: u8,
        x: usize,
        y: usize,
        captured: [u8; 3],
        expected: [u8; 3],
    },
}

/// One row of text the capture has to show, and where it starts.
#[derive(Clone, Debug)]
struct Expectation {
    row: usize,
    column: usize,
    text: String,
}

impl std::str::FromStr for Expectation {
    type Err = CheckError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let malformed = || CheckError::Expectation(value.to_owned());
        let (position, text) = value.split_once('=').ok_or_else(malformed)?;
        let (row, column) = position.split_once(',').ok_or_else(malformed)?;
        Ok(Self {
            row: row.trim().parse().map_err(|_| malformed())?,
            column: column.trim().parse().map_err(|_| malformed())?,
            text: text.to_owned(),
        })
    }
}

/// Where the terminal's top-left character cell sits on the scanout.
#[derive(Clone, Copy, Debug)]
struct Origin {
    x: usize,
    y: usize,
}

impl std::str::FromStr for Origin {
    type Err = CheckError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let malformed = || CheckError::Origin(value.to_owned());
        let (x, y) = value.split_once(',').ok_or_else(malformed)?;
        Ok(Self {
            x: x.trim().parse().map_err(|_| malformed())?,
            y: y.trim().parse().map_err(|_| malformed())?,
        })
    }
}

#[derive(Parser)]
#[command(
    name = "check-desktop-text",
    about = "Check that a desktop capture shows the text the guest was typed"
)]
struct Args {
    /// The scanout capture to read.
    #[arg(long, value_name = "PATH")]
    capture: PathBuf,

    /// Where the terminal's top-left character cell sits, as `x,y`.
    #[arg(long, value_name = "X,Y")]
    origin: Origin,

    /// A row the capture has to show, as `<row>,<column>=<text>`.
    #[arg(long = "expect", value_name = "ROW,COLUMN=TEXT", required = true)]
    expect: Vec<Expectation>,

    /// How far one channel may sit from what the compositor wrote.
    #[arg(long, value_name = "STEPS", default_value_t = DEFAULT_TOLERANCE)]
    tolerance: u8,
}

/// A decoded capture, as eight-bit RGB with its own dimensions.
struct Capture {
    width: usize,
    height: usize,
    /// Three bytes a pixel, row-major.
    pixels: Vec<u8>,
}

impl Capture {
    fn read(path: &PathBuf) -> Result<Self, CheckError> {
        let name = path.display().to_string();
        let file = File::open(path).map_err(|source| CheckError::Open {
            path: name.clone(),
            source,
        })?;
        let decoder = png::Decoder::new(BufReader::new(file));
        let mut reader = decoder.read_info().map_err(|source| CheckError::Decode {
            path: name.clone(),
            source,
        })?;
        let mut buffer = vec![0; reader.output_buffer_size().unwrap_or_default()];
        let info = reader
            .next_frame(&mut buffer)
            .map_err(|source| CheckError::Decode {
                path: name.clone(),
                source,
            })?;
        let channels = match (info.color_type, info.bit_depth) {
            (png::ColorType::Rgb, png::BitDepth::Eight) => 3,
            (png::ColorType::Rgba, png::BitDepth::Eight) => 4,
            (colour_type, depth) => {
                return Err(CheckError::UnsupportedFormat {
                    path: name,
                    colour_type,
                    depth,
                });
            }
        };
        let pixels = buffer[..info.buffer_size()]
            .chunks_exact(channels)
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
            .collect();
        Ok(Self {
            width: info.width as usize,
            height: info.height as usize,
            pixels,
        })
    }

    fn pixel(&self, x: usize, y: usize) -> [u8; 3] {
        let start = (y * self.width + x) * 3;
        [
            self.pixels[start],
            self.pixels[start + 1],
            self.pixels[start + 2],
        ]
    }
}

/// The pixels the compositor would have written for `text`.
///
/// Rendered through the compositor's own terminal, so the strip carries
/// whatever the grid does with the text — a tab that advances to the next
/// stop, a character the face has no raster for — rather than what this
/// tool imagines it does.
fn expected_strip(text: &str) -> (Vec<u8>, usize) {
    let columns = text.chars().count().max(1);
    let width = columns * CELL_WIDTH;
    let stride = width * BYTES_PER_PIXEL;
    let mut strip = vec![0; stride * CELL_HEIGHT];
    let mut terminal = Terminal::new(columns, 1);
    terminal.write(text.as_bytes());
    terminal.render_row(
        0,
        &mut strip,
        stride,
        (0, 0),
        TERMINAL_FOREGROUND,
        TERMINAL_BACKGROUND,
    );
    (strip, width)
}

fn check(
    capture: &Capture,
    origin: Origin,
    expectation: &Expectation,
    tolerance: u8,
) -> Result<usize, CheckError> {
    let (strip, width) = expected_strip(&expectation.text);
    let left = origin.x + expectation.column * CELL_WIDTH;
    let top = origin.y + expectation.row * CELL_HEIGHT;
    if left + width > capture.width || top + CELL_HEIGHT > capture.height {
        return Err(CheckError::OutOfCapture {
            row: expectation.row,
            column: expectation.column,
            right: left + width,
            bottom: top + CELL_HEIGHT,
            width: capture.width,
            height: capture.height,
        });
    }
    let stride = width * BYTES_PER_PIXEL;
    let mut mismatched = 0;
    let mut worst = 0;
    let mut worst_at = (0, 0);
    let mut worst_pair = ([0; 3], [0; 3]);
    for y in 0..CELL_HEIGHT {
        for x in 0..width {
            let start = y * stride + x * BYTES_PER_PIXEL;
            // The compositor writes `bgrx8888`; a PNG is red first.
            let expected = [strip[start + 2], strip[start + 1], strip[start]];
            let captured = capture.pixel(left + x, top + y);
            let distance = expected
                .iter()
                .zip(captured.iter())
                .map(|(expected, captured)| expected.abs_diff(*captured))
                .max()
                .unwrap_or(0);
            if distance > tolerance {
                mismatched += 1;
                if distance > worst {
                    worst = distance;
                    worst_at = (left + x, top + y);
                    worst_pair = (captured, expected);
                }
            }
        }
    }
    let total = width * CELL_HEIGHT;
    if mismatched > 0 {
        return Err(CheckError::Mismatch {
            row: expectation.row,
            column: expectation.column,
            text: expectation.text.clone(),
            mismatched,
            total,
            worst,
            x: worst_at.0,
            y: worst_at.1,
            captured: worst_pair.0,
            expected: worst_pair.1,
        });
    }
    Ok(total)
}

fn main() -> Result<(), CheckError> {
    let args = Args::parse();
    let capture = Capture::read(&args.capture)?;
    println!(
        "{}: {}x{}, cell {CELL_WIDTH}x{CELL_HEIGHT}, terminal origin {},{}",
        args.capture.display(),
        capture.width,
        capture.height,
        args.origin.x,
        args.origin.y
    );
    for expectation in &args.expect {
        let checked = check(&capture, args.origin, expectation, args.tolerance)?;
        println!(
            "row {} column {} reads `{}` ({checked} pixels within {})",
            expectation.row, expectation.column, expectation.text, args.tolerance
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A capture that shows `text` at the origin, and nothing else.
    fn capture_of(text: &str) -> Capture {
        let (strip, width) = expected_strip(text);
        let stride = width * BYTES_PER_PIXEL;
        let mut pixels = Vec::with_capacity(width * CELL_HEIGHT * 3);
        for y in 0..CELL_HEIGHT {
            for x in 0..width {
                let start = y * stride + x * BYTES_PER_PIXEL;
                pixels.extend_from_slice(&[strip[start + 2], strip[start + 1], strip[start]]);
            }
        }
        Capture {
            width,
            height: CELL_HEIGHT,
            pixels,
        }
    }

    fn expect(text: &str) -> Expectation {
        Expectation {
            row: 0,
            column: 0,
            text: text.to_owned(),
        }
    }

    #[test]
    fn a_capture_of_the_text_passes_with_no_tolerance_at_all() {
        let capture = capture_of("echo hello");
        let checked = check(&capture, Origin { x: 0, y: 0 }, &expect("echo hello"), 0)
            .expect("a capture rendered from the same glyphs matches them exactly");
        assert_eq!(checked, capture.width * CELL_HEIGHT);
    }

    #[test]
    fn one_wrong_character_is_caught() {
        let capture = capture_of("echo hello");
        let error = check(
            &capture,
            Origin { x: 0, y: 0 },
            &expect("echo hallo"),
            DEFAULT_TOLERANCE,
        )
        .expect_err("a different character is different pixels");
        assert!(matches!(error, CheckError::Mismatch { .. }), "{error}");
    }

    #[test]
    fn a_blank_screen_is_caught_rather_than_read_as_text() {
        let capture = Capture {
            width: 200,
            height: CELL_HEIGHT,
            pixels: vec![0; 200 * CELL_HEIGHT * 3],
        };
        let error = check(
            &capture,
            Origin { x: 0, y: 0 },
            &expect("hello"),
            DEFAULT_TOLERANCE,
        )
        .expect_err("black is not the terminal's background, let alone its text");
        assert!(matches!(error, CheckError::Mismatch { .. }), "{error}");
    }

    #[test]
    fn a_strip_that_runs_off_the_capture_is_refused_rather_than_wrapped() {
        let capture = capture_of("hi");
        let error = check(
            &capture,
            Origin { x: 0, y: 0 },
            &expect("hello there"),
            DEFAULT_TOLERANCE,
        )
        .expect_err("a row wider than the capture cannot be read from it");
        assert!(matches!(error, CheckError::OutOfCapture { .. }), "{error}");
    }

    #[test]
    fn an_expectation_is_parsed_as_row_column_and_text() {
        let expectation: Expectation = "3,7=echo hello".parse().expect("well formed");
        assert_eq!(expectation.row, 3);
        assert_eq!(expectation.column, 7);
        assert_eq!(expectation.text, "echo hello");
        assert!("3=hello".parse::<Expectation>().is_err());
        assert!("3,7".parse::<Expectation>().is_err());
    }
}
