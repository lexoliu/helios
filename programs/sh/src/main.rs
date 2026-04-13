mod script;

use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use helios_api::fs as helios_fs;
use helios_api::programs::{self, ExecErrorKind, ExecRequest};
use script::{CommandStatus, ParseState, ScriptHost, Statement};

const PROGRAM_SEARCH_DIRECTORIES: &[&str] = &["/bin"];

#[helios_api::main]
async fn main() -> Result<()> {
    let invocation = Invocation::from_env()?;
    let program = invocation.load_program().await?;
    let mut shell = GuestShell;
    let status = script::execute_script(&mut shell, &program).await?;
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
            None => {
                bail!("interactive dash is unsupported; use `dash -c <command>` or `dash <script>`")
            }
            Some("-c") => {
                let command = args.next().context("`dash -c` requires a command string")?;
                if args.next().is_some() {
                    bail!("`dash -c` does not accept extra positional arguments")
                }
                Ok(Self {
                    inline_command: Some(command),
                    script_path: None,
                })
            }
            Some(path) => {
                if args.next().is_some() {
                    bail!("`dash <script>` does not accept extra positional arguments")
                }
                Ok(Self {
                    inline_command: None,
                    script_path: Some(PathBuf::from(path)),
                })
            }
        }
    }

    async fn load_program(&self) -> Result<Vec<Statement>> {
        let source = match (&self.inline_command, &self.script_path) {
            (Some(command), None) => command.clone(),
            (None, Some(path)) => helios_fs::read_to_string(path).await.map_err(|error| {
                anyhow::anyhow!("failed to read shell script {}: {error:#}", path.display())
            })?,
            _ => unreachable!("invocation construction must choose exactly one shell source"),
        };
        match script::parse(&source)? {
            ParseState::Complete(program) => Ok(program),
            ParseState::Incomplete => bail!("shell block is incomplete"),
        }
    }
}

struct GuestShell;

#[async_trait(?Send)]
impl ScriptHost for GuestShell {
    async fn execute_line(&mut self, line: &str) -> Result<CommandStatus> {
        let tokens = shell_words::split(line)
            .with_context(|| format!("failed to parse shell line {line:?}"))?;
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
                let bytes = helios_fs::read(path)
                    .await
                    .map_err(|error| anyhow::anyhow!("failed to read file {path}: {error:#}"))?;
                io::stdout().write_all(&bytes)?;
                Ok(CommandStatus::SUCCESS)
            }
            "test" => run_test(&tokens[1..]).await,
            "exec" => {
                let path = tokens.get(1).context("`exec` requires a program path")?;
                exec_program(path, &tokens[2..]).await
            }
            "exit" => {
                let code = match tokens.get(1) {
                    Some(value) => value
                        .parse::<u8>()
                        .with_context(|| format!("invalid exit code {value:?}"))?,
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

async fn run_test(args: &[String]) -> Result<CommandStatus> {
    match args {
        [flag, path] if flag == "-e" => {
            Ok(CommandStatus::new((!helios_fs::exists(path).await) as u8))
        }
        [flag, path] if flag == "-f" => {
            Ok(CommandStatus::new((!helios_fs::is_file(path).await) as u8))
        }
        [flag, path] if flag == "-d" => {
            Ok(CommandStatus::new((!helios_fs::is_dir(path).await) as u8))
        }
        _ => bail!("unsupported test expression: expected `test -e|-f|-d <path>`"),
    }
}

async fn exec_program(program: &str, args: &[String]) -> Result<CommandStatus> {
    let resolved = resolve_program(program).await?;
    let name = infer_program_name(Path::new(&resolved.path))?;
    match programs::exec(ExecRequest {
        name,
        args: args.to_vec(),
        wasm: resolved.wasm,
    })
    .await
    {
        Ok(result) => {
            io::stdout().write_all(&result.output.stdout)?;
            io::stderr().write_all(&result.output.stderr)?;
            let code = u8::try_from(result.exit_code)
                .with_context(|| format!("guest exit code {} exceeded u8", result.exit_code))?;
            Ok(CommandStatus::new(code))
        }
        Err(error) if error.kind == ExecErrorKind::Unavailable => {
            bail!("program exec is unavailable: {}", error.detail)
        }
        Err(error) => bail!(
            "failed to exec {program:?}: {:?}: {}",
            error.kind,
            error.detail
        ),
    }
}

async fn resolve_program(input: &str) -> Result<ResolvedProgram> {
    let mut errors = Vec::new();

    for path in candidate_paths(input)? {
        match helios_fs::read(&path).await {
            Ok(wasm) => return Ok(ResolvedProgram { path, wasm }),
            Err(error) => errors.push(format!("{path}: {error:#}")),
        }
    }

    bail!(
        "failed to locate executable program {input:?}:\n{}",
        errors.join("\n")
    )
}

struct ResolvedProgram {
    path: String,
    wasm: Vec<u8>,
}

fn candidate_paths(input: &str) -> Result<Vec<String>> {
    if input.contains('/') || input.starts_with('.') {
        return explicit_candidate_paths(input);
    }

    let mut candidates = Vec::with_capacity(PROGRAM_SEARCH_DIRECTORIES.len() * 2);
    for directory in PROGRAM_SEARCH_DIRECTORIES {
        candidates.push(format!("{directory}/{input}"));
        if !input.ends_with(".wasm") {
            candidates.push(format!("{directory}/{input}.wasm"));
        }
    }
    Ok(candidates)
}

fn explicit_candidate_paths(input: &str) -> Result<Vec<String>> {
    let path = input.to_owned();
    if path.ends_with(".wasm") {
        return Ok(vec![path]);
    }

    Ok(vec![path.clone(), format!("{path}.wasm")])
}

fn infer_program_name(path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("program path does not end with a valid utf-8 file name")?;
    if name.is_empty() {
        bail!("program path does not name an executable")
    }
    Ok(name.strip_suffix(".wasm").unwrap_or(name).to_owned())
}

fn single_argument<'a>(command: &str, args: &'a [String]) -> Result<&'a str> {
    match args {
        [path] => Ok(path),
        _ => bail!("`{command}` requires exactly one path argument"),
    }
}
