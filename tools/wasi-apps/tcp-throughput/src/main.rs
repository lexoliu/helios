use std::io;
use std::num::{ParseIntError, TryFromIntError};

use helios_api::net::{DEFAULT_READ_CHUNK_BYTES, TcpStream};
use thiserror::Error;

type Result<T> = core::result::Result<T, TcpThroughputError>;

#[derive(Debug, Error)]
enum TcpThroughputError {
    #[error("usage: tcp-throughput <host> <port> <expected-bytes>")]
    Usage,
    #[error("unexpected argument `{0}`")]
    UnexpectedArgument(String),
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
    #[error("tcp connect failed for {host}:{port}: {source}")]
    TcpConnect {
        host: String,
        port: u16,
        #[source]
        source: io::Error,
    },
    #[error("tcp read failed after {bytes_read} bytes: {source}")]
    TcpRead {
        bytes_read: u64,
        #[source]
        source: io::Error,
    },
    #[error("tcp read chunk length does not fit into u64")]
    ChunkLength(#[source] TryFromIntError),
    #[error("tcp byte counter overflowed")]
    ByteCounterOverflow,
    #[error("tcp stream delivered {actual} bytes, expected {expected}")]
    UnexpectedByteCount { actual: u64, expected: u64 },
}

struct TcpThroughputArgs {
    host: String,
    port: u16,
    expected_bytes: u64,
}

fn parse_args() -> Result<TcpThroughputArgs> {
    let mut args = std::env::args().skip(1);
    let host = args.next().ok_or(TcpThroughputError::Usage)?;
    let port_raw = args.next().ok_or(TcpThroughputError::Usage)?;
    let expected_raw = args.next().ok_or(TcpThroughputError::Usage)?;
    if let Some(extra) = args.next() {
        return Err(TcpThroughputError::UnexpectedArgument(extra));
    }
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
        host,
        port,
        expected_bytes,
    })
}

async fn run() -> Result<()> {
    let args = parse_args()?;
    let stream = TcpStream::connect(&args.host, args.port)
        .await
        .map_err(|source| TcpThroughputError::TcpConnect {
            host: args.host.clone(),
            port: args.port,
            source,
        })?;
    let mut total = 0u64;
    while let Some(chunk) =
        stream
            .read(DEFAULT_READ_CHUNK_BYTES)
            .await
            .map_err(|source| TcpThroughputError::TcpRead {
                bytes_read: total,
                source,
            })?
    {
        let chunk_len = u64::try_from(chunk.len()).map_err(TcpThroughputError::ChunkLength)?;
        total = total
            .checked_add(chunk_len)
            .ok_or(TcpThroughputError::ByteCounterOverflow)?;
    }
    if total != args.expected_bytes {
        return Err(TcpThroughputError::UnexpectedByteCount {
            actual: total,
            expected: args.expected_bytes,
        });
    }
    println!("tcp-throughput:{total}");
    Ok(())
}

#[helios_api::main]
async fn main() -> Result<()> {
    run().await
}
