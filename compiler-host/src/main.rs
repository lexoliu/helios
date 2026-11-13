//! `helios-compiler-host`: the compiler plugin, resident under Wasmtime on
//! this system, answering one compile per request line.
//!
//! It starts, precompiles the plugin, writes `{"type":"ready"}` on stdout
//! and waits. Every `compile` line on stdin compiles `--input` for
//! `--target` at ISA level `--isa-level` under `--hint` with `--workers`
//! workers, writes the artifact
//! to `--output`, and answers one `{"type":"compiled",...}` JSON line on
//! stdout. It exits 0 at the end of stdin, and on any failure logs it to
//! stderr, answers nothing and exits non-zero. A harness times each request
//! from writing the line to reading the answer, the way the bench times the
//! Helios kernel's compile around its RPC.

use std::io::{BufRead, Write};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use helios_compiler_abi::CompileHint;
use helios_compiler_host::{CompilerHost, CompilerHostError, on_wasm_thread};
use serde::Serialize;
use thiserror::Error;
use tracing_log::LogTracer;
use tracing_log::log::LevelFilter;

/// The one request the protocol has.
const COMPILE_REQUEST: &str = "compile";

#[derive(Debug, Parser)]
#[command(about = "Host the Helios compiler plugin under Wasmtime and serve compile requests")]
struct Args {
    /// The compiler plugin's core module (`compiler_plugin.wasm`).
    #[arg(long)]
    plugin: PathBuf,
    /// The wasm every request compiles, read again for each request.
    #[arg(long)]
    input: PathBuf,
    /// Where every request writes the compiled artifact.
    #[arg(long)]
    output: PathBuf,
    /// The target triple the plugin compiles for.
    #[arg(long)]
    target: String,
    /// The name of the target's ISA level the artifact is compiled for, as
    /// the Helios kernel sends it, such as `x86-64-v4+vbmi+bitalg`.
    #[arg(long)]
    isa_level: String,
    #[arg(long, value_enum)]
    hint: Hint,
    /// The worker count the plugin is told through `RAYON_NUM_THREADS`.
    #[arg(long)]
    workers: NonZeroU32,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Hint {
    Balanced,
    Performance,
}

impl From<Hint> for CompileHint {
    fn from(hint: Hint) -> Self {
        match hint {
            Hint::Balanced => CompileHint::Balanced,
            Hint::Performance => CompileHint::Performance,
        }
    }
}

#[derive(Debug, Error)]
enum ServeError {
    #[error("failed to read the {what} {path}: {source}")]
    Read {
        what: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write the compiled output to {path}: {source}")]
    WriteOutput {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read a request from stdin: {0}")]
    Stdin(#[source] std::io::Error),
    #[error("expected a `{COMPILE_REQUEST}` request line, read {line:?}")]
    Protocol { line: String },
    #[error("failed to encode the reply: {0}")]
    EncodeReply(#[source] serde_json::Error),
    #[error("failed to write the reply to stdout: {0}")]
    Reply(#[source] std::io::Error),
    #[error(transparent)]
    Host(#[from] CompilerHostError),
}

/// One line the host writes to stdout.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Reply {
    /// The plugin is precompiled and the host reads requests: written once,
    /// before the first request, so that start-up is never timed as a
    /// compile.
    Ready,
    /// The answer to one `compile` request.
    Compiled {
        output_bytes: usize,
        metrics: ReplyMetrics,
    },
}

/// The measurements the bench records beside the request's duration.
#[derive(Serialize)]
struct ReplyMetrics {
    /// The worker count the plugin compiled with.
    compiler_workers: u32,
}

fn main() -> ExitCode {
    let subscriber = tracing_subscriber::fmt::Subscriber::builder()
        .with_writer(std::io::stderr)
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("no other subscriber is installed before main");
    LogTracer::init_with_filter(LevelFilter::Info)
        .expect("no other logger is installed before main");
    let args = Args::parse();
    let outcome = match on_wasm_thread(move || serve(&args)) {
        Ok(served) => served,
        Err(error) => Err(error.into()),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn serve(args: &Args) -> Result<(), ServeError> {
    let plugin = read(&args.plugin, "compiler plugin")?;
    let mut host = CompilerHost::new(&plugin, args.workers)?;
    drop(plugin);
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let mut stdout = std::io::stdout().lock();
    write_reply(&mut stdout, &Reply::Ready)?;
    let mut line = String::new();
    loop {
        line.clear();
        if stdin.read_line(&mut line).map_err(ServeError::Stdin)? == 0 {
            return Ok(());
        }
        if line.trim_end_matches('\n') != COMPILE_REQUEST {
            return Err(ServeError::Protocol { line });
        }
        let input = read(&args.input, "input")?;
        let compiled = host.compile(&input, &args.target, &args.isa_level, args.hint.into())?;
        std::fs::write(&args.output, &compiled.output).map_err(|source| {
            ServeError::WriteOutput {
                path: args.output.clone(),
                source,
            }
        })?;
        let reply = Reply::Compiled {
            output_bytes: compiled.output.len(),
            metrics: ReplyMetrics {
                compiler_workers: host.workers().get(),
            },
        };
        write_reply(&mut stdout, &reply)?;
    }
}

fn write_reply(stdout: &mut impl Write, reply: &Reply) -> Result<(), ServeError> {
    serde_json::to_writer(&mut *stdout, reply).map_err(ServeError::EncodeReply)?;
    stdout
        .write_all(b"\n")
        .and_then(|()| stdout.flush())
        .map_err(ServeError::Reply)
}

fn read(path: &Path, what: &'static str) -> Result<Vec<u8>, ServeError> {
    std::fs::read(path).map_err(|source| ServeError::Read {
        what,
        path: path.to_owned(),
        source,
    })
}
