mod programs;
mod ready;
mod remote;
mod repl;
mod runtime;
mod serial;
mod stats_tui;
mod system;
mod tui;
mod vm;

use anyhow::{bail, Context as _, Result};
use clap::{Args as ClapArgs, Parser, Subcommand};
use std::io::Write as _;

#[derive(Debug, Parser)]
#[command(name = "helios-inspector")]
struct Args {
    #[command(flatten)]
    serial: SerialOptions,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, ClapArgs)]
struct SerialOptions {
    /// Host serial device used as the inspector transport.
    #[arg(long)]
    device: Option<String>,

    /// Baud rate for the inspector transport.
    #[arg(long, default_value_t = 115_200)]
    baud: u32,

    #[arg(long, hide = true)]
    boot_sync: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Execute a shell script inside the remote guest dash program.
    Dash(DashCommand),
    /// Stream tracing events until interrupted.
    Tracing(TracingCommand),
    /// Open the live system monitor.
    Stats,
    /// Start an interactive shell that forwards most input to remote dash.
    Repl,
    /// Launch a local QEMU VM, wait for the debugger, and connect the inspector.
    Vm(vm::VmCommand),
}

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum SessionCommand {
    /// Execute a shell script inside the remote guest dash program.
    Dash(DashCommand),
    /// Stream tracing events until interrupted.
    Tracing(TracingCommand),
    /// Open the live system monitor.
    Stats,
    /// Start an interactive shell that forwards most input to remote dash.
    Repl,
}

#[derive(Debug, Clone, ClapArgs)]
pub(crate) struct DashCommand {
    /// Inline script passed to `dash -c`.
    #[arg(short = 'c', long = "command", conflicts_with = "script")]
    command: Option<String>,

    /// Path to a local script file whose contents are executed remotely.
    #[arg(conflicts_with = "command")]
    script: Option<String>,
}

#[derive(Debug, Clone, ClapArgs)]
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
    match args.command {
        Some(Command::Vm(command)) => vm::run(command),
        command => run_serial(args.serial, into_session_command(command)),
    }
}

fn run_serial(serial: SerialOptions, command: Option<SessionCommand>) -> Result<()> {
    let device = serial.device.as_deref().ok_or_else(|| {
        anyhow::anyhow!("--device is required unless using `helios-inspector vm`")
    })?;
    let client = connect_client(device, serial.baud, serial.boot_sync)?;
    run_connected(client, command)
}

pub(crate) fn connect_client(
    device: &str,
    baud: u32,
    boot_sync: bool,
) -> Result<serial::RpcClient> {
    runtime::block_on(async move {
        let io = serial::open(device, baud).await?;
        if boot_sync {
            Ok::<_, anyhow::Error>(ready::connect_after_boot(io).await?)
        } else {
            let mut client = io.into_client();
            ready::wait_until_ready(&mut client).await?;
            Ok::<_, anyhow::Error>(client)
        }
    })
}

pub(crate) fn run_connected(
    client: serial::RpcClient,
    command: Option<SessionCommand>,
) -> Result<()> {
    match command.unwrap_or(SessionCommand::Repl) {
        SessionCommand::Dash(command) => run_interruptible(async move {
            let mut client = client;
            let output = repl::run_dash_command(&mut client, &command).await?;
            std::io::stdout().write_all(&output.output.stdout)?;
            std::io::stderr().write_all(&output.output.stderr)?;
            if output.exit_code != 0 {
                bail!("remote dash exited with code {}", output.exit_code);
            }
            Ok(())
        }),
        SessionCommand::Tracing(command) => run_interruptible(system::run_tracing(
            client,
            command.limit,
            command.min_level.as_deref(),
            command.target_prefix,
        )),
        SessionCommand::Stats => run_interruptible(async move {
            let mut client = client;
            stats_tui::run(&mut client).await
        }),
        SessionCommand::Repl => repl::run(client),
    }
}

fn into_session_command(command: Option<Command>) -> Option<SessionCommand> {
    match command {
        Some(Command::Dash(command)) => Some(SessionCommand::Dash(command)),
        Some(Command::Tracing(command)) => Some(SessionCommand::Tracing(command)),
        Some(Command::Stats) => Some(SessionCommand::Stats),
        Some(Command::Repl) => Some(SessionCommand::Repl),
        Some(Command::Vm(_)) => {
            unreachable!("vm command must be handled before connecting transport")
        }
        None => None,
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
