//! `display-test`: draws on the machine's display and keeps it drawn.
//!
//! It is the guest side of the display path's acceptance evidence. It
//! claims the display, creates a surface at the output's preferred mode,
//! fills it with a gradient whose colour at every pixel is a function of
//! that pixel's position, and presents it. Then it stays: it walks the
//! hardware cursor around the surface and re-presents on a fixed
//! cadence, so a host that captures the scanout at any point after the
//! first frame sees the same picture.
//!
//! Every step prints a line, because a capture that comes back blank has
//! to be told apart from a program that never got as far as drawing.
//!
//! The gradient is deterministic on purpose. A host reading the capture
//! back knows what the pixel at any position should be — red rises to
//! the right, green rises downward, blue is constant — so the check is
//! "is this the picture" rather than "is this not black".

use std::env;
use std::time::{Duration, Instant};

use helios_api::display::{
    CURSOR_BYTES, CURSOR_HEIGHT, CURSOR_WIDTH, Display, DisplayError, Mode, PixelFormat, Point,
    Rect, Surface,
};
use helios_api::task::sleep;
use thiserror::Error;

/// How long the pointer takes to walk once around its path.
const CURSOR_PERIOD: Duration = Duration::from_secs(4);
/// How often the pointer moves.
const CURSOR_STEP: Duration = Duration::from_millis(50);
/// How often the surface is presented again after the first frame.
///
/// The gradient does not change, so this is not a repaint anybody needs;
/// it is what keeps the display path exercised for as long as a host is
/// watching, and what makes the frame counter mean something.
const PRESENT_INTERVAL: Duration = Duration::from_secs(1);

/// The blue channel of every pixel of the gradient.
///
/// Constant so a capture can be checked in one dimension that does not
/// depend on where the pixel is.
const GRADIENT_BLUE: u8 = 0x40;

#[derive(Debug, Error)]
enum DisplayTestError {
    #[error("usage: display-test [--seconds <n>] [--scanout <n>]")]
    Usage,
    #[error("--{option} needs a number, not {value:?}")]
    NotANumber { option: &'static str, value: String },
    #[error("the display refused: {0}")]
    Display(#[from] DisplayError),
    #[error("the output reports a {width}x{height} mode, which has no pixels to draw")]
    EmptyMode { width: u32, height: u32 },
}

struct Options {
    /// How long to keep drawing. Absent means until this program is
    /// killed, which is what a host that captures the scanout wants: it
    /// decides when it has seen enough.
    seconds: Option<u64>,
    scanout: u32,
}

fn parse_options() -> Result<Options, DisplayTestError> {
    let mut options = Options {
        seconds: None,
        scanout: 0,
    };
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--seconds" => {
                let value = arguments.next().ok_or(DisplayTestError::Usage)?;
                options.seconds =
                    Some(value.parse().map_err(|_| DisplayTestError::NotANumber {
                        option: "seconds",
                        value,
                    })?);
            }
            "--scanout" => {
                let value = arguments.next().ok_or(DisplayTestError::Usage)?;
                options.scanout = value.parse().map_err(|_| DisplayTestError::NotANumber {
                    option: "scanout",
                    value,
                })?;
            }
            _ => return Err(DisplayTestError::Usage),
        }
    }
    Ok(options)
}

/// Fill `surface` with the gradient.
fn draw_gradient(surface: &mut Surface, mode: Mode) {
    let stride = surface.stride();
    let pixels = surface.pixels();
    for y in 0..mode.height {
        let row = (y as usize) * stride;
        // Integer arithmetic on purpose: the same input has to produce
        // the same byte on every target, so a host checking the capture
        // can compute the expected value itself.
        let green = ((y * 255) / mode.height.max(1)) as u8;
        for x in 0..mode.width {
            let offset = row + (x as usize) * 4;
            let red = ((x * 255) / mode.width.max(1)) as u8;
            // `bgrx8888`: blue, green, red, then the byte the display
            // engine ignores.
            pixels[offset] = GRADIENT_BLUE;
            pixels[offset + 1] = green;
            pixels[offset + 2] = red;
            pixels[offset + 3] = 0xff;
        }
    }
}

/// The pointer image: a filled diamond that is opaque in the middle and
/// transparent at its corners, so it is recognisable whatever it is over.
fn cursor_image() -> Vec<u8> {
    let mut image = vec![0_u8; CURSOR_BYTES];
    let centre = (CURSOR_WIDTH / 2) as i32;
    for y in 0..CURSOR_HEIGHT {
        for x in 0..CURSOR_WIDTH {
            let distance = (x as i32 - centre).abs() + (y as i32 - centre).abs();
            if distance > centre {
                continue;
            }
            let offset = ((y * CURSOR_WIDTH + x) as usize) * 4;
            // Opaque white with a black edge, which reads against both
            // ends of the gradient.
            let edge = distance + 2 > centre;
            let level = if edge { 0x00 } else { 0xff };
            image[offset] = level;
            image[offset + 1] = level;
            image[offset + 2] = level;
            image[offset + 3] = 0xff;
        }
    }
    image
}

/// Where the pointer sits `elapsed` into its walk.
///
/// A rectangular circuit rather than a circle: it needs no floating
/// point, and every position on it is a whole number of pixels a host
/// can predict.
fn cursor_position(mode: Mode, elapsed: Duration) -> Point {
    let period = CURSOR_PERIOD.as_millis().max(1) as u64;
    let phase = (elapsed.as_millis() as u64 % period) * 4 / period;
    let progress = ((elapsed.as_millis() as u64 % period) * 4 % period) * 1000 / period;
    let along = |span: u32| ((span as u64 - 1) * progress / 1000) as u32;
    let (width, height) = (mode.width.max(1), mode.height.max(1));
    match phase {
        0 => Point {
            x: along(width),
            y: 0,
        },
        1 => Point {
            x: width - 1,
            y: along(height),
        },
        2 => Point {
            x: width - 1 - along(width),
            y: height - 1,
        },
        _ => Point {
            x: 0,
            y: height - 1 - along(height),
        },
    }
}

#[helios_api::main]
async fn main() -> Result<(), DisplayTestError> {
    let options = parse_options()?;

    let display = Display::claim()?;
    let scanouts = display.scanouts().await?;
    println!("display-test:claimed scanouts={}", scanouts.len());

    let mode = display.preferred_mode(options.scanout).await?;
    if mode.width == 0 || mode.height == 0 {
        return Err(DisplayTestError::EmptyMode {
            width: mode.width,
            height: mode.height,
        });
    }

    let mut surface = display
        .create(options.scanout, mode, PixelFormat::Bgrx8888)
        .await?;
    let placement = surface.placement();
    println!(
        "display-test:surface scanout={} width={} height={} offset={} length={}",
        options.scanout, mode.width, mode.height, placement.offset, placement.length
    );

    draw_gradient(&mut surface, mode);
    let token = surface.present_all().await?;
    println!(
        "display-test:presented sequence={} blue={GRADIENT_BLUE}",
        token.sequence
    );

    surface
        .set_cursor(cursor_image(), Point { x: 32, y: 32 })
        .await?;
    println!("display-test:cursor-set width={CURSOR_WIDTH} height={CURSOR_HEIGHT} hotspot=32,32");

    run_until_done(&surface, mode, options.seconds).await?;
    Ok(())
}

/// Walk the pointer and keep presenting.
async fn run_until_done(
    surface: &Surface,
    mode: Mode,
    seconds: Option<u64>,
) -> Result<(), DisplayError> {
    let start = Instant::now();
    let deadline = seconds.map(|seconds| start + Duration::from_secs(seconds));
    let mut next_present = start + PRESENT_INTERVAL;
    loop {
        let now = Instant::now();
        if deadline.is_some_and(|deadline| now >= deadline) {
            println!("display-test:done");
            return Ok(());
        }
        let position = cursor_position(mode, now.duration_since(start));
        surface.move_cursor(position).await?;
        if now >= next_present {
            let token = surface
                .present(Rect {
                    x: 0,
                    y: 0,
                    width: mode.width,
                    height: mode.height,
                })
                .await?;
            println!(
                "display-test:frame sequence={} cursor={},{}",
                token.sequence, position.x, position.y
            );
            next_present = now + PRESENT_INTERVAL;
        }
        sleep(CURSOR_STEP).await;
    }
}
