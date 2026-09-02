//! `date`: prints the wall clock a guest sees.
//!
//! The reading comes from `wasi:clocks/wall-clock.now`, which the
//! kernel answers from the offset its platform real-time clock set at
//! boot. The program exists so that path is reachable from a shell and
//! from the inspector VM smoke, which compares the printed epoch
//! against the host's own clock.

use std::env;
use std::time::{SystemTime, SystemTimeError, UNIX_EPOCH};

use thiserror::Error;

#[derive(Debug, Error)]
enum DateCommandError {
    #[error("usage: date")]
    UnexpectedArgument(String),
    #[error("the guest wall clock reads before the Unix epoch")]
    BeforeEpoch(#[source] SystemTimeError),
}

#[helios_api::main]
async fn main() -> Result<(), DateCommandError> {
    if let Some(argument) = env::args().nth(1) {
        return Err(DateCommandError::UnexpectedArgument(argument));
    }

    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(DateCommandError::BeforeEpoch)?;
    println!("unix_seconds={}", since_epoch.as_secs());

    Ok(())
}
