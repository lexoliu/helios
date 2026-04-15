//! Helios guest shell.
//!
//! Invoked as `dash -c "<command>"` or `dash <script-path>`.
//! Parses POSIX/bash-ish shell scripts with `brush-parser` and executes
//! them against `helios-api`'s program spawn + filesystem syscalls.

mod builtin;
mod exec;
mod parser;

use std::env;
use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};
use helios_api::fs as helios_fs;

#[helios_api::main]
async fn main() -> Result<()> {
    let invocation = Invocation::from_env()?;
    let source = invocation.load_source().await?;
    let program = parser::parse(&source)?;
    let mut ctx = exec::Context::new();
    let status = exec::run_program(&mut ctx, &program).await?;
    if status.is_success() {
        Ok(())
    } else {
        bail!("shell command exited with status {}", status.code())
    }
}

struct Invocation {
    inline: Option<String>,
    script: Option<PathBuf>,
}

impl Invocation {
    fn from_env() -> Result<Self> {
        let mut args = env::args().skip(1);
        match args.next().as_deref() {
            None => bail!("interactive dash is unsupported; use `dash -c <command>` or `dash <script>`"),
            Some("-c") => {
                let command = args.next().context("`dash -c` requires a command string")?;
                if args.next().is_some() {
                    bail!("`dash -c` does not accept extra positional arguments")
                }
                Ok(Self {
                    inline: Some(command),
                    script: None,
                })
            }
            Some(path) => {
                if args.next().is_some() {
                    bail!("`dash <script>` does not accept extra positional arguments")
                }
                Ok(Self {
                    inline: None,
                    script: Some(PathBuf::from(path)),
                })
            }
        }
    }

    async fn load_source(&self) -> Result<String> {
        match (&self.inline, &self.script) {
            (Some(command), None) => Ok(command.clone()),
            (None, Some(path)) => helios_fs::read_to_string(path)
                .await
                .map_err(|error| anyhow::anyhow!("failed to read shell script {}: {error:#}", path.display())),
            _ => unreachable!("invocation construction must choose exactly one shell source"),
        }
    }
}
