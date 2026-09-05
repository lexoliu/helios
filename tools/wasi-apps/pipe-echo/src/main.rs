//! `pipe-echo`: writes back every chunk it reads from stdin until EOF.
//!
//! Each read is flushed straight away so a ping-pong parent sees its
//! message come back without waiting for a buffer to fill; the same
//! program serves the streaming workload because a large write arrives
//! as large reads.

use std::io::{self, Read as _, Write as _};

use thiserror::Error;

const CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug, Error)]
enum PipeEchoError {
    #[error("pipe-echo takes no arguments")]
    UnexpectedArgument(String),
    #[error("stdin read failed after {echoed} bytes: {source}")]
    Stdin {
        echoed: u64,
        #[source]
        source: io::Error,
    },
    #[error("stdout write failed after {echoed} bytes: {source}")]
    Stdout {
        echoed: u64,
        #[source]
        source: io::Error,
    },
}

fn main() -> Result<(), PipeEchoError> {
    if let Some(argument) = std::env::args().nth(1) {
        return Err(PipeEchoError::UnexpectedArgument(argument));
    }
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let mut buffer = vec![0u8; CHUNK_BYTES];
    let mut echoed = 0u64;
    loop {
        let read = stdin
            .read(&mut buffer)
            .map_err(|source| PipeEchoError::Stdin { echoed, source })?;
        if read == 0 {
            return Ok(());
        }
        stdout
            .write_all(&buffer[..read])
            .and_then(|()| stdout.flush())
            .map_err(|source| PipeEchoError::Stdout { echoed, source })?;
        echoed += read as u64;
    }
}
