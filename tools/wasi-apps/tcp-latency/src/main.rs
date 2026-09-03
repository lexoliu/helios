//! `tcp-latency`: round-trip time of a 16-byte message over one TCP stream.
//!
//! The host runs `tools/wasi-apps/tcp_echo_server.py`; every round trip
//! crosses the guest's network stack twice, so the distribution of round
//! trips is the latency of the in-kernel TCP path against Linux's.

use std::io::{self, Read as _, Write as _};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::num::ParseIntError;
use std::time::Instant;

use helios_bench_metrics::LatencySamples;
use thiserror::Error;

const MESSAGE_BYTES: usize = 16;

#[derive(Debug, Error)]
enum TcpLatencyError {
    #[error("usage: tcp-latency <ip-host> <port> <rounds>")]
    Usage,
    #[error("unexpected argument `{0}`")]
    UnexpectedArgument(String),
    #[error("invalid IP host `{raw}`")]
    InvalidHost {
        raw: String,
        #[source]
        source: std::net::AddrParseError,
    },
    #[error("invalid TCP port `{raw}`")]
    InvalidPort {
        raw: String,
        #[source]
        source: ParseIntError,
    },
    #[error("invalid round count `{raw}`")]
    InvalidRounds {
        raw: String,
        #[source]
        source: ParseIntError,
    },
    #[error("tcp connect failed for {address}: {source}")]
    Connect {
        address: SocketAddr,
        #[source]
        source: io::Error,
    },
    #[error("write failed in round {round}: {source}")]
    Write {
        round: u64,
        #[source]
        source: io::Error,
    },
    #[error("read failed in round {round}: {source}")]
    Read {
        round: u64,
        #[source]
        source: io::Error,
    },
    #[error("echo server returned a corrupted message in round {round}")]
    CorruptEcho { round: u64 },
}

struct Args {
    address: SocketAddr,
    rounds: u64,
}

fn parse_args() -> Result<Args, TcpLatencyError> {
    let mut args = std::env::args().skip(1);
    let host_raw = args.next().ok_or(TcpLatencyError::Usage)?;
    let port_raw = args.next().ok_or(TcpLatencyError::Usage)?;
    let rounds_raw = args.next().ok_or(TcpLatencyError::Usage)?;
    if let Some(extra) = args.next() {
        return Err(TcpLatencyError::UnexpectedArgument(extra));
    }
    let host: IpAddr = host_raw
        .parse()
        .map_err(|source| TcpLatencyError::InvalidHost {
            raw: host_raw,
            source,
        })?;
    let port = port_raw
        .parse()
        .map_err(|source| TcpLatencyError::InvalidPort {
            raw: port_raw,
            source,
        })?;
    let rounds = rounds_raw
        .parse()
        .map_err(|source| TcpLatencyError::InvalidRounds {
            raw: rounds_raw,
            source,
        })?;
    if rounds == 0 {
        return Err(TcpLatencyError::Usage);
    }
    Ok(Args {
        address: SocketAddr::new(host, port),
        rounds,
    })
}

fn main() -> Result<(), TcpLatencyError> {
    let args = parse_args()?;
    let mut stream = TcpStream::connect(args.address).map_err(|source| TcpLatencyError::Connect {
        address: args.address,
        source,
    })?;

    let mut samples = LatencySamples::with_capacity(usize::try_from(args.rounds).expect("rounds fit usize"));
    let mut message = [0u8; MESSAGE_BYTES];
    let mut reply = [0u8; MESSAGE_BYTES];
    for round in 0..args.rounds {
        message.copy_from_slice(&round.to_le_bytes().repeat(2));
        let started = Instant::now();
        stream
            .write_all(&message)
            .map_err(|source| TcpLatencyError::Write { round, source })?;
        stream
            .read_exact(&mut reply)
            .map_err(|source| TcpLatencyError::Read { round, source })?;
        samples.record(started.elapsed());
        if reply != message {
            return Err(TcpLatencyError::CorruptEcho { round });
        }
    }

    println!("tcp-latency:{}", args.rounds);
    samples.report("rtt");
    Ok(())
}
