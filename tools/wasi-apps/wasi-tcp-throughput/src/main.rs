use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::num::ParseIntError;

use thiserror::Error;

type Result<T> = core::result::Result<T, TcpThroughputError>;

const READ_CHUNK_BYTES: usize = 1024 * 1024;
const UPLOAD_CHUNK_BYTES: usize = 256 * 1024;
const DOWNLOAD_LABEL: &str = "wasi-tcp-throughput";
const UPLOAD_LABEL: &str = "wasi-tcp-upload";

#[derive(Debug, Error)]
enum TcpThroughputError {
    #[error("usage: wasi-tcp-throughput [--label <name>] <ip-host> <port> <expected-bytes> [up]")]
    Usage,
    #[error("--label requires a value")]
    MissingLabelValue,
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
    #[error("tcp write failed after {bytes_written} bytes: {source}")]
    TcpWrite {
        bytes_written: u64,
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
    upload: bool,
    label: String,
}

fn parse_args() -> Result<TcpThroughputArgs> {
    let mut positionals = Vec::new();
    let mut label = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--label" {
            label = Some(args.next().ok_or(TcpThroughputError::MissingLabelValue)?);
        } else {
            positionals.push(arg);
        }
    }
    let mut positionals = positionals.into_iter();
    let host_raw = positionals.next().ok_or(TcpThroughputError::Usage)?;
    let port_raw = positionals.next().ok_or(TcpThroughputError::Usage)?;
    let expected_raw = positionals.next().ok_or(TcpThroughputError::Usage)?;
    let upload = match positionals.next() {
        None => false,
        Some(mode) if mode == "up" => true,
        Some(mode) => return Err(TcpThroughputError::UnexpectedArgument(mode)),
    };
    if let Some(extra) = positionals.next() {
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
    let label = label.unwrap_or_else(|| {
        if upload {
            UPLOAD_LABEL.to_owned()
        } else {
            DOWNLOAD_LABEL.to_owned()
        }
    });
    Ok(TcpThroughputArgs {
        address: SocketAddr::new(host, port),
        expected_bytes,
        upload,
        label,
    })
}

fn receive(stream: &mut TcpStream, expected_bytes: u64) -> Result<u64> {
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
    if total != expected_bytes {
        return Err(TcpThroughputError::UnexpectedByteCount {
            actual: total,
            expected: expected_bytes,
        });
    }
    Ok(total)
}

fn send(stream: &mut TcpStream, total_bytes: u64) -> Result<u64> {
    let chunk: Vec<u8> = (0..UPLOAD_CHUNK_BYTES as u32)
        .map(|index| (index & 0xFF) as u8)
        .collect();
    let mut written = 0u64;
    while written < total_bytes {
        let length = usize::try_from((total_bytes - written).min(chunk.len() as u64))
            .map_err(|_| TcpThroughputError::ByteCounterOverflow)?;
        stream
            .write_all(&chunk[..length])
            .map_err(|source| TcpThroughputError::TcpWrite {
                bytes_written: written,
                source,
            })?;
        written += length as u64;
    }
    Ok(written)
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let mut stream =
        TcpStream::connect(args.address).map_err(|source| TcpThroughputError::TcpConnect {
            address: args.address,
            source,
        })?;
    let total = if args.upload {
        send(&mut stream, args.expected_bytes)?
    } else {
        receive(&mut stream, args.expected_bytes)?
    };
    println!("{}:{total}", args.label);
    Ok(())
}
