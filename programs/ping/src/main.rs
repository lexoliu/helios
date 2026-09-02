//! `ping`: one ICMP echo exchange through `helios:system/net`.
//!
//! The kernel owns resolution, transmission and reply matching; this
//! program exists so the echo path is reachable from a shell and from
//! the inspector VM smoke.

use std::env;
use std::fmt;
use std::io;
use std::num::ParseIntError;
use std::time::Duration;

use helios_api::net::{self, IpAddress};
use thiserror::Error;

/// Matches the `-W` default a conventional `ping` waits for one reply.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

const NANOS_PER_MILLI: f64 = 1_000_000.0;

#[derive(Debug, Error)]
enum PingCommandError {
    #[error("usage: ping <host> [timeout-seconds]")]
    Usage,
    #[error("invalid timeout `{raw}`; expected whole seconds")]
    InvalidTimeout {
        raw: String,
        #[source]
        source: ParseIntError,
    },
    #[error("unexpected argument `{0}`")]
    UnexpectedArgument(String),
    #[error("ping {host} failed: {source}")]
    Unreachable {
        host: String,
        #[source]
        source: io::Error,
    },
}

#[helios_api::main]
async fn main() -> Result<(), PingCommandError> {
    let mut args = env::args().skip(1);
    let host = args.next().ok_or(PingCommandError::Usage)?;
    let timeout = match args.next() {
        Some(raw) => Duration::from_secs(
            raw.parse()
                .map_err(|source| PingCommandError::InvalidTimeout { raw, source })?,
        ),
        None => DEFAULT_TIMEOUT,
    };
    if let Some(extra) = args.next() {
        return Err(PingCommandError::UnexpectedArgument(extra));
    }

    let reply =
        net::ping(&host, timeout)
            .await
            .map_err(|source| PingCommandError::Unreachable {
                host: host.clone(),
                source,
            })?;
    println!(
        "{} bytes from {}: time={:.3} ms",
        reply.payload_bytes,
        Address(reply.address),
        reply.round_trip as f64 / NANOS_PER_MILLI
    );
    Ok(())
}

/// Renders the address that answered.
struct Address(IpAddress);

impl fmt::Display for Address {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            IpAddress::Ipv4((a, b, c, d)) => write!(formatter, "{a}.{b}.{c}.{d}"),
            // Uncompressed colon-hex: one line names one address, so
            // being unambiguous matters more than being short.
            IpAddress::Ipv6((a, b, c, d, e, f, g, h)) => {
                write!(formatter, "{a:x}:{b:x}:{c:x}:{d:x}:{e:x}:{f:x}:{g:x}:{h:x}")
            }
        }
    }
}
