use std::io::{self, Read};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::num::ParseIntError;

use thiserror::Error;

type Result<T> = core::result::Result<T, TcpThroughputError>;

const READ_CHUNK_BYTES: usize = 1024 * 1024;

#[derive(Debug, Error)]
enum TcpThroughputError {
    #[error("usage: wasi-tcp-throughput <ip-host> <port> <expected-bytes>")]
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
    #[error("invalid expected byte count `{raw}`")]
    InvalidExpectedBytes {
        raw: String,
        #[source]
        source: ParseIntError,
    },
    #[error("tcp connect failed for {address}: {source}")]
    TcpConnect {
        address: SocketAddr,
        #[source]
        source: io::Error,
    },
    #[error("tcp read failed after {bytes_read} bytes: {source}")]
    TcpRead {
        bytes_read: u64,
        #[source]
        source: io::Error,
    },
    #[error("tcp byte counter overflowed")]
    ByteCounterOverflow,
    #[error("tcp stream delivered {actual} bytes, expected {expected}")]
    UnexpectedByteCount { actual: u64, expected: u64 },
}

struct TcpThroughputArgs {
    address: SocketAddr,
    expected_bytes: u64,
}

fn parse_args() -> Result<TcpThroughputArgs> {
    let mut args = std::env::args().skip(1);
    let host_raw = args.next().ok_or(TcpThroughputError::Usage)?;
    let port_raw = args.next().ok_or(TcpThroughputError::Usage)?;
    let expected_raw = args.next().ok_or(TcpThroughputError::Usage)?;
    if let Some(extra) = args.next() {
        return Err(TcpThroughputError::UnexpectedArgument(extra));
    }
    let host: IpAddr = host_raw
        .parse()
        .map_err(|source| TcpThroughputError::InvalidHost {
            raw: host_raw,
            source,
        })?;
    let port = port_raw
        .parse()
        .map_err(|source| TcpThroughputError::InvalidPort {
            raw: port_raw,
            source,
        })?;
    let expected_bytes =
        expected_raw
            .parse()
            .map_err(|source| TcpThroughputError::InvalidExpectedBytes {
                raw: expected_raw,
                source,
            })?;
    Ok(TcpThroughputArgs {
        address: SocketAddr::new(host, port),
        expected_bytes,
    })
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let mut stream =
        TcpStream::connect(args.address).map_err(|source| TcpThroughputError::TcpConnect {
            address: args.address,
            source,
        })?;
    let mut buffer = vec![0u8; READ_CHUNK_BYTES];
    let mut total = 0u64;
    loop {
        let read = stream
            .read(&mut buffer)
            .map_err(|source| TcpThroughputError::TcpRead {
                bytes_read: total,
                source,
            })?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or(TcpThroughputError::ByteCounterOverflow)?;
    }
    if total != args.expected_bytes {
        return Err(TcpThroughputError::UnexpectedByteCount {
            actual: total,
            expected: args.expected_bytes,
        });
    }
    println!("wasi-tcp-throughput:{total}");
    Ok(())
}
