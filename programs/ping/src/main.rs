use std::env;
use std::io::{self, Write};
use std::time::Duration;

use helios_api::net::{self, IpAddress};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

#[helios_api::main]
async fn main() -> io::Result<()> {
    let mut args = env::args();
    let argv0 = args.next().unwrap_or_else(|| "ping".to_owned());
    let host = args.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("usage: {argv0} <host> [timeout-ms]"),
        )
    })?;
    let timeout = match args.next() {
        Some(value) => Duration::from_millis(value.parse().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid timeout {value:?}: {error}"),
            )
        })?),
        None => DEFAULT_TIMEOUT,
    };

    let reply = net::ping(&host, timeout).await?;
    writeln!(
        io::stdout(),
        "reply from {}: time={}ms bytes={}",
        format_ip(reply.address),
        reply.round_trip / 1_000_000,
        reply.payload_bytes
    )?;
    Ok(())
}

fn format_ip(address: IpAddress) -> String {
    match address {
        IpAddress::Ipv4((a, b, c, d)) => format!("{a}.{b}.{c}.{d}"),
    }
}
