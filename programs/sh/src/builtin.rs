use crate::error::{Result, ShellError};
use crate::exec::{CommandStatus, ExecutionIo, Shell};
use crate::platform::ShellPlatform;
use crate::streams::{close, read_all, write_all};

pub async fn dispatch<P: ShellPlatform>(
    shell: &mut Shell<P>,
    name: &str,
    args: &[String],
    mut io: ExecutionIo,
) -> Result<Option<CommandStatus>> {
    let status = match name {
        "true" => Some(CommandStatus::SUCCESS),
        "false" => Some(CommandStatus::new(1)),
        "echo" => Some(echo(args, &mut io).await?),
        "cat" => Some(cat(shell, args, &mut io).await?),
        "pwd" => Some(pwd(shell, &mut io).await?),
        "cd" => Some(cd(shell, args, &mut io).await?),
        "test" | "[" => Some(run_test(shell, name, args).await?),
        "export" => Some(export(shell, args, &mut io).await?),
        "unset" => Some(unset(shell, args)),
        "exit" => Some(exit(shell, args)?),
        ":" => Some(CommandStatus::SUCCESS),
        _ => None,
    };

    if status.is_some() {
        close(&mut io.stdout).await?;
        close(&mut io.stderr).await?;
    }

    Ok(status)
}

pub fn is_builtin(name: &str) -> bool {
    matches!(
        name,
        "true"
            | "false"
            | "echo"
            | "cat"
            | "pwd"
            | "cd"
            | "test"
            | "["
            | "export"
            | "unset"
            | "exit"
            | ":"
    )
}

async fn cat<P: ShellPlatform>(
    shell: &Shell<P>,
    args: &[String],
    io: &mut ExecutionIo,
) -> Result<CommandStatus> {
    if args.is_empty() {
        let bytes = read_all(&mut io.stdin).await?;
        write_all(&mut io.stdout, &bytes).await?;
        return Ok(CommandStatus::SUCCESS);
    }

    for arg in args {
        let path = shell.resolve_user_path(arg);
        let bytes = shell.platform.read_file(&path).await?;
        write_all(&mut io.stdout, &bytes).await?;
    }

    Ok(CommandStatus::SUCCESS)
}

async fn echo(args: &[String], io: &mut ExecutionIo) -> Result<CommandStatus> {
    let mut newline = true;
    let mut start = 0;
    while start < args.len() && args[start] == "-n" {
        newline = false;
        start += 1;
    }

    let mut bytes = Vec::new();
    for (index, arg) in args[start..].iter().enumerate() {
        if index > 0 {
            bytes.push(b' ');
        }
        bytes.extend_from_slice(arg.as_bytes());
    }
    if newline {
        bytes.push(b'\n');
    }

    write_all(&mut io.stdout, &bytes).await?;
    Ok(CommandStatus::SUCCESS)
}

async fn pwd<P: ShellPlatform>(shell: &Shell<P>, io: &mut ExecutionIo) -> Result<CommandStatus> {
    let mut bytes = shell.current_dir().display().to_string().into_bytes();
    bytes.push(b'\n');
    write_all(&mut io.stdout, &bytes).await?;
    Ok(CommandStatus::SUCCESS)
}

async fn cd<P: ShellPlatform>(
    shell: &mut Shell<P>,
    args: &[String],
    io: &mut ExecutionIo,
) -> Result<CommandStatus> {
    let target = match args.first().map(String::as_str) {
        None => shell
            .variable("HOME")
            .map(|variable| variable.value.clone())
            .unwrap_or_else(|| "/".to_owned()),
        Some("-") => shell
            .variable("OLDPWD")
            .map(|variable| variable.value.clone())
            .unwrap_or_else(|| "/".to_owned()),
        Some(path) => path.to_owned(),
    };
    let resolved = shell.resolve_user_path(&target);
    if shell.platform.is_dir(&resolved).await {
        shell.set_working_dir(resolved);
        return Ok(CommandStatus::SUCCESS);
    }

    let message = format!("cd: {}: not a directory\n", resolved.display());
    write_all(&mut io.stderr, message.as_bytes()).await?;
    Ok(CommandStatus::new(1))
}

async fn run_test<P: ShellPlatform>(
    shell: &Shell<P>,
    name: &str,
    raw_args: &[String],
) -> Result<CommandStatus> {
    let args: &[String] = if name == "[" {
        match raw_args.last() {
            Some(last) if last == "]" => &raw_args[..raw_args.len() - 1],
            _ => {
                return Err(ShellError::message("`[` must be terminated by `]`"));
            }
        }
    } else {
        raw_args
    };

    match args {
        [] => Ok(CommandStatus::new(1)),
        [value] => Ok(CommandStatus::new(if value.is_empty() { 1 } else { 0 })),
        [flag, operand] => match flag.as_str() {
            "-e" => Ok(CommandStatus::new(
                (!shell
                    .platform
                    .exists(&shell.resolve_user_path(operand))
                    .await) as u8,
            )),
            "-f" => Ok(CommandStatus::new(
                (!shell
                    .platform
                    .is_file(&shell.resolve_user_path(operand))
                    .await) as u8,
            )),
            "-d" => Ok(CommandStatus::new(
                (!shell
                    .platform
                    .is_dir(&shell.resolve_user_path(operand))
                    .await) as u8,
            )),
            "-n" => Ok(CommandStatus::new(if operand.is_empty() { 1 } else { 0 })),
            "-z" => Ok(CommandStatus::new(if operand.is_empty() { 0 } else { 1 })),
            "!" => Ok(CommandStatus::new(if operand.is_empty() { 0 } else { 1 })),
            _ => Err(ShellError::message(format!(
                "unsupported test flag {flag:?}"
            ))),
        },
        [left, op, right] => match op.as_str() {
            "=" | "==" => Ok(CommandStatus::new(if left == right { 0 } else { 1 })),
            "!=" => Ok(CommandStatus::new(if left != right { 0 } else { 1 })),
            _ => Err(ShellError::message(format!(
                "unsupported test comparison {op:?}"
            ))),
        },
        _ => Err(ShellError::message(format!(
            "unsupported test expression with {} arguments: {:?}",
            args.len(),
            args
        ))),
    }
}

async fn export<P: ShellPlatform>(
    shell: &mut Shell<P>,
    args: &[String],
    io: &mut ExecutionIo,
) -> Result<CommandStatus> {
    if args.is_empty() {
        let mut names = shell
            .variables
            .iter()
            .filter(|(_, variable)| variable.exported)
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        names.sort();
        for name in names {
            let value = shell
                .variable(&name)
                .map(|variable| variable.value.clone())
                .unwrap_or_default();
            let line = format!("export {name}={value}\n");
            write_all(&mut io.stdout, line.as_bytes()).await?;
        }
        return Ok(CommandStatus::SUCCESS);
    }

    for arg in args {
        if let Some((name, value)) = arg.split_once('=') {
            shell.set_variable(name.to_owned(), value.to_owned(), Some(true));
        } else {
            shell.mark_exported(arg);
        }
    }
    Ok(CommandStatus::SUCCESS)
}

fn unset<P: ShellPlatform>(shell: &mut Shell<P>, args: &[String]) -> CommandStatus {
    for name in args {
        shell.unset(name);
    }
    CommandStatus::SUCCESS
}

fn exit<P: ShellPlatform>(shell: &Shell<P>, args: &[String]) -> Result<CommandStatus> {
    let code = match args.first() {
        Some(value) => value.parse::<u8>().map_err(|error| {
            ShellError::message(format!("invalid exit code {value:?}: {error}"))
        })?,
        None => shell.last_status,
    };
    Ok(CommandStatus::exit(code))
}
