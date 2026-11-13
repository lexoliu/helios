//! `cpu`: the machine's processors from the shell.
//!
//! `cpu list` prints the possible and online sets on one line:
//!
//! ```text
//! possible 4 bootstrap 0 online 0,1,2,3
//! ```
//!
//! `cpu offline <n>` and `cpu online <n>` request the transition and
//! print `offline <n>` or `online <n>` once it has settled. A refusal or
//! an invalid request is the lifecycle's own answer, printed to stderr
//! with a non-zero exit; each case has its own message, so a script can
//! tell a refusal from a request that named the wrong processor.

use std::env;
use std::fmt;
use std::num::ParseIntError;

use helios_api::processors::{self, OfflineError, OfflineRefusal, OnlineError, ProcessorId};
use thiserror::Error;

#[derive(Debug, Error)]
enum CpuError {
    #[error("usage: cpu <list | offline <processor> | online <processor>>")]
    Usage,
    #[error("unexpected argument `{0}`")]
    UnexpectedArgument(String),
    #[error("invalid processor `{raw}`")]
    InvalidProcessor {
        raw: String,
        #[source]
        source: ParseIntError,
    },
    #[error("processor {processor} did not go offline: {reason}")]
    Offline {
        processor: ProcessorId,
        reason: OfflineReason,
    },
    #[error("processor {processor} did not come online: {reason}")]
    Online {
        processor: ProcessorId,
        reason: OnlineReason,
    },
}

/// [`OfflineError`] as the shell reads it.
#[derive(Debug)]
struct OfflineReason(OfflineError);

impl fmt::Display for OfflineReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            OfflineError::NotPossible(possible) => {
                write!(f, "it is not one of the {possible} possible processors")
            }
            OfflineError::Bootstrap => {
                f.write_str("it is the bootstrap processor, which stays online")
            }
            OfflineError::CurrentProcessor => f.write_str(
                "the request ran on it, and another online processor has to initiate its offline",
            ),
            OfflineError::NotOnline => f.write_str("it is not online"),
            OfflineError::Refused(OfflineRefusal::PinnedTasks(live)) => {
                write!(f, "refused: {live} pinned task(s) still run on it")
            }
            OfflineError::Denied => {
                f.write_str("this program was not granted the right to control processors")
            }
        }
    }
}

/// [`OnlineError`] as the shell reads it.
#[derive(Debug)]
struct OnlineReason(OnlineError);

impl fmt::Display for OnlineReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            OnlineError::NotPossible(possible) => {
                write!(f, "it is not one of the {possible} possible processors")
            }
            OnlineError::NotOffline => f.write_str("it is not offline"),
            OnlineError::Denied => {
                f.write_str("this program was not granted the right to control processors")
            }
        }
    }
}

#[helios_api::main]
async fn main() -> Result<(), CpuError> {
    let mut args = env::args().skip(1);
    let command = args.next().ok_or(CpuError::Usage)?;
    match command.as_str() {
        "list" => {
            no_more(args)?;
            list();
        }
        "offline" => {
            let processor = processor(args.next())?;
            no_more(args)?;
            processors::offline(processor)
                .await
                .map_err(|error| CpuError::Offline {
                    processor,
                    reason: OfflineReason(error),
                })?;
            println!("offline {processor}");
        }
        "online" => {
            let processor = processor(args.next())?;
            no_more(args)?;
            processors::online(processor)
                .await
                .map_err(|error| CpuError::Online {
                    processor,
                    reason: OnlineReason(error),
                })?;
            println!("online {processor}");
        }
        _ => return Err(CpuError::Usage),
    }
    Ok(())
}

fn list() {
    let sets = processors::sets();
    let online = sets
        .online
        .iter()
        .map(ProcessorId::to_string)
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "possible {} bootstrap {} online {online}",
        sets.possible, sets.bootstrap
    );
}

fn processor(raw: Option<String>) -> Result<ProcessorId, CpuError> {
    let raw = raw.ok_or(CpuError::Usage)?;
    raw.parse()
        .map_err(|source| CpuError::InvalidProcessor { raw, source })
}

fn no_more(mut args: impl Iterator<Item = String>) -> Result<(), CpuError> {
    match args.next() {
        Some(extra) => Err(CpuError::UnexpectedArgument(extra)),
        None => Ok(()),
    }
}
