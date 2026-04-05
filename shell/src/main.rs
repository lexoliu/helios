mod edit_tui;
mod filesystem;
mod help;
mod programs;
mod ready;
mod repl;
mod rpc;
mod runtime;
mod script;
mod serial;
mod stats_tui;
mod system;
mod tui;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use std::io::Write as _;

#[derive(Debug, Parser)]
struct Args {
    /// Host serial device used as the shell transport.
    #[arg(long)]
    device: String,

    /// Baud rate for the shell transport.
    #[arg(long, default_value_t = 115_200)]
    baud: u32,

    #[arg(long, hide = true)]
    boot_sync: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the current remote working directory.
    Pwd,
    /// List files in the remote debugger filesystem.
    Ls { path: Option<String> },
    /// Print remote file contents.
    Cat { path: String },
    /// Edit a remote text file inside a full-screen terminal editor.
    Edit { path: String },
    /// Create a directory in the remote debugger filesystem.
    Mkdir { path: String },
    /// Remove a file or an empty directory from the remote debugger filesystem.
    Rm { path: String },
    /// Create a file if it does not exist in the remote debugger filesystem.
    Touch { path: String },
    /// Print text or write it into a remote file with `>` or `>>`.
    Echo {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        words: Vec<String>,
    },
    /// Launch a host-local wasm file inside Helios with the default minimal rights set.
    Run {
        path: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    Stats {
        #[arg(long, default_value_t = 1_000)]
        period_ms: u64,
    },
    Tracing {
        /// Maximum number of recent events kept in the incremental polling window.
        #[arg(long, default_value_t = 64)]
        limit: u32,
        #[arg(long)]
        min_level: Option<String>,
        #[arg(long)]
        target_prefix: Vec<String>,
    },
    Rpc {
        #[arg(long)]
        instance: String,
        #[arg(long)]
        func: String,
        #[arg(long, default_value = "")]
        request_hex: String,
    },
}

fn main() -> Result<()> {
    let args = Args::parse();
    let client = runtime::block_on(async move {
        let io = serial::open(&args.device, args.baud).await?;
        if args.boot_sync {
            Ok::<_, anyhow::Error>(ready::connect_after_boot(io).await?)
        } else {
            let mut client = io.into_client();
            ready::wait_until_ready(&mut client).await?;
            Ok::<_, anyhow::Error>(client)
        }
    })?;

    match args.command {
        None => repl::run(client),
        Some(Command::Pwd) => run_interruptible(async move {
            std::io::stdout().write_all(filesystem::pwd().as_bytes())?;
            std::io::stdout().write_all(b"\n")?;
            Ok(())
        }),
        Some(Command::Ls { path }) => run_interruptible(async move {
            let mut client = client;
            let output = filesystem::list(&mut client, path.as_deref()).await?;
            std::io::stdout().write_all(output.as_bytes())?;
            Ok(())
        }),
        Some(Command::Cat { path }) => run_interruptible(async move {
            let mut client = client;
            let bytes = filesystem::cat(&mut client, &path).await?;
            std::io::stdout().write_all(&bytes)?;
            Ok(())
        }),
        Some(Command::Edit { path }) => run_interruptible(async move {
            let mut client = client;
            edit_tui::run(&mut client, &path).await
        }),
        Some(Command::Mkdir { path }) => run_interruptible(async move {
            let mut client = client;
            filesystem::mkdir(&mut client, &path).await
        }),
        Some(Command::Rm { path }) => run_interruptible(async move {
            let mut client = client;
            filesystem::remove(&mut client, &path).await
        }),
        Some(Command::Touch { path }) => run_interruptible(async move {
            let mut client = client;
            filesystem::touch(&mut client, &path).await
        }),
        Some(Command::Echo { words }) => run_interruptible(async move {
            let mut client = client;
            match filesystem::parse_echo(&words)? {
                filesystem::EchoTarget::Stdout(bytes) => std::io::stdout().write_all(&bytes)?,
                filesystem::EchoTarget::File {
                    path,
                    bytes,
                    append,
                } => filesystem::write(&mut client, &path, &bytes, append).await?,
            }
            Ok(())
        }),
        Some(Command::Run { path, args }) => run_interruptible(async move {
            let mut client = client;
            let started = programs::run(&mut client, &path, &args).await?;
            writeln!(
                std::io::stdout(),
                "started instance {} {}",
                started.instance_id,
                started.name
            )?;
            Ok(())
        }),
        Some(Command::Stats { period_ms }) => run_interruptible(async move {
            let mut client = client;
            stats_tui::run(&mut client, period_ms).await
        }),
        Some(Command::Tracing {
            limit,
            min_level,
            target_prefix,
        }) => run_interruptible(system::run_tracing(
            client,
            limit,
            min_level.as_deref(),
            target_prefix,
        )),
        Some(Command::Rpc {
            instance,
            func,
            request_hex,
        }) => run_interruptible(rpc::run_call(client, &instance, &func, &request_hex)),
    }
}

fn run_interruptible(command: impl std::future::Future<Output = Result<()>>) -> Result<()> {
    match runtime::block_on(runtime::interruptible(command))
        .context("failed to listen for Ctrl+C during command execution")?
    {
        runtime::CommandRun::Completed(result) => result,
        runtime::CommandRun::Interrupted => Ok(()),
    }
}
