//! Shell AST executor.
//!
//! Walks brush-parser's [`Program`] AST and dispatches each simple
//! command to either a built-in (see [`crate::builtin`]) or an external
//! wasm program launched via `helios_api::programs::spawn`.
//!
//! We support the core POSIX surface needed to run non-interactive
//! scripts: pipelines, `&&`/`||`, `;`, redirection (`<`, `>`, `>>`),
//! variable assignment on simple commands, and `if`/`then`/`else`/`fi`
//! compound commands. Features we intentionally reject for now (and
//! report as unsupported rather than silently ignore) include
//! functions, case statements, loops, arithmetic, and command
//! substitution — every one of those can be lit up later as dash
//! matures, without touching the spawn syscall layer.

use std::collections::HashMap;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};
use brush_parser::ast::{
    AndOr, Command, CommandPrefixOrSuffixItem, CompleteCommand, CompoundListItem, IoFileRedirectKind,
    IoFileRedirectTarget, IoRedirect, Pipeline, PipelineOperator, Program, SimpleCommand, Word,
};
use helios_api::fs as helios_fs;
use helios_api::programs::{self, SpawnRequest};

use crate::builtin;

const PROGRAM_SEARCH_DIRECTORIES: &[&str] = &["/bin"];

/// Shell evaluation context: variables, exit status, etc. Held across
/// statements so assignments and `$?` are visible to later commands.
pub struct Context {
    pub variables: HashMap<String, String>,
    pub last_status: u8,
    pub exiting: bool,
}

impl Context {
    pub fn new() -> Self {
        Self {
            variables: HashMap::new(),
            last_status: 0,
            exiting: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandStatus {
    code: u8,
    exiting: bool,
}

impl CommandStatus {
    pub const SUCCESS: Self = Self {
        code: 0,
        exiting: false,
    };

    pub const fn new(code: u8) -> Self {
        Self {
            code,
            exiting: false,
        }
    }

    pub const fn exit(code: u8) -> Self {
        Self {
            code,
            exiting: true,
        }
    }

    pub const fn code(self) -> u8 {
        self.code
    }

    pub const fn is_success(self) -> bool {
        self.code == 0
    }

    pub const fn is_exit(self) -> bool {
        self.exiting
    }
}

/// Bytes destined for a simple command's stdout.
pub enum OutputTarget {
    /// Inherit the shell's own stdout (normal case).
    Inherit,
    /// Redirect to a filesystem path using the given redirect kind.
    File { path: String, append: bool },
    /// Consume the bytes into an in-memory buffer (used when piping to
    /// the next stage of a pipeline).
    Buffer(Vec<u8>),
}

/// Bytes consumed from a simple command's stdin.
pub enum InputSource {
    /// No stdin — the child sees EOF immediately.
    Empty,
    /// Read stdin from the given file.
    File(String),
    /// Use the supplied byte slice as stdin.
    Bytes(Vec<u8>),
}

/// Run every top-level compound list item in the parsed program.
pub async fn run_program(ctx: &mut Context, program: &Program) -> Result<CommandStatus> {
    let mut last = CommandStatus::SUCCESS;
    for complete in &program.complete_commands {
        last = run_complete_command(ctx, complete).await?;
        if last.is_exit() {
            ctx.exiting = true;
            return Ok(last);
        }
    }
    Ok(last)
}

async fn run_complete_command(
    ctx: &mut Context,
    command: &CompleteCommand,
) -> Result<CommandStatus> {
    let mut last = CommandStatus::SUCCESS;
    for CompoundListItem(and_or, _separator) in &command.0 {
        last = run_and_or(ctx, and_or).await?;
        if last.is_exit() {
            return Ok(last);
        }
    }
    Ok(last)
}

async fn run_and_or(
    ctx: &mut Context,
    and_or: &brush_parser::ast::AndOrList,
) -> Result<CommandStatus> {
    let mut last = run_pipeline(ctx, &and_or.first).await?;
    if last.is_exit() {
        return Ok(last);
    }
    for link in &and_or.additional {
        let (op, pipeline) = match link {
            AndOr::And(p) => (PipelineOperator::And, p),
            AndOr::Or(p) => (PipelineOperator::Or, p),
        };
        let should_run = match op {
            PipelineOperator::And => last.is_success(),
            PipelineOperator::Or => !last.is_success(),
        };
        if !should_run {
            continue;
        }
        last = run_pipeline(ctx, pipeline).await?;
        if last.is_exit() {
            return Ok(last);
        }
    }
    Ok(last)
}

async fn run_pipeline(ctx: &mut Context, pipeline: &Pipeline) -> Result<CommandStatus> {
    if pipeline.timed.is_some() {
        bail!("pipeline timing is not yet supported");
    }
    // Single-command pipeline: trivial dispatch.
    if pipeline.seq.len() == 1 {
        let mut output = OutputTarget::Inherit;
        let status = run_command(ctx, &pipeline.seq[0], InputSource::Empty, &mut output).await?;
        return Ok(if pipeline.bang {
            CommandStatus::new(if status.is_success() { 1 } else { 0 })
        } else {
            status
        });
    }
    // Real pipe stages: feed stage N-1's captured stdout into stage N's
    // stdin buffer. Middle stages capture to a fresh buffer; final stage
    // inherits the shell's own stdout.
    let mut carry: Vec<u8> = Vec::new();
    let last_idx = pipeline.seq.len() - 1;
    let mut final_status = CommandStatus::SUCCESS;
    for (idx, command) in pipeline.seq.iter().enumerate() {
        let input = if idx == 0 {
            InputSource::Empty
        } else {
            InputSource::Bytes(std::mem::take(&mut carry))
        };
        let output = if idx == last_idx {
            OutputTarget::Inherit
        } else {
            OutputTarget::Buffer(Vec::new())
        };
        let (status, produced) = run_command_capture(ctx, command, input, output).await?;
        final_status = status;
        carry = produced;
    }
    Ok(if pipeline.bang {
        CommandStatus::new(if final_status.is_success() { 1 } else { 0 })
    } else {
        final_status
    })
}

async fn run_command_capture(
    ctx: &mut Context,
    command: &Command,
    input: InputSource,
    mut output: OutputTarget,
) -> Result<(CommandStatus, Vec<u8>)> {
    let status = run_command(ctx, command, input, &mut output).await?;
    let bytes = match output {
        OutputTarget::Buffer(bytes) => bytes,
        _ => Vec::new(),
    };
    Ok((status, bytes))
}

async fn run_command(
    ctx: &mut Context,
    command: &Command,
    input: InputSource,
    output: &mut OutputTarget,
) -> Result<CommandStatus> {
    match command {
        Command::Simple(simple) => run_simple(ctx, simple, input, output).await,
        Command::Compound(_, _) => {
            bail!("compound commands (if/while/for/case/{{...}}) are not yet supported")
        }
        Command::Function(_) => bail!("function definitions are not yet supported"),
        Command::ExtendedTest(_) => bail!("[[ ... ]] extended test is not yet supported"),
    }
}

async fn run_simple(
    ctx: &mut Context,
    command: &SimpleCommand,
    input: InputSource,
    output: &mut OutputTarget,
) -> Result<CommandStatus> {
    // Start with the shell's inherited environment so child processes
    // see whatever `export` / prior `NAME=value` assignments set.
    let mut assignments: Vec<(String, String)> =
        ctx.variables.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let mut args = Vec::new();
    let mut redirects = Vec::new();

    let mut override_assignment = |assignments: &mut Vec<(String, String)>, name: String, value: String| {
        if let Some(existing) = assignments.iter_mut().find(|(k, _)| k == &name) {
            existing.1 = value;
        } else {
            assignments.push((name, value));
        }
    };

    if let Some(prefix) = &command.prefix {
        for item in &prefix.0 {
            match item {
                CommandPrefixOrSuffixItem::AssignmentWord(assign, _) => {
                    override_assignment(
                        &mut assignments,
                        assign.name.to_string(),
                        expand_assignment_value(&assign.value, ctx)?,
                    );
                }
                CommandPrefixOrSuffixItem::Word(word) => {
                    args.push(expand_word(word, ctx)?);
                }
                CommandPrefixOrSuffixItem::IoRedirect(redir) => redirects.push(redir),
                CommandPrefixOrSuffixItem::ProcessSubstitution(_, _) => {
                    bail!("process substitution is not yet supported")
                }
            }
        }
    }

    if let Some(word) = &command.word_or_name {
        args.push(expand_word(word, ctx)?);
    }

    if let Some(suffix) = &command.suffix {
        for item in &suffix.0 {
            match item {
                CommandPrefixOrSuffixItem::AssignmentWord(_, word) => {
                    // In POSIX a `NAME=value` in the suffix is just a literal word.
                    args.push(expand_word(word, ctx)?);
                }
                CommandPrefixOrSuffixItem::Word(word) => {
                    args.push(expand_word(word, ctx)?);
                }
                CommandPrefixOrSuffixItem::IoRedirect(redir) => redirects.push(redir),
                CommandPrefixOrSuffixItem::ProcessSubstitution(_, _) => {
                    bail!("process substitution is not yet supported")
                }
            }
        }
    }

    let mut effective_input = input;
    for redir in redirects {
        apply_redirect(redir, ctx, &mut effective_input, output)?;
    }

    // No command name — this is a "pure assignment" statement. Commit
    // assignments to the shell's own scope and leave.
    if args.is_empty() {
        for (name, value) in assignments {
            ctx.variables.insert(name, value);
        }
        ctx.last_status = 0;
        return Ok(CommandStatus::SUCCESS);
    }

    let mut program = args.remove(0);
    let mut arguments = args;

    // POSIX-like `exec <prog> <args...>`: drop the `exec` keyword and
    // launch the real program in its place.
    if program == "exec" {
        if arguments.is_empty() {
            return Ok(CommandStatus::SUCCESS);
        }
        program = arguments.remove(0);
    }

    if let Some(result) = builtin::dispatch(ctx, &program, &arguments).await? {
        commit_output(output, &result.stdout).await?;
        ctx.last_status = result.status.code();
        return Ok(result.status);
    }

    launch_external(
        ctx,
        &program,
        arguments,
        assignments,
        effective_input,
        output,
    )
    .await
}

fn apply_redirect(
    redir: &IoRedirect,
    ctx: &Context,
    input: &mut InputSource,
    output: &mut OutputTarget,
) -> Result<()> {
    match redir {
        IoRedirect::File(_fd, kind, target) => {
            let filename = match target {
                IoFileRedirectTarget::Filename(word) => expand_word(word, ctx)?,
                _ => bail!("only filename redirection is supported"),
            };
            match kind {
                IoFileRedirectKind::Read => {
                    *input = InputSource::File(filename);
                }
                IoFileRedirectKind::Write | IoFileRedirectKind::Clobber => {
                    *output = OutputTarget::File {
                        path: filename,
                        append: false,
                    };
                }
                IoFileRedirectKind::Append => {
                    *output = OutputTarget::File {
                        path: filename,
                        append: true,
                    };
                }
                IoFileRedirectKind::ReadAndWrite | IoFileRedirectKind::DuplicateInput | IoFileRedirectKind::DuplicateOutput => {
                    bail!("only <, >, >> file redirections are supported")
                }
            }
            Ok(())
        }
        IoRedirect::HereDocument(_, _) => bail!("here-documents are not yet supported"),
        IoRedirect::HereString(_, _) => bail!("here-strings are not yet supported"),
        IoRedirect::OutputAndError(_, _) => bail!("&> redirection is not yet supported"),
    }
}

async fn launch_external(
    ctx: &mut Context,
    program: &str,
    args: Vec<String>,
    assignments: Vec<(String, String)>,
    input: InputSource,
    output: &mut OutputTarget,
) -> Result<CommandStatus> {
    let resolved = resolve_program(program).await?;
    // Use the full resolved path as argv[0] so runtimes that derive a
    // prefix from `argv[0]` (notably CPython's `getpath`) can locate
    // the standard library next to the executable.
    let name = resolved.path.clone();
    let stdin = resolve_stdin(input).await?;
    let request = SpawnRequest {
        name,
        args,
        env: assignments,
        wasm: resolved.wasm,
    };
    let child = programs::spawn(request)
        .await
        .map_err(|error| anyhow!("spawn failed: {:?}: {}", error.kind, error.detail))?;

    // Feed stdin (one-shot).
    if !stdin.is_empty() {
        child.write_stdin(stdin).await.ok();
    } else {
        // Still need to close stdin so the child sees EOF; write_stdin
        // with empty bytes does that.
        child.write_stdin(Vec::new()).await.ok();
    }

    let stdout_bytes = child.read_stdout().await;
    let stderr_bytes = child.read_stderr().await;
    let exit = child
        .wait()
        .await
        .map_err(|error| anyhow!("child.wait failed: {:?}: {}", error.kind, error.detail))?;

    commit_output(output, &stdout_bytes).await?;
    if !stderr_bytes.is_empty() {
        io::stderr().write_all(&stderr_bytes)?;
    }

    let code = u8::try_from(exit.code).unwrap_or(1);
    ctx.last_status = code;
    Ok(CommandStatus::new(code))
}

async fn resolve_stdin(input: InputSource) -> Result<Vec<u8>> {
    match input {
        InputSource::Empty => Ok(Vec::new()),
        InputSource::Bytes(bytes) => Ok(bytes),
        InputSource::File(path) => helios_fs::read(&path)
            .await
            .map_err(|error| anyhow!("failed to read stdin redirect {path}: {error:#}")),
    }
}

async fn commit_output(output: &mut OutputTarget, bytes: &[u8]) -> Result<()> {
    match output {
        OutputTarget::Inherit => {
            io::stdout().write_all(bytes)?;
            Ok(())
        }
        OutputTarget::Buffer(buffer) => {
            buffer.extend_from_slice(bytes);
            Ok(())
        }
        OutputTarget::File { path, append } => {
            if *append {
                let existing = helios_fs::read(path.as_str()).await.unwrap_or_default();
                let mut buf = existing;
                buf.extend_from_slice(bytes);
                helios_fs::write(path.as_str(), buf)
                    .await
                    .map_err(|e| anyhow!("failed to write redirect target {path}: {e:#}"))
            } else {
                helios_fs::write(path.as_str(), bytes.to_vec())
                    .await
                    .map_err(|e| anyhow!("failed to write redirect target {path}: {e:#}"))
            }
        }
    }
}

fn expand_word(word: &Word, ctx: &Context) -> Result<String> {
    expand_text(&word.value, ctx)
}

fn expand_assignment_value(value: &brush_parser::ast::AssignmentValue, ctx: &Context) -> Result<String> {
    use brush_parser::ast::AssignmentValue;
    match value {
        AssignmentValue::Scalar(word) => expand_word(word, ctx),
        AssignmentValue::Array(_) => bail!("array assignments are not yet supported"),
    }
}

/// Minimal `$VAR` expansion. Shell word semantics are huge; we
/// implement just enough to cover the common CLI invocations we need.
fn expand_text(raw: &str, ctx: &Context) -> Result<String> {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\'' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'\'' {
                    out.push(bytes[i] as char);
                    i += 1;
                }
                i += 1;
            }
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'$' {
                        let (value, new_i) = expand_dollar(raw, i, ctx);
                        out.push_str(&value);
                        i = new_i;
                        continue;
                    }
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        out.push(bytes[i + 1] as char);
                        i += 2;
                        continue;
                    }
                    out.push(bytes[i] as char);
                    i += 1;
                }
                i += 1;
            }
            b'\\' if i + 1 < bytes.len() => {
                out.push(bytes[i + 1] as char);
                i += 2;
            }
            b'$' => {
                let (value, new_i) = expand_dollar(raw, i, ctx);
                out.push_str(&value);
                i = new_i;
            }
            _ => {
                out.push(b as char);
                i += 1;
            }
        }
    }
    Ok(out)
}

fn expand_dollar(raw: &str, start: usize, ctx: &Context) -> (String, usize) {
    let bytes = raw.as_bytes();
    debug_assert_eq!(bytes[start], b'$');
    if start + 1 >= bytes.len() {
        return ("$".to_string(), start + 1);
    }
    // $? → last status
    if bytes[start + 1] == b'?' {
        return (ctx.last_status.to_string(), start + 2);
    }
    // ${NAME}
    if bytes[start + 1] == b'{' {
        if let Some(close) = raw[start + 2..].find('}') {
            let name = &raw[start + 2..start + 2 + close];
            let value = ctx.variables.get(name).cloned().unwrap_or_default();
            return (value, start + 2 + close + 1);
        }
    }
    // $NAME (letters/digits/underscore)
    let mut end = start + 1;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    if end == start + 1 {
        return ("$".to_string(), start + 1);
    }
    let name = &raw[start + 1..end];
    let value = ctx.variables.get(name).cloned().unwrap_or_default();
    (value, end)
}

struct Resolved {
    path: String,
    wasm: Vec<u8>,
}

async fn resolve_program(input: &str) -> Result<Resolved> {
    let mut errors = Vec::new();
    for path in candidate_paths(input) {
        match helios_fs::read(&path).await {
            Ok(wasm) => return Ok(Resolved { path, wasm }),
            Err(error) => errors.push(format!("{path}: {error:#}")),
        }
    }
    bail!(
        "failed to locate executable program {input:?}:\n{}",
        errors.join("\n")
    )
}

fn candidate_paths(input: &str) -> Vec<String> {
    if input.contains('/') || input.starts_with('.') {
        let mut candidates = vec![input.to_owned()];
        if !input.ends_with(".wasm") {
            candidates.push(format!("{input}.wasm"));
        }
        return candidates;
    }
    let mut candidates = Vec::new();
    for dir in PROGRAM_SEARCH_DIRECTORIES {
        candidates.push(format!("{dir}/{input}"));
        if !input.ends_with(".wasm") {
            candidates.push(format!("{dir}/{input}.wasm"));
        }
    }
    candidates
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

#[allow(dead_code)]
fn _pathbuf_for(path: &str) -> PathBuf {
    PathBuf::from(path)
}
