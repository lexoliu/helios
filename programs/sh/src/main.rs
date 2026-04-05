use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use helios_api::programs::{self, ExecErrorKind, ExecRequest};
use shell_core::{CommandStatus, ParseState, ScriptHost, Statement};
use wasmparser::Parser;
use wit_component::ComponentEncoder;

const PROGRAM_SEARCH_DIRECTORIES: &[&str] = &["/bin"];

#[helios_api::main]
async fn main() -> Result<()> {
    let invocation = Invocation::from_env()?;
    let program = invocation.load_program()?;
    let mut shell = GuestShell;
    let status = shell_core::execute_script(&mut shell, &program).await?;
    if status.is_success() {
        Ok(())
    } else {
        bail!("shell command exited with status {}", status.code())
    }
}

struct Invocation {
    inline_command: Option<String>,
    script_path: Option<PathBuf>,
}

impl Invocation {
    fn from_env() -> Result<Self> {
        let mut args = env::args().skip(1);
        match args.next().as_deref() {
            None => bail!("interactive helios-sh is not implemented yet; use `sh -c <command>` or `sh <script>`"),
            Some("-c") => {
                let command = args
                    .next()
                    .context("`sh -c` requires a command string")?;
                if args.next().is_some() {
                    bail!("`sh -c` does not accept extra positional arguments yet")
                }
                Ok(Self {
                    inline_command: Some(command),
                    script_path: None,
                })
            }
            Some(path) => {
                if args.next().is_some() {
                    bail!("`sh <script>` does not accept extra positional arguments yet")
                }
                Ok(Self {
                    inline_command: None,
                    script_path: Some(PathBuf::from(path)),
                })
            }
        }
    }

    fn load_program(&self) -> Result<Vec<Statement>> {
        let source = match (&self.inline_command, &self.script_path) {
            (Some(command), None) => command.clone(),
            (None, Some(path)) => {
                fs::read_to_string(path).with_context(|| format!("failed to read shell script {}", path.display()))?
            }
            _ => unreachable!("invocation construction must choose exactly one shell source"),
        };
        match shell_core::parse(&source)? {
            ParseState::Complete(program) => Ok(program),
            ParseState::Incomplete => bail!("shell block is incomplete"),
        }
    }
}

struct GuestShell;

#[async_trait(?Send)]
impl ScriptHost for GuestShell {
    async fn execute_line(&mut self, line: &str) -> Result<CommandStatus> {
        let tokens = shell_words::split(line).with_context(|| format!("failed to parse shell line {line:?}"))?;
        let Some(command) = tokens.first().map(String::as_str) else {
            return Ok(CommandStatus::SUCCESS);
        };

        match command {
            "true" => Ok(CommandStatus::SUCCESS),
            "false" => Ok(CommandStatus::new(1)),
            "echo" => {
                run_echo(&tokens[1..])?;
                Ok(CommandStatus::SUCCESS)
            }
            "pwd" => {
                writeln!(io::stdout(), "/")?;
                Ok(CommandStatus::SUCCESS)
            }
            "cat" => {
                let path = single_argument(command, &tokens[1..])?;
                let bytes = fs::read(path)
                    .with_context(|| format!("failed to read file {path}"))?;
                io::stdout().write_all(&bytes)?;
                Ok(CommandStatus::SUCCESS)
            }
            "test" => run_test(&tokens[1..]),
            "exec" => {
                let path = tokens.get(1).context("`exec` requires a program path")?;
                exec_program(path, &tokens[2..]).await
            }
            "exit" => {
                let code = match tokens.get(1) {
                    Some(value) => value.parse::<u8>().with_context(|| format!("invalid exit code {value:?}"))?,
                    None => 0,
                };
                Ok(CommandStatus::exiting(code))
            }
            _ => exec_program(command, &tokens[1..]).await,
        }
    }
}

fn run_echo(words: &[String]) -> Result<()> {
    let mut stdout = io::stdout().lock();
    if !words.is_empty() {
        stdout.write_all(words.join(" ").as_bytes())?;
    }
    stdout.write_all(b"\n")?;
    Ok(())
}

fn run_test(args: &[String]) -> Result<CommandStatus> {
    match args {
        [flag, path] if flag == "-e" => Ok(CommandStatus::new((!Path::new(path).exists()) as u8)),
        [flag, path] if flag == "-f" => Ok(CommandStatus::new((!Path::new(path).is_file()) as u8)),
        [flag, path] if flag == "-d" => Ok(CommandStatus::new((!Path::new(path).is_dir()) as u8)),
        _ => bail!("unsupported test expression: expected `test -e|-f|-d <path>`"),
    }
}

async fn exec_program(program: &str, args: &[String]) -> Result<CommandStatus> {
    let resolved = resolve_program(program)?;
    let wasm = load_component(&resolved)?;
    let name = infer_instance_name(&resolved)?;
    match programs::exec(&ExecRequest {
        name,
        args: args.to_vec(),
        wasm,
    }) {
        Ok(_) => Ok(CommandStatus::SUCCESS),
        Err(error) if error.kind == ExecErrorKind::Unavailable => {
            bail!("program exec is unavailable: {}", error.detail)
        }
        Err(error) => bail!("failed to exec {program:?}: {:?}: {}", error.kind, error.detail),
    }
}

fn resolve_program(input: &str) -> Result<PathBuf> {
    if input.contains('/') || input.starts_with('.') {
        return resolve_explicit_path(input);
    }

    for directory in PROGRAM_SEARCH_DIRECTORIES {
        let direct = PathBuf::from(directory).join(input);
        if direct.exists() {
            return Ok(direct);
        }
        let wasm = direct.with_extension("wasm");
        if wasm.exists() {
            return Ok(wasm);
        }
    }

    bail!("failed to locate executable program {input:?}")
}

fn resolve_explicit_path(input: &str) -> Result<PathBuf> {
    let path = PathBuf::from(input);
    if path.exists() {
        return Ok(path);
    }
    let wasm = path.with_extension("wasm");
    if wasm.exists() {
        return Ok(wasm);
    }
    bail!("failed to locate executable path {input:?}")
}

fn load_component(path: &Path) -> Result<Vec<u8>> {
    let wasm = fs::read(path).with_context(|| format!("failed to read wasm file {}", path.display()))?;
    if Parser::is_component(&wasm) {
        return Ok(wasm);
    }

    ComponentEncoder::default()
        .module(&wasm)
        .with_context(|| format!("failed to load core wasm module {}", path.display()))?
        .validate(true)
        .encode()
        .with_context(|| format!("failed to encode component {}", path.display()))
}

fn infer_instance_name(path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("program path does not end with a valid utf-8 file name")?;
    Ok(name.strip_suffix(".wasm").unwrap_or(name).to_owned())
}

fn single_argument<'a>(command: &str, args: &'a [String]) -> Result<&'a str> {
    match args {
        [path] => Ok(path),
        _ => bail!("`{command}` requires exactly one path argument"),
    }
}
