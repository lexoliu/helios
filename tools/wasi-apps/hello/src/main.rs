//! `hello`: the child every start-up workload launches.
//!
//! It prints one line as early as a program can, which is what the parent
//! measures as time to first output. With `hold` it then blocks on stdin
//! until the parent closes it, so a batch of instances stays alive while
//! the parent samples their memory footprint.

use std::io::{self, Read as _, Write as _};

use thiserror::Error;

#[derive(Debug, Error)]
enum HelloError {
    #[error("usage: hello [hold]")]
    Usage(String),
    #[error("stdout write failed: {0}")]
    Stdout(#[source] io::Error),
    #[error("stdin read failed: {0}")]
    Stdin(#[source] io::Error),
}

fn main() -> Result<(), HelloError> {
    let mut args = std::env::args().skip(1);
    let hold = match args.next() {
        None => false,
        Some(argument) if argument == "hold" => true,
        Some(argument) => return Err(HelloError::Usage(argument)),
    };
    if let Some(extra) = args.next() {
        return Err(HelloError::Usage(extra));
    }

    let mut stdout = io::stdout().lock();
    stdout.write_all(b"hello\n").map_err(HelloError::Stdout)?;
    stdout.flush().map_err(HelloError::Stdout)?;

    if hold {
        let mut sink = [0u8; 64];
        let mut stdin = io::stdin().lock();
        loop {
            match stdin.read(&mut sink) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) => return Err(HelloError::Stdin(error)),
            }
        }
    }
    Ok(())
}
