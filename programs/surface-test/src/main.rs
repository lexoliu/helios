//! `surface-test`: takes a window on the desktop, draws into it, and
//! hands it back.
//!
//! It is the guest side of the `helios:system/surface` path's
//! acceptance evidence: a program that is not the compositor asking for
//! a window, publishing one solid frame, and letting the window go.
//! Every step prints a line, because a boot whose compositor died under
//! a call has to be told apart from a program that never asked.
//!
//! The window is short: the desktop puts the first client at the top of
//! the screen's right half, and the input probe leaves the pointer at
//! the screen's centre, so a window that does not reach down to it
//! never takes the keyboard from the shell.

use std::time::Duration;

use helios_api::surface::{BYTES_PER_PIXEL, Surface, SurfaceError};
use helios_api::task::sleep;
use thiserror::Error;

/// The window's size in pixels.
const WIDTH: u32 = 320;
const HEIGHT: u32 = 160;

/// The frame's only colour, in the display's `bgrx8888` order.
const PIXEL: [u8; BYTES_PER_PIXEL] = [0x30, 0xa0, 0xff, 0xff];

/// How long the frame stays on the desktop before the window goes.
///
/// The compositor folds a commit into its own next frame rather than
/// flushing on the spot, so the publish is given a moment to reach the
/// screen before the window underneath it disappears.
const HOLD: Duration = Duration::from_millis(500);

/// How long the program stays up after the window is gone.
///
/// `screendump --run` calls a program that exits before the captures
/// were taken a failure, and delivers what it printed only once it
/// exits. The captures follow the input script by a twenty-second
/// settle and the script starts once the compositor has claimed the
/// input devices — which it does before it serves `create` — so the
/// program outlives the capture pass with room on both sides of the
/// session's run-wait window.
const LINGER: Duration = Duration::from_secs(40);

#[derive(Debug, Error)]
enum SurfaceTestError {
    #[error("the desktop refused: {0}")]
    Surface(#[from] SurfaceError),
}

#[helios_api::main]
async fn main() -> Result<(), SurfaceTestError> {
    match draw_once().await {
        Ok(()) => {
            sleep(LINGER).await;
            Ok(())
        }
        Err(error) => {
            println!("surface-test:error={error:?}");
            Err(error)
        }
    }
}

/// Take one window, publish one frame into it, and give it back.
async fn draw_once() -> Result<(), SurfaceTestError> {
    let mut surface = Surface::create(WIDTH, HEIGHT).await?;
    let size = surface.size();
    println!(
        "surface-test:created width={} height={}",
        size.width, size.height
    );

    for pixel in surface.pixels().chunks_exact_mut(BYTES_PER_PIXEL) {
        pixel.copy_from_slice(&PIXEL);
    }
    surface.commit_all().await?;
    println!("surface-test:committed");

    sleep(HOLD).await;
    drop(surface);
    println!("surface-test:destroyed");
    Ok(())
}
