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
mod vsock;
mod workload_bench;

use clap::{Args as ClapArgs, Parser, Subcommand};
use std::io::Write as _;
use std::path::PathBuf;

use crate::ready::BootError;
use crate::repl::{ReplError, ShellScriptError};
use crate::serial::SerialError;
use crate::system::SystemError;
use crate::tui::TerminalError;

/// Why an inspector run ended other than by doing what it was asked.
///
/// The three ways in are separate variants because they are answered in
/// different places: the command line, the transport to a guest, and the
/// session that ran once the transport was up.
#[derive(Debug, thiserror::Error)]
enum InspectorError {
    #[error("--device is required unless using `helios-inspector vm`")]
    MissingDevice,
    #[error("{0}")]
    Vm(#[from] vm::VmError),
    #[error("{0}")]
    Connect(#[from] ConnectError),
    #[error("{0}")]
    Session(#[from] SessionError),
}

/// Why the inspector could not reach the guest debugger.
///
/// A transport that never opened and a guest that never came up are
/// different problems: the first is the host's device or socket, the
/// second is the guest's own boot.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ConnectError {
    #[error("{0}")]
    Transport(#[from] SerialError),
    #[error("{0}")]
    Boot(#[from] BootError),
}

/// Why the session that ran over an open transport failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SessionError {
    #[error("failed to prime the guest tracing capture: {source}")]
    PrimeCapture {
        #[source]
        source: SystemError,
    },
    #[error("failed to drain the guest tracing capture: {source}")]
    DrainCapture {
        #[source]
        source: SystemError,
    },
    #[error("failed to create the guest trace log at {path}: {source}")]
    CreateTraceLog {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write the guest trace log at {path}: {source}")]
    WriteTraceLog {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Shell(#[from] ShellScriptError),
    #[error("failed to write the remote shell output: {source}")]
    WriteShellOutput {
        #[source]
        source: std::io::Error,
    },
    #[error("remote shell exited with code {exit_code}")]
    RemoteShellExited { exit_code: u32 },
    #[error("{0}")]
    Tracing(#[from] SystemError),
    #[error("{0}")]
    Stats(#[from] TerminalError),
    #[error("{0}")]
    Repl(#[from] ReplError),
    #[error("{0}")]
    Interrupt(#[from] InterruptError),
}

/// The inspector could not arm the Ctrl+C handler a cancellable command
/// runs under.
///
/// A command that cannot be interrupted is not run: the operator would
/// have no way back out of it short of killing the process, which loses
/// the terminal it left in raw mode.
#[derive(Debug, thiserror::Error)]
#[error("failed to listen for Ctrl+C during inspector command execution: {source}")]
pub(crate) struct InterruptError {
    #[source]
    source: std::io::Error,
}

#[derive(Debug, Parser)]
#[command(
    name = "helios-inspector",
    version,
    about = "Inspect and drive a Helios guest over the debugger transport",
    long_about = "Connects to the in-guest debugger component over a serial \
                  transport (or boots one with `vm`) and exposes the guest \
                  shell, tracing stream, and live system monitor."
)]
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
    /// Execute a shell script inside the remote guest shell program (`/bin/dash`).
    Shell(ShellCommand),
    /// Stream tracing events until interrupted.
    Tracing(TracingCommand),
    /// Open the live system monitor.
    Stats,
    /// List the live program instances, and optionally stop one.
    Instances(InstancesCommand),
    /// Start an interactive shell that forwards most input to the remote shell.
    Repl,
    /// Launch a local QEMU VM, wait for the debugger, and connect the inspector.
    ///
    /// Boxed: the VM options dwarf every other subcommand's, and the
    /// parsed command is moved around by value.
    Vm(Box<vm::VmCommand>),
}

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum SessionCommand {
    /// Execute a shell script inside the remote guest shell program (`/bin/dash`).
    Shell(ShellCommand),
    /// Stream tracing events until interrupted.
    Tracing(TracingCommand),
    /// Open the live system monitor.
    Stats,
    /// List the live program instances, and optionally stop one.
    Instances(InstancesCommand),
    /// Start an interactive shell that forwards most input to the remote shell.
    Repl,
}

#[derive(Debug, Clone, ClapArgs)]
pub(crate) struct ShellCommand {
    /// Inline script passed to `/bin/dash -c`.
    #[arg(short = 'c', long = "command", conflicts_with = "script")]
    command: Option<String>,

    /// Path to a local script file whose contents are executed remotely by `/bin/dash`.
    #[arg(conflicts_with = "command")]
    script: Option<String>,

    /// Write the guest tracing events the script produced to this file.
    ///
    /// The capture is primed before the script runs and drained after it, so
    /// the file holds the guest's own account of the run — including a run
    /// that failed, which is the case the file exists for.
    #[arg(long = "trace-log", value_name = "PATH")]
    trace_log: Option<PathBuf>,

    /// Lowest severity to capture into `--trace-log`.
    #[arg(
        long = "trace-log-level",
        value_name = "LEVEL",
        default_value = "trace"
    )]
    trace_log_level: String,

    /// Number of recent guest events each `--trace-log` poll retrieves.
    #[arg(long = "trace-log-limit", value_name = "COUNT", default_value_t = 512)]
    trace_log_limit: u32,
}

/// Which live instances there are, and which of them to stop.
///
/// Stopping one by name rather than by identifier is what a script
/// wants: the identifiers a registry hands out are whatever the boot
/// happened to allocate, while `compositor-plugin` is the same name on
/// every boot.
#[derive(Debug, Clone, ClapArgs)]
pub(crate) struct InstancesCommand {
    /// Stop the instance carrying this identifier.
    #[arg(long = "kill", value_name = "ID", conflicts_with = "kill_name")]
    kill: Option<u64>,

    /// Stop the instance registered under this name.
    #[arg(long = "kill-name", value_name = "NAME", conflicts_with = "kill")]
    kill_name: Option<String>,
}

#[derive(Debug, Clone, ClapArgs)]
pub(crate) struct TracingCommand {
    /// Maximum number of recent events kept in the incremental polling window.
    #[arg(long, default_value_t = 64)]
    limit: u32,

    /// Lowest severity to stream (trace, debug, info, warn, or error).
    #[arg(long)]
    min_level: Option<String>,

    /// Only stream events whose target starts with one of these prefixes;
    /// repeat the flag to allow several prefixes.
    #[arg(long)]
    target_prefix: Vec<String>,
}

fn main() -> Result<(), InspectorError> {
    let args = Args::parse();
    let session = match args.command {
        Some(Command::Vm(command)) => return Ok(vm::run(*command)?),
        Some(Command::Shell(command)) => Some(SessionCommand::Shell(command)),
        Some(Command::Tracing(command)) => Some(SessionCommand::Tracing(command)),
        Some(Command::Stats) => Some(SessionCommand::Stats),
        Some(Command::Instances(command)) => Some(SessionCommand::Instances(command)),
        Some(Command::Repl) => Some(SessionCommand::Repl),
        None => None,
    };
    let device = args
        .serial
        .device
        .as_deref()
        .ok_or(InspectorError::MissingDevice)?;
    let client = connect_client(device, args.serial.baud, args.serial.boot_sync)?;
    Ok(run_connected(client, session)?)
}

pub(crate) fn connect_client(
    device: &str,
    baud: u32,
    boot_sync: bool,
) -> Result<serial::RpcClient, ConnectError> {
    runtime::block_on(async move {
        let io = serial::open(device, baud).await?;
        if boot_sync {
            Ok(ready::connect_after_boot(io).await?)
        } else {
            let mut client = io.into_client();
            ready::wait_until_ready(&mut client).await?;
            Ok(client)
        }
    })
}

pub(crate) fn run_connected(
    client: serial::RpcClient,
    command: Option<SessionCommand>,
) -> Result<(), SessionError> {
    match command.unwrap_or(SessionCommand::Repl) {
        SessionCommand::Shell(command) => run_interruptible(async move {
            let mut client = client;
            let mut capture = match command.trace_log.as_ref() {
                Some(_) => Some(
                    system::TracingCapture::start(
                        &mut client,
                        system::tracing_config(
                            command.trace_log_limit,
                            Some(command.trace_log_level.as_str()),
                            Vec::new(),
                        )?,
                    )
                    .await
                    .map_err(|source| SessionError::PrimeCapture { source })?,
                ),
                None => None,
            };
            let result = repl::run_shell_command(&mut client, &command).await;
            if let (Some(capture), Some(path)) = (capture.as_mut(), command.trace_log.as_ref()) {
                write_trace_log(&mut client, capture, path).await?;
            }
            let output = result?;
            std::io::stdout()
                .write_all(&output.output.stdout)
                .and_then(|()| std::io::stderr().write_all(&output.output.stderr))
                .map_err(|source| SessionError::WriteShellOutput { source })?;
            if output.exit_code != 0 {
                return Err(SessionError::RemoteShellExited {
                    exit_code: output.exit_code,
                });
            }
            Ok(())
        }),
        SessionCommand::Tracing(command) => Ok(runtime::block_on(system::run_tracing(
            client,
            command.limit,
            command.min_level.as_deref(),
            command.target_prefix,
        ))?),
        SessionCommand::Stats => run_interruptible(async move {
            let mut client = client;
            Ok(stats_tui::run(&mut client).await?)
        }),
        SessionCommand::Instances(command) => run_interruptible(async move {
            let mut client = client;
            Ok(
                system::run_instances(&mut client, command.kill, command.kill_name.as_deref())
                    .await?,
            )
        }),
        SessionCommand::Repl => Ok(repl::run(client)?),
    }
}

/// Writes what the guest traced during the shell command to `path`.
///
/// A failed script is exactly when this file matters, so a failure to drain or
/// write it is reported rather than swallowed: an empty or missing trace log
/// would be read as "the guest said nothing", which is a different claim.
async fn write_trace_log(
    client: &mut serial::RpcClient,
    capture: &mut system::TracingCapture,
    path: &PathBuf,
) -> Result<(), SessionError> {
    let lines = capture
        .drain(client)
        .await
        .map_err(|source| SessionError::DrainCapture { source })?;
    let mut log = std::fs::File::create(path).map_err(|source| SessionError::CreateTraceLog {
        path: path.display().to_string(),
        source,
    })?;
    for line in lines {
        writeln!(log, "{line}").map_err(|source| SessionError::WriteTraceLog {
            path: path.display().to_string(),
            source,
        })?;
    }
    Ok(())
}

fn run_interruptible<E>(command: impl std::future::Future<Output = Result<(), E>>) -> Result<(), E>
where
    E: From<InterruptError>,
{
    match runtime::block_on(runtime::interruptible(command))
        .map_err(|source| InterruptError { source })?
    {
        runtime::CommandRun::Completed(result) => result,
        runtime::CommandRun::Interrupted => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::{Args, Command};
    use clap::Parser;

    #[test]
    fn parses_shell_subcommand() {
        let args = Args::try_parse_from(["helios-inspector", "shell", "-c", "echo hi"])
            .expect("shell subcommand must parse");
        match args.command.expect("subcommand must be present") {
            Command::Shell(_) => {}
            _ => panic!("expected shell subcommand"),
        }
    }

    #[test]
    fn rejects_legacy_dash_subcommand() {
        let result = Args::try_parse_from(["helios-inspector", "dash", "-c", "echo hi"]);
        assert!(result.is_err(), "legacy dash subcommand must stay removed");
    }

    #[test]
    fn parses_vm_shell_subcommand() {
        let args = Args::try_parse_from([
            "helios-inspector",
            "vm",
            "--arch",
            "riscv64",
            "--no-build",
            "shell",
            "-c",
            "echo hi",
        ])
        .expect("vm shell subcommand must parse");
        match args.command.expect("subcommand must be present") {
            Command::Vm(_) => {}
            _ => panic!("expected vm command"),
        }
    }

    #[test]
    fn parses_vm_release_flag() {
        let args = Args::try_parse_from([
            "helios-inspector",
            "vm",
            "--arch",
            "x86-64",
            "--release",
            "repl",
        ])
        .expect("vm release flag must parse");
        match args.command.expect("subcommand must be present") {
            Command::Vm(_) => {}
            _ => panic!("expected vm command"),
        }
    }

    #[test]
    fn parses_vm_debug_diagnostics_flags() {
        let args = Args::try_parse_from([
            "helios-inspector",
            "vm",
            "--arch",
            "x86-64",
            "--debug",
            "--cpu",
            "max",
            "--accel",
            "tcg,thread=multi",
            "--qemu-trace",
            "int,exec",
            "--qemu-arg",
            "-no-reboot",
            "repl",
        ])
        .expect("vm debug diagnostics flags must parse");
        match args.command.expect("subcommand must be present") {
            Command::Vm(_) => {}
            _ => panic!("expected vm command"),
        }
    }
}
