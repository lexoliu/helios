//! Shell builtins. Each builtin is a single async fn; `dispatch`
//! returns `Some(status)` when `name` matches a builtin and `None`
//! otherwise.

use std::io::{self, Write as _};

use anyhow::{bail, Context as _, Result};
use helios_api::fs as helios_fs;

use crate::exec::{CommandStatus, Context};

pub async fn dispatch(
    ctx: &mut Context,
    name: &str,
    args: &[String],
) -> Result<Option<CommandStatus>> {
    match name {
        "true" => Ok(Some(CommandStatus::SUCCESS)),
        "false" => Ok(Some(CommandStatus::new(1))),
        "echo" => {
            echo(args)?;
            Ok(Some(CommandStatus::SUCCESS))
        }
        "pwd" => {
            writeln!(io::stdout(), "/")?;
            Ok(Some(CommandStatus::SUCCESS))
        }
        "cd" => Ok(Some(cd(args)?)),
        "test" | "[" => Ok(Some(run_test(name, args).await?)),
        "export" => Ok(Some(export(ctx, args))),
        "unset" => Ok(Some(unset(ctx, args))),
        "exit" => Ok(Some(exit(args)?)),
        "exec" => {
            // `exec <prog>` without redirections in POSIX replaces the
            // current process. For our shell we treat it as "run the
            // program, then exit with its status" which is the nearest
            // sensible behaviour.
            if args.is_empty() {
                return Ok(Some(CommandStatus::SUCCESS));
            }
            // Fall through so the executor launches the program
            // externally; mark as "not a builtin" to reuse the normal
            // launch path.
            let _ = ctx;
            Ok(None)
        }
        ":" => Ok(Some(CommandStatus::SUCCESS)),
        _ => Ok(None),
    }
}

fn echo(args: &[String]) -> Result<()> {
    let mut stdout = io::stdout().lock();
    let mut newline = true;
    let mut iter = args.iter().peekable();
    // Parse -n flag.
    while let Some(arg) = iter.peek() {
        if arg.as_str() == "-n" {
            newline = false;
            iter.next();
        } else {
            break;
        }
    }
    let rest: Vec<&String> = iter.collect();
    for (i, arg) in rest.iter().enumerate() {
        if i > 0 {
            stdout.write_all(b" ")?;
        }
        stdout.write_all(arg.as_bytes())?;
    }
    if newline {
        stdout.write_all(b"\n")?;
    }
    Ok(())
}

fn cd(args: &[String]) -> Result<CommandStatus> {
    // We have a single-directory filesystem in the guest; make `cd /`
    // succeed and anything else fail with a warning.
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
    // Trim the trailing `]` if invoked as `[`.
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
