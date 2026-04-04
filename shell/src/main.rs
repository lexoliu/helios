mod filesystem;
mod ready;
mod repl;
mod rpc;
mod runtime;
mod serial;
mod stats_tui;
mod system;

use anyhow::Result;
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
    /// List files in the remote debugger filesystem.
    Ls { path: Option<String> },
    /// Remove a file or an empty directory from the remote debugger filesystem.
    Rm { path: String },
    /// Create a file if it does not exist in the remote debugger filesystem.
    Touch { path: String },
    Stats {
        #[arg(long, default_value_t = 1_000)]
        period_ms: u64,
    },
    Tracing {
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
        Some(Command::Ls { path }) => runtime::block_on(async move {
            let mut client = client;
            let output = filesystem::list(&mut client, path.as_deref()).await?;
            std::io::stdout().write_all(output.as_bytes())?;
            Ok(())
        }),
        Some(Command::Rm { path }) => runtime::block_on(async move {
            let mut client = client;
            filesystem::remove(&mut client, &path).await
        }),
        Some(Command::Touch { path }) => runtime::block_on(async move {
            let mut client = client;
            filesystem::touch(&mut client, &path).await
        }),
        Some(Command::Stats { period_ms }) => runtime::block_on(async move {
            let mut client = client;
            stats_tui::run(&mut client, period_ms).await
        }),
        Some(Command::Tracing {
            limit,
            min_level,
            target_prefix,
        }) => runtime::block_on(system::run_tracing(
            client,
            limit,
            min_level.as_deref(),
            target_prefix,
        )),
        Some(Command::Rpc {
            instance,
            func,
            request_hex,
        }) => runtime::block_on(rpc::run_call(client, &instance, &func, &request_hex)),
    }
}
