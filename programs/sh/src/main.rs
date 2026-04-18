//! Helios guest shell.
//!
//! This crate keeps shell semantics in a platform-agnostic interpreter layer
//! and confines Helios/WASI specifics to a dedicated adapter.

mod builtin;
mod error;
mod exec;
mod parser;
mod platform;
mod streams;

use std::env;
use std::path::PathBuf;

use error::{Result, ShellError};
use exec::{Bootstrap, Shell};
use helios_api::ReadExt as _;
use platform::HeliosPlatform;

#[helios_api::main]
async fn main() -> Result<()> {
    let invocation = Invocation::from_env()?;
    let source = invocation.load_source().await?;
    let program = parser::parse(&source)?;
    let mut shell = Shell::new(HeliosPlatform, invocation.bootstrap());
    let status = shell.run_program(&program).await?;
    std::process::exit(i32::from(status.code()));
}

enum Source {
    Inline(String),
    Script(PathBuf),
    Stdin,
}

struct Invocation {
    source: Source,
    shell_name: String,
    positional_parameters: Vec<String>,
}

impl Invocation {
    fn from_env() -> Result<Self> {
        let mut args = env::args().skip(1);
        match args.next().as_deref() {
            None => Ok(Self {
                source: Source::Stdin,
                shell_name: "dash".to_owned(),
                positional_parameters: Vec::new(),
            }),
            Some("-c") => {
                let command = args
                    .next()
                    .ok_or_else(|| ShellError::message("`dash -c` requires a command string"))?;
                let shell_name = args.next().unwrap_or_else(|| "dash".to_owned());
                Ok(Self {
                    source: Source::Inline(command),
                    shell_name,
                    positional_parameters: args.collect(),
                })
            }
            Some(path) => Ok(Self {
                source: Source::Script(PathBuf::from(path)),
                shell_name: path.to_owned(),
                positional_parameters: args.collect(),
            }),
        }
    }

    async fn load_source(&self) -> Result<String> {
        match &self.source {
            Source::Inline(command) => Ok(command.clone()),
            Source::Script(path) => helios_api::fs::read_to_string(path).await.map_err(|error| {
                ShellError::message(format!(
                    "failed to read shell script {}: {error:#}",
                    path.display()
                ))
            }),
            Source::Stdin => {
                let mut stdin = helios_api::io::stdin();
                let mut bytes = Vec::new();
                stdin.read_to_end(&mut bytes).await.map_err(|error| {
                    ShellError::message(format!("failed to read stdin: {error}"))
                })?;
                String::from_utf8(bytes).map_err(|error| {
                    ShellError::message(format!("stdin is not valid utf-8: {error}"))
                })
            }
        }
    }

    fn bootstrap(&self) -> Bootstrap {
        Bootstrap {
            shell_name: self.shell_name.clone(),
            positional_parameters: self.positional_parameters.clone(),
            working_dir: PathBuf::from("/"),
            environment: env::vars().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Invocation;

    #[test]
    fn parse_dash_c_arguments_like_posix_shell() {
        let invocation =
            Invocation::from_env_with(["dash", "-c", "echo hi", "name", "arg1", "arg2"]);
        assert_eq!(invocation.shell_name, "name");
        assert_eq!(invocation.positional_parameters, vec!["arg1", "arg2"]);
    }
}

impl Invocation {
    #[cfg(test)]
    fn from_env_with(args: impl IntoIterator<Item = &'static str>) -> Self {
        let mut args = args.into_iter().skip(1);
        match args.next().as_deref() {
            None => Self {
                source: Source::Stdin,
                shell_name: "dash".to_owned(),
                positional_parameters: Vec::new(),
            },
            Some("-c") => {
                let command = args.next().expect("command is required");
                let shell_name = args.next().unwrap_or("dash").to_owned();
                Self {
                    source: Source::Inline(command.to_owned()),
                    shell_name,
                    positional_parameters: args.map(str::to_owned).collect(),
                }
            }
            Some(path) => Self {
                source: Source::Script(PathBuf::from(path)),
                shell_name: path.to_owned(),
                positional_parameters: args.map(str::to_owned).collect(),
            },
        }
    }
}
