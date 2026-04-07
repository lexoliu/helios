mod edit_tui;
mod filesystem;
mod guest_exec;
mod help;
mod programs;
mod ready;
mod remote;
mod repl;
mod rpc;
mod runtime;
mod serial;
mod stats_tui;
mod system;
mod tui;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use shell_core::guest_commands::GuestCommand;
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
    /// Copy a remote file inside the debugger filesystem.
    Cp { source: String, destination: String },
    /// Move or rename a remote path inside the debugger filesystem.
    Mv { source: String, destination: String },
    /// Create a file if it does not exist in the remote debugger filesystem.
    Touch { path: String },
    /// Print text or write it into a remote file with `>` or `>>`.
    Echo {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        words: Vec<String>,
    },
    /// Exec a wasm guest program from the remote filesystem with the default minimal rights set.
    Exec {
        path: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Open the live stats view.
    Stats {
        #[arg(long, default_value_t = 1_000)]
        period_ms: u64,
    },
    /// Stream tracing events until interrupted.
    Tracing {
        /// Maximum number of recent events kept in the incremental polling window.
        #[arg(long, default_value_t = 64)]
        limit: u32,
        #[arg(long)]
        min_level: Option<String>,
        #[arg(long)]
        target_prefix: Vec<String>,
    },
    /// Invoke a raw remote WIT method.
    Rpc {
        #[arg(long)]
        instance: String,
        #[arg(long)]
        func: String,
        #[arg(long, default_value = "")]
        request_hex: String,
    },
}

impl Command {
    fn as_guest_command(&self) -> Result<Option<GuestCommand>> {
        Ok(match self {
            Self::Pwd => Some(GuestCommand::Pwd),
            Self::Ls { path } => Some(GuestCommand::List(path.clone())),
            Self::Cat { path } => Some(GuestCommand::Cat(path.clone())),
            Self::Mkdir { path } => Some(GuestCommand::Mkdir(path.clone())),
            Self::Rm { path } => Some(GuestCommand::Remove(path.clone())),
            Self::Cp { source, destination } => Some(GuestCommand::Copy {
                source: source.clone(),
                destination: destination.clone(),
            }),
            Self::Mv { source, destination } => Some(GuestCommand::Move {
                source: source.clone(),
                destination: destination.clone(),
            }),
            Self::Touch { path } => Some(GuestCommand::Touch(path.clone())),
            Self::Echo { words } => Some(GuestCommand::Echo(filesystem::parse_echo(words)?)),
            Self::Exec { path, args } => Some(GuestCommand::Exec {
                path: path.clone(),
                args: args.clone(),
            }),
            Self::Edit { .. } | Self::Stats { .. } | Self::Tracing { .. } | Self::Rpc { .. } => None,
        })
    }
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
        Some(command) => {
            if let Some(command) = command.as_guest_command()? {
                return run_interruptible(async move {
                    let mut client = client;
                    let output = guest_exec::run(&mut client, &command).await?;
                    std::io::stdout().write_all(&output.stdout)?;
                    std::io::stderr().write_all(&output.stderr)?;
                    if output.exit_code != 0 {
                        anyhow::bail!("guest command exited with code {}", output.exit_code);
                    }
                    Ok(())
                });
            }

            match command {
                Command::Edit { path } => run_interruptible(async move {
                    let mut client = client;
                    edit_tui::run(&mut client, &path).await
                }),
                Command::Stats { period_ms } => run_interruptible(async move {
                    let mut client = client;
                    stats_tui::run(&mut client, period_ms).await
                }),
                Command::Tracing {
                    limit,
                    min_level,
                    target_prefix,
                } => run_interruptible(system::run_tracing(
                    client,
                    limit,
                    min_level.as_deref(),
                    target_prefix,
                )),
                Command::Rpc {
                    instance,
                    func,
                    request_hex,
                } => run_interruptible(rpc::run_call(client, &instance, &func, &request_hex)),
                Command::Pwd
                | Command::Ls { .. }
                | Command::Cat { .. }
                | Command::Mkdir { .. }
                | Command::Rm { .. }
                | Command::Cp { .. }
                | Command::Mv { .. }
                | Command::Touch { .. }
                | Command::Echo { .. }
                | Command::Exec { .. } => unreachable!(),
            }
        }
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
