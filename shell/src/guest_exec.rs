use anyhow::{Context as _, Result};
use shell_core::guest_commands::{EchoTarget, GuestCommand};

use crate::filesystem;
use crate::programs;
use crate::serial::RpcClient;

#[derive(Debug, Default)]
pub struct GuestOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: u8,
}

pub async fn run(client: &mut RpcClient, command: &GuestCommand) -> Result<GuestOutput> {
    match command {
        GuestCommand::Pwd => Ok(GuestOutput {
            stdout: format!("{}
", filesystem::pwd()).into_bytes(),
            ..GuestOutput::default()
        }),
        GuestCommand::List(path) => Ok(GuestOutput {
            stdout: filesystem::list(client, path.as_deref()).await?.into_bytes(),
            ..GuestOutput::default()
        }),
        GuestCommand::Cat(path) => Ok(GuestOutput {
            stdout: filesystem::cat(client, path).await?,
            ..GuestOutput::default()
        }),
        GuestCommand::Mkdir(path) => {
            filesystem::mkdir(client, path).await?;
            Ok(GuestOutput::default())
        }
        GuestCommand::Remove(path) => {
            filesystem::remove(client, path).await?;
            Ok(GuestOutput::default())
        }
        GuestCommand::Copy { source, destination } => {
            filesystem::copy(client, source, destination).await?;
            Ok(GuestOutput::default())
        }
        GuestCommand::Move { source, destination } => {
            filesystem::move_path(client, source, destination).await?;
            Ok(GuestOutput::default())
        }
        GuestCommand::Touch(path) => {
            filesystem::touch(client, path).await?;
            Ok(GuestOutput::default())
        }
        GuestCommand::Echo(target) => run_echo(client, target).await,
        GuestCommand::Exec { path, args } => run_program(client, path, args).await,
        GuestCommand::Test(_) => {
            let line = shell_core::guest_commands::render(command)?;
            run_program(client, "/bin/sh", &["-c".to_owned(), line]).await
        }
    }
}

async fn run_echo(client: &mut RpcClient, target: &EchoTarget) -> Result<GuestOutput> {
    match target {
        EchoTarget::Stdout(bytes) => Ok(GuestOutput {
            stdout: bytes.clone(),
            ..GuestOutput::default()
        }),
        EchoTarget::File {
            path,
            bytes,
            append,
        } => {
            filesystem::write(client, path, bytes, *append).await?;
            Ok(GuestOutput::default())
        }
    }
}

async fn run_program(client: &mut RpcClient, path: &str, args: &[String]) -> Result<GuestOutput> {
    let result = programs::exec(client, path, args).await?;
    Ok(GuestOutput {
        stdout: result.output.stdout,
        stderr: result.output.stderr,
        exit_code: u8::try_from(result.exit_code)
            .with_context(|| format!("guest exit code {} exceeded u8", result.exit_code))?,
    })
}
