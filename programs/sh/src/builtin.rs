//! Shell builtins. Each builtin is a single async fn; `dispatch`
//! returns `Some((status, stdout_bytes))` when `name` matches a builtin
//! and `None` otherwise. Builtins never write to the real stdout — that
//! is the executor's job once it has applied the command's redirects.

use std::io::{self, Write as _};

use anyhow::{bail, Context as _, Result};
use helios_api::fs as helios_fs;

use crate::exec::{CommandStatus, Context};

pub struct BuiltinOutput {
    pub status: CommandStatus,
    pub stdout: Vec<u8>,
}

pub async fn dispatch(
    ctx: &mut Context,
    name: &str,
    args: &[String],
) -> Result<Option<BuiltinOutput>> {
    Ok(Some(match name {
        "true" => BuiltinOutput::status_only(CommandStatus::SUCCESS),
        "false" => BuiltinOutput::status_only(CommandStatus::new(1)),
        "echo" => echo(args),
        "cat" => cat(args).await?,
        "pwd" => BuiltinOutput::with_stdout(CommandStatus::SUCCESS, b"/\n".to_vec()),
        "cd" => BuiltinOutput::status_only(cd(args)?),
        "test" | "[" => BuiltinOutput::status_only(run_test(name, args).await?),
        "export" => BuiltinOutput::status_only(export(ctx, args)),
        "unset" => BuiltinOutput::status_only(unset(ctx, args)),
        "exit" => BuiltinOutput::status_only(exit(args)?),
        ":" => BuiltinOutput::status_only(CommandStatus::SUCCESS),
        _ => return Ok(None),
    }))
}

impl BuiltinOutput {
    fn status_only(status: CommandStatus) -> Self {
        Self {
            status,
            stdout: Vec::new(),
        }
    }

    fn with_stdout(status: CommandStatus, stdout: Vec<u8>) -> Self {
        Self { status, stdout }
    }
}

async fn cat(args: &[String]) -> Result<BuiltinOutput> {
    let mut buf = Vec::new();
    for arg in args {
        let bytes = helios_fs::read(arg)
            .await
            .map_err(|e| anyhow::anyhow!("cat: {arg}: {e:#}"))?;
        buf.extend_from_slice(&bytes);
    }
    Ok(BuiltinOutput::with_stdout(CommandStatus::SUCCESS, buf))
}

fn echo(args: &[String]) -> BuiltinOutput {
    let mut newline = true;
    let mut start = 0;
    // Parse `-n` flags at the front.
    while start < args.len() && args[start] == "-n" {
        newline = false;
        start += 1;
    }
    let mut buf = Vec::with_capacity(args.iter().skip(start).map(|s| s.len() + 1).sum());
    for (i, arg) in args[start..].iter().enumerate() {
        if i > 0 {
            buf.push(b' ');
        }
        buf.extend_from_slice(arg.as_bytes());
    }
    if newline {
        buf.push(b'\n');
    }
    BuiltinOutput::with_stdout(CommandStatus::SUCCESS, buf)
}

fn cd(args: &[String]) -> Result<CommandStatus> {
    match args.first().map(String::as_str) {
        None | Some("/") => Ok(CommandStatus::SUCCESS),
        Some(path) => {
            let _ = writeln!(
                io::stderr(),
                "cd: only `cd /` is supported in this shell (requested {path:?})"
            );
            Ok(CommandStatus::new(1))
        }
    }
}

async fn run_test(name: &str, raw_args: &[String]) -> Result<CommandStatus> {
    let args: &[String] = if name == "[" {
        match raw_args.last() {
            Some(last) if last == "]" => &raw_args[..raw_args.len() - 1],
            _ => bail!("`[` must be terminated by `]`"),
        }
    } else {
        raw_args
    };
    match args {
        [] => Ok(CommandStatus::new(1)),
        [value] => Ok(CommandStatus::new(if value.is_empty() { 1 } else { 0 })),
        [flag, operand] => match flag.as_str() {
            "-e" => Ok(CommandStatus::new((!helios_fs::exists(operand).await) as u8)),
            "-f" => Ok(CommandStatus::new((!helios_fs::is_file(operand).await) as u8)),
            "-d" => Ok(CommandStatus::new((!helios_fs::is_dir(operand).await) as u8)),
            "-n" => Ok(CommandStatus::new(if operand.is_empty() { 1 } else { 0 })),
            "-z" => Ok(CommandStatus::new(if operand.is_empty() { 0 } else { 1 })),
            "!" => Ok(CommandStatus::new(if operand.is_empty() { 0 } else { 1 })),
            _ => bail!("unsupported test flag {flag:?}"),
        },
        [left, op, right] => match op.as_str() {
            "=" | "==" => Ok(CommandStatus::new(if left == right { 0 } else { 1 })),
            "!=" => Ok(CommandStatus::new(if left != right { 0 } else { 1 })),
            _ => bail!("unsupported test comparison {op:?}"),
        },
        _ => bail!(
            "unsupported test expression with {} arguments: {:?}",
            args.len(),
            args
        ),
    }
}

fn export(ctx: &mut Context, args: &[String]) -> CommandStatus {
    for arg in args {
        if let Some((name, value)) = arg.split_once('=') {
            ctx.variables.insert(name.to_owned(), value.to_owned());
        }
    }
    CommandStatus::SUCCESS
}

fn unset(ctx: &mut Context, args: &[String]) -> CommandStatus {
    for name in args {
        ctx.variables.remove(name);
    }
    CommandStatus::SUCCESS
}

fn exit(args: &[String]) -> Result<CommandStatus> {
    let code = match args.first() {
        Some(value) => value
            .parse::<u8>()
            .with_context(|| format!("invalid exit code {value:?}"))?,
        None => 0,
    };
    Ok(CommandStatus::exit(code))
}
