//! `procbench`: the process-model workloads of the benchmark suite.
//!
//! Every subcommand launches children through `helios:system/programs`,
//! which is the only way one Helios program starts another, and reports
//! what the design claims are about:
//!
//! - `startup <n> <child> [args..]` spawns `n` instances at once and measures
//!   each one's time to first output, then samples the memory the batch
//!   occupies while every child is still alive.
//! - `spawn-wait <n> <child> [args..]` spawns and waits for one child at a
//!   time, `n` times.
//! - `pingpong <rounds> <bytes> <child> [args..]` sends `rounds` messages of
//!   `bytes` through the child's stdin and waits for each to come back.
//! - `stream <total> <child> [args..]` pushes `total` bytes through the child
//!   and drains them from its stdout concurrently.
//!
//! `tools/bench/native/procbench.c` is the Linux counterpart with the same
//! subcommands and the same output lines.

use std::env;
use std::num::ParseIntError;
use std::time::Instant;

use helios_api::channel;
use helios_api::programs::{self, Child, ExecRequest, SpawnRequest};
use helios_api::{stats, task};
use helios_bench_metrics::{LatencySamples, mib_per_second, report_metric};
use thiserror::Error;

const STREAM_CHUNK_BYTES: usize = 64 * 1024;
const READ_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug, Error)]
enum ProcbenchError {
    #[error(
        "usage: procbench startup <n> <child> [args..] | spawn-wait <n> <child> [args..] | pingpong <rounds> <bytes> <child> [args..] | stream <total-bytes> <child> [args..]"
    )]
    Usage,
    #[error("unknown subcommand `{0}`")]
    UnknownCommand(String),
    #[error("invalid number `{raw}`")]
    InvalidNumber {
        raw: String,
        #[source]
        source: ParseIntError,
    },
    #[error("spawning {path} failed: {kind:?}: {detail}")]
    Spawn {
        path: String,
        kind: programs::SpawnErrorKind,
        detail: String,
    },
    #[error("executing {path} failed: {kind:?}: {detail}")]
    Exec {
        path: String,
        kind: programs::ExecErrorKind,
        detail: String,
    },
    #[error("child {path} exited with code {code}")]
    ChildFailed { path: String, code: u32 },
    #[error("child {path} closed its stdout before producing output")]
    NoOutput { path: String },
    #[error("child returned a corrupted message in round {round}")]
    CorruptEcho { round: u64 },
    #[error("the child's stdin closed after {written} of {total} bytes")]
    StdinClosed { written: u64, total: u64 },
    #[error("the child echoed {echoed} bytes, expected {expected}")]
    ShortStream { echoed: u64, expected: u64 },
    #[error("a worker task ended without reporting its result")]
    WorkerLost,
}

#[derive(Clone)]
struct ChildSpec {
    path: String,
    args: Vec<String>,
}

impl ChildSpec {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, ProcbenchError> {
        let path = args.next().ok_or(ProcbenchError::Usage)?;
        Ok(Self {
            path,
            args: args.collect(),
        })
    }

    fn name(&self) -> String {
        self.path
            .rsplit('/')
            .next()
            .unwrap_or(&self.path)
            .to_owned()
    }

    fn spawn_request(&self) -> SpawnRequest {
        SpawnRequest {
            name: self.name(),
            args: self.args.clone(),
            env: Vec::new(),
            path: self.path.clone(),
            capability_grants: Vec::new(),
        }
    }

    async fn spawn(&self) -> Result<Child, ProcbenchError> {
        programs::spawn(self.spawn_request())
            .await
            .map_err(|error| ProcbenchError::Spawn {
                path: self.path.clone(),
                kind: error.kind,
                detail: error.detail,
            })
    }
}

#[helios_api::main]
async fn main() -> Result<(), ProcbenchError> {
    let mut args = env::args().skip(1);
    let command = args.next().ok_or(ProcbenchError::Usage)?;
    match command.as_str() {
        "startup" => {
            let count = parse_number(args.next())?;
            startup(count, ChildSpec::parse(args)?).await
        }
        "spawn-wait" => {
            let count = parse_number(args.next())?;
            spawn_wait(count, ChildSpec::parse(args)?).await
        }
        "pingpong" => {
            let rounds = parse_number(args.next())?;
            let bytes = parse_number(args.next())?;
            pingpong(rounds, bytes, ChildSpec::parse(args)?).await
        }
        "stream" => {
            let total = parse_number(args.next())?;
            stream(total, ChildSpec::parse(args)?).await
        }
        other => Err(ProcbenchError::UnknownCommand(other.to_owned())),
    }
}

fn parse_number(raw: Option<String>) -> Result<u64, ProcbenchError> {
    let raw = raw.ok_or(ProcbenchError::Usage)?;
    let value = raw
        .parse::<u64>()
        .map_err(|source| ProcbenchError::InvalidNumber { raw, source })?;
    if value == 0 {
        return Err(ProcbenchError::Usage);
    }
    Ok(value)
}

async fn startup(count: u64, child: ChildSpec) -> Result<(), ProcbenchError> {
    let memory_before = stats::snapshot().memory.available_bytes;
    let (tx, rx) = channel::bounded(usize::try_from(count).expect("count fits usize"));
    let batch_started = Instant::now();
    for _ in 0..count {
        let child = child.clone();
        let tx = tx.clone();
        task::spawn(async move {
            let outcome = spawn_until_first_output(&child).await;
            // The receiver only disappears when main already failed, and
            // then there is nobody left to report to.
            let _ = tx.send(outcome).await;
        });
    }
    drop(tx);

    let mut samples =
        LatencySamples::with_capacity(usize::try_from(count).expect("count fits usize"));
    let mut children = Vec::with_capacity(usize::try_from(count).expect("count fits usize"));
    for _ in 0..count {
        let (child, elapsed) = rx.recv().await.map_err(|_| ProcbenchError::WorkerLost)??;
        samples.record(elapsed);
        children.push(child);
    }
    let batch_elapsed = batch_started.elapsed();

    // Every child is alive and blocked on stdin here, so the difference in
    // available memory is what the batch costs the kernel. The per-instance
    // registry (`helios:system/instances`) is not consulted: it belongs to
    // the privileged debugger world, and the batch is proven alive by
    // construction, since every child has written its first line and none
    // has had its stdin closed yet.
    let memory_after = stats::snapshot().memory.available_bytes;

    for handle in children {
        // Closing stdin releases a `hello hold` child; the batch is
        // dismantled only after its footprint was sampled.
        handle
            .write_stdin(Vec::new())
            .await
            .map_err(|()| ProcbenchError::StdinClosed {
                written: 0,
                total: 0,
            })?;
        wait_child(handle, &child.path).await?;
    }

    println!("instance-startup:{count}");
    samples.report("first_output");
    report_metric(
        "batch_ms",
        format!("{:.3}", batch_elapsed.as_secs_f64() * 1_000.0),
    );
    report_metric(
        "memory_per_instance_bytes",
        memory_before.saturating_sub(memory_after) / count,
    );
    Ok(())
}

async fn spawn_until_first_output(
    child: &ChildSpec,
) -> Result<(Child, std::time::Duration), ProcbenchError> {
    let started = Instant::now();
    let handle = child.spawn().await?;
    let (mut stdout, _completion) = handle.stdout();
    let (result, chunk) = stdout.read(Vec::with_capacity(READ_CHUNK_BYTES)).await;
    let elapsed = started.elapsed();
    if chunk.is_empty() && helios_api::stream_closed(result) {
        return Err(ProcbenchError::NoOutput {
            path: child.path.clone(),
        });
    }
    Ok((handle, elapsed))
}

async fn wait_child(child: Child, path: &str) -> Result<(), ProcbenchError> {
    let status = child.wait().await.map_err(|error| ProcbenchError::Spawn {
        path: path.to_owned(),
        kind: error.kind,
        detail: error.detail,
    })?;
    if status.code != 0 {
        return Err(ProcbenchError::ChildFailed {
            path: path.to_owned(),
            code: status.code,
        });
    }
    Ok(())
}

async fn spawn_wait(count: u64, child: ChildSpec) -> Result<(), ProcbenchError> {
    let mut samples =
        LatencySamples::with_capacity(usize::try_from(count).expect("count fits usize"));
    for _ in 0..count {
        let started = Instant::now();
        let result = programs::exec(ExecRequest {
            name: child.name(),
            args: child.args.clone(),
            env: Vec::new(),
            path: child.path.clone(),
            stdin: Vec::new(),
            hint: None,
            capability_grants: Vec::new(),
        })
        .await
        .map_err(|error| ProcbenchError::Exec {
            path: child.path.clone(),
            kind: error.kind,
            detail: error.detail,
        })?;
        samples.record(started.elapsed());
        if result.exit_code != 0 {
            return Err(ProcbenchError::ChildFailed {
                path: child.path.clone(),
                code: result.exit_code,
            });
        }
    }
    println!("spawn-wait:{count}");
    samples.report("spawn_wait");
    Ok(())
}

async fn pingpong(rounds: u64, bytes: u64, child: ChildSpec) -> Result<(), ProcbenchError> {
    let message_len = usize::try_from(bytes).expect("message size fits usize");
    let handle = child.spawn().await?;
    let (mut writer, reader) = helios_api::bindings::wit_stream::new::<u8>();
    let stdin_done = handle.pipe_stdin(reader);
    let (mut stdout, _completion) = handle.stdout();

    let mut samples =
        LatencySamples::with_capacity(usize::try_from(rounds).expect("rounds fit usize"));
    let mut pending = Vec::with_capacity(message_len);
    for round in 0..rounds {
        let message: Vec<u8> = (0..message_len)
            .map(|index| (round as usize + index) as u8)
            .collect();
        let started = Instant::now();
        let unwritten = writer.write_all(message.clone()).await;
        if !unwritten.is_empty() {
            return Err(ProcbenchError::StdinClosed {
                written: round * bytes,
                total: rounds * bytes,
            });
        }
        pending.clear();
        while pending.len() < message_len {
            let (result, chunk) = stdout
                .read(Vec::with_capacity(message_len - pending.len()))
                .await;
            pending.extend_from_slice(&chunk);
            if chunk.is_empty() && helios_api::stream_closed(result) {
                return Err(ProcbenchError::NoOutput {
                    path: child.path.clone(),
                });
            }
        }
        samples.record(started.elapsed());
        if pending != message {
            return Err(ProcbenchError::CorruptEcho { round });
        }
    }
    drop(writer);
    let _ = std::future::IntoFuture::into_future(stdin_done).await;
    wait_child(handle, &child.path).await?;

    println!("pipe-pingpong:{rounds}");
    samples.report("rtt");
    Ok(())
}

async fn stream(total: u64, child: ChildSpec) -> Result<(), ProcbenchError> {
    let handle = child.spawn().await?;
    let (mut writer, reader) = helios_api::bindings::wit_stream::new::<u8>();
    let stdin_done = handle.pipe_stdin(reader);
    let (mut stdout, _completion) = handle.stdout();

    let started = Instant::now();
    let (tx, rx) = channel::bounded(1);
    task::spawn(async move {
        let mut written = 0u64;
        let outcome = loop {
            if written >= total {
                break Ok(());
            }
            let len = usize::try_from((total - written).min(STREAM_CHUNK_BYTES as u64))
                .expect("chunk fits usize");
            let chunk: Vec<u8> = (0..len)
                .map(|index| (written as usize + index) as u8)
                .collect();
            let unwritten = writer.write_all(chunk).await;
            if !unwritten.is_empty() {
                break Err(ProcbenchError::StdinClosed { written, total });
            }
            written += len as u64;
        };
        drop(writer);
        let _ = std::future::IntoFuture::into_future(stdin_done).await;
        let _ = tx.send(outcome).await;
    });

    let mut echoed = 0u64;
    loop {
        let (result, chunk) = stdout.read(Vec::with_capacity(READ_CHUNK_BYTES)).await;
        echoed += chunk.len() as u64;
        if chunk.is_empty() && helios_api::stream_closed(result) {
            break;
        }
    }
    let elapsed = started.elapsed();
    rx.recv().await.map_err(|_| ProcbenchError::WorkerLost)??;
    wait_child(handle, &child.path).await?;
    if echoed != total {
        return Err(ProcbenchError::ShortStream {
            echoed,
            expected: total,
        });
    }

    println!("pipe-stream:{total}");
    report_metric("mib_per_s", mib_per_second(total, elapsed));
    Ok(())
}
