mod programs;
mod ready;
mod remote;
mod repl;
mod runtime;
mod serial;
mod stats_tui;
mod system;
mod tui;

use anyhow::{Context as _, Result};
use clap::{Args as ClapArgs, Parser, Subcommand};
use std::io::Write as _;

#[derive(Debug, Parser)]
#[command(name = "helios-inspector")]
struct Args {
    /// Host serial device used as the inspector transport.
    #[arg(long)]
    device: String,

    /// Baud rate for the inspector transport.
    #[arg(long, default_value_t = 115_200)]
    baud: u32,

    #[arg(long, hide = true)]
    boot_sync: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Execute a shell script inside the remote guest dash program.
    Dash(DashCommand),
    /// Stream tracing events until interrupted.
    Tracing(TracingCommand),
    /// Open the live system monitor.
    Stats,
    /// Start an interactive shell that forwards most input to remote dash.
    Repl,
}

#[derive(Debug, ClapArgs)]
pub(crate) struct DashCommand {
    /// Inline script passed to `dash -c`.
    #[arg(short = 'c', long = "command", conflicts_with = "script")]
    command: Option<String>,

    /// Path to a local script file whose contents are executed remotely.
    #[arg(conflicts_with = "command")]
    script: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub(crate) struct TracingCommand {
    /// Maximum number of recent events kept in the incremental polling window.
    #[arg(long, default_value_t = 64)]
    limit: u32,

    #[arg(long)]
    min_level: Option<String>,

    #[arg(long)]
    target_prefix: Vec<String>,
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
        Some(Command::Dash(command)) => run_interruptible(async move {
            let mut client = client;
            let output = repl::run_dash_command(&mut client, &command).await?;
            std::io::stdout().write_all(&output.output.stdout)?;
            std::io::stderr().write_all(&output.output.stderr)?;
            if output.exit_code != 0 {
                anyhow::bail!("remote dash exited with code {}", output.exit_code);
            }
            Ok(())
        }),
        Some(Command::Tracing(command)) => run_interruptible(system::run_tracing(
            client,
            command.limit,
            command.min_level.as_deref(),
            command.target_prefix,
        )),
        Some(Command::Stats) => run_interruptible(async move {
            let mut client = client;
            stats_tui::run(&mut client).await
        }),
        Some(Command::Repl) | None => repl::run(client),
    }
}

fn run_interruptible(command: impl std::future::Future<Output = Result<()>>) -> Result<()> {
    match runtime::block_on(runtime::interruptible(command))
        .context("failed to listen for Ctrl+C during inspector command execution")?
    {
        runtime::CommandRun::Completed(result) => result,
        runtime::CommandRun::Interrupted => Ok(()),
    }
}
