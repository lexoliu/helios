mod rpc;
mod runtime;
mod serial;
mod system;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
struct Args {
    /// Host serial device used as the shell transport.
    #[arg(long)]
    device: String,

    /// Baud rate for the shell transport.
    #[arg(long, default_value_t = 115_200)]
    baud: u32,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
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
    runtime::block_on(async move {
        let args = Args::parse();
        let io = serial::open(&args.device, args.baud).await?;
        match args.command {
            None => tui::run(io).await,
            Some(Command::Stats { period_ms }) => system::run_stats(io, period_ms).await,
            Some(Command::Tracing {
                limit,
                min_level,
                target_prefix,
            }) => system::run_tracing(io, limit, min_level.as_deref(), target_prefix).await,
            Some(Command::Rpc {
                instance,
                func,
                request_hex,
            }) => rpc::run_call(io, &instance, &func, &request_hex).await,
        }
    })
}
