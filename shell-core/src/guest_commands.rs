use anyhow::{Context as _, Result};

/// Guest-shell echo destination after host-side parsing normalized redirections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EchoTarget {
    Stdout(Vec<u8>),
    File {
        path: String,
        bytes: Vec<u8>,
        append: bool,
    },
}

/// Commands delegated to the guest `/bin/sh` program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestCommand {
    Pwd,
    List(Option<String>),
    Cat(String),
    Test(Vec<String>),
    Mkdir(String),
    Remove(String),
    Copy { source: String, destination: String },
    Move { source: String, destination: String },
    Touch(String),
    Echo(EchoTarget),
    Exec { path: String, args: Vec<String> },
}

pub trait GuestCommandSource {
    fn pwd(&self) -> bool {
        false
    }

    fn list_path(&self) -> Option<Option<&str>> {
        None
    }

    fn cat_path(&self) -> Option<&str> {
        None
    }

    fn test_expression(&self) -> Option<&[String]> {
        None
    }

    fn mkdir_path(&self) -> Option<&str> {
        None
    }

    fn remove_path(&self) -> Option<&str> {
        None
    }

    fn copy_paths(&self) -> Option<(&str, &str)> {
        None
    }

    fn move_paths(&self) -> Option<(&str, &str)> {
        None
    }

    fn touch_path(&self) -> Option<&str> {
        None
    }

    fn echo_target(&self) -> Option<&EchoTarget> {
        None
    }

    fn exec_program(&self) -> Option<(&str, &[String])> {
        None
    }
}

pub fn from_source(source: &impl GuestCommandSource) -> Option<GuestCommand> {
    if source.pwd() {
        return Some(GuestCommand::Pwd);
    }
    if let Some(path) = source.list_path() {
        return Some(GuestCommand::List(path.map(ToOwned::to_owned)));
    }
    if let Some(path) = source.cat_path() {
        return Some(GuestCommand::Cat(path.to_owned()));
    }
    if let Some(expression) = source.test_expression() {
        return Some(GuestCommand::Test(expression.to_vec()));
    }
    if let Some(path) = source.mkdir_path() {
        return Some(GuestCommand::Mkdir(path.to_owned()));
    }
    if let Some(path) = source.remove_path() {
        return Some(GuestCommand::Remove(path.to_owned()));
    }
    if let Some((source_path, destination_path)) = source.copy_paths() {
        return Some(GuestCommand::Copy {
            source: source_path.to_owned(),
            destination: destination_path.to_owned(),
        });
    }
    if let Some((source_path, destination_path)) = source.move_paths() {
        return Some(GuestCommand::Move {
            source: source_path.to_owned(),
            destination: destination_path.to_owned(),
        });
    }
    if let Some(path) = source.touch_path() {
        return Some(GuestCommand::Touch(path.to_owned()));
    }
    if let Some(target) = source.echo_target() {
        return Some(GuestCommand::Echo(target.clone()));
    }
    if let Some((path, args)) = source.exec_program() {
        return Some(GuestCommand::Exec {
            path: path.to_owned(),
            args: args.to_vec(),
        });
    }
    None
}

pub fn render(command: &GuestCommand) -> Result<String> {
    match command {
        GuestCommand::Pwd => Ok("pwd".to_owned()),
        GuestCommand::List(path) => Ok(match path {
            Some(path) => format!("ls {}", shell_words::quote(path)),
            None => "ls".to_owned(),
        }),
        GuestCommand::Cat(path) => Ok(format!("cat {}", shell_words::quote(path))),
        GuestCommand::Test(expression) => Ok(format!(
            "test {}",
            expression
                .iter()
                .map(|part| shell_words::quote(part))
                .collect::<Vec<_>>()
                .join(" ")
        )),
        GuestCommand::Mkdir(path) => Ok(format!("mkdir {}", shell_words::quote(path))),
        GuestCommand::Remove(path) => Ok(format!("rm {}", shell_words::quote(path))),
        GuestCommand::Copy { source, destination } => Ok(format!(
            "cp {} {}",
            shell_words::quote(source),
            shell_words::quote(destination)
        )),
        GuestCommand::Move { source, destination } => Ok(format!(
            "mv {} {}",
            shell_words::quote(source),
            shell_words::quote(destination)
        )),
        GuestCommand::Touch(path) => Ok(format!("touch {}", shell_words::quote(path))),
        GuestCommand::Echo(target) => match target {
            EchoTarget::Stdout(bytes) => Ok(format!(
                "echo {}",
                shell_words::quote(render_echo_text(bytes)?)
            )),
            EchoTarget::File {
                path,
                bytes,
                append,
            } => {
                let operator = if *append { ">>" } else { ">" };
                Ok(format!(
                    "echo {} {} {}",
                    shell_words::quote(render_echo_text(bytes)?),
                    operator,
                    shell_words::quote(path)
                ))
            }
        },
        GuestCommand::Exec { path, args } => {
            let mut rendered = vec!["exec".to_owned(), shell_words::quote(path).to_string()];
            rendered.extend(args.iter().map(|arg| shell_words::quote(arg).to_string()));
            Ok(rendered.join(" "))
        }
    }
}

fn render_echo_text(bytes: &[u8]) -> Result<&str> {
    let text = core::str::from_utf8(bytes).context("echo payload must stay utf-8")?;
    Ok(text.trim_end_matches('\n'))
}

#[cfg(test)]
mod tests {
    use super::{render, EchoTarget, GuestCommand};

    #[test]
    fn renders_exec_with_quoted_args() {
        let rendered = render(&GuestCommand::Exec {
            path: "/bin/ping.wasm".to_owned(),
            args: vec!["google.com".to_owned(), "1500".to_owned()],
        })
        .expect("exec must render");
        assert_eq!(rendered, "exec /bin/ping.wasm google.com 1500");
    }

    #[test]
    fn renders_echo_redirection() {
        let rendered = render(&GuestCommand::Echo(EchoTarget::File {
            path: "/tmp/file".to_owned(),
            bytes: b"hello\n".to_vec(),
            append: false,
        }))
        .expect("echo redirection must render");
        assert_eq!(rendered, "echo hello > /tmp/file");
    }

    #[test]
    fn rejects_non_utf8_echo_payloads() {
        let error = render(&GuestCommand::Echo(EchoTarget::Stdout(vec![0xff])))
            .expect_err("non-utf8 echo payload must fail fast");
        assert!(error.to_string().contains("utf-8"));
    }
}
