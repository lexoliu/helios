use anyhow::{Result, bail};
use async_trait::async_trait;

/// A parsed script: a sequence of top-level statements, executed in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    /// A logical command chain joined by `;`, `&&`, and `||` operators.
    ///
    /// This is the smallest building block — evaluating a pipeline is
    /// delegated to the host via `ScriptHost::execute_line`.
    Pipeline(Pipeline),
    /// `if <cond> then <branch> [else <branch>] end`.
    If {
        condition: Pipeline,
        then_branch: Vec<Statement>,
        else_branch: Vec<Statement>,
    },
}

/// A sequence of commands glued by short-circuit operators and sequencing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pipeline {
    pub head: String,
    pub tail: Vec<PipelineStep>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operator {
    /// `cmd1 ; cmd2` — always run `cmd2` regardless of `cmd1` status.
    Sequence,
    /// `cmd1 && cmd2` — run `cmd2` only when `cmd1` succeeds.
    AndIf,
    /// `cmd1 || cmd2` — run `cmd2` only when `cmd1` fails.
    OrIf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineStep {
    pub operator: Operator,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseState {
    Complete(Vec<Statement>),
    Incomplete,
}

#[async_trait(?Send)]
pub trait ScriptHost {
    /// Execute a single simple command (already split out of any
    /// `;`/`&&`/`||` chain). The host is free to interpret builtins and
    /// delegate `exec <prog> <args...>` to a program loader.
    async fn execute_line(&mut self, line: &str) -> Result<CommandStatus>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandStatus {
    code: u8,
    should_exit: bool,
}

impl CommandStatus {
    pub const SUCCESS: Self = Self {
        code: 0,
        should_exit: false,
    };

    pub const fn new(code: u8) -> Self {
        Self {
            code,
            should_exit: false,
        }
    }

    pub const fn exiting(code: u8) -> Self {
        Self {
            code,
            should_exit: true,
        }
    }

    pub const fn code(self) -> u8 {
        self.code
    }

    pub const fn is_success(self) -> bool {
        self.code == 0
    }

    pub const fn should_exit(self) -> bool {
        self.should_exit
    }
}

pub async fn execute_script<H: ScriptHost>(
    host: &mut H,
    program: &[Statement],
) -> Result<CommandStatus> {
    let mut last = CommandStatus::SUCCESS;
    for statement in program {
        last = execute_statement(host, statement).await?;
        if last.should_exit() {
            return Ok(last);
        }
    }
    Ok(last)
}

pub fn parse(input: &str) -> Result<ParseState> {
    let lines = input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return Ok(ParseState::Complete(Vec::new()));
    }
    if needs_more_input(input) {
        return Ok(ParseState::Incomplete);
    }

    let mut parser = BlockParser { lines, cursor: 0 };
    let block = parser.parse_block(BlockStop::TopLevel)?;
    if parser.cursor != parser.lines.len() {
        return Ok(ParseState::Incomplete);
    }
    Ok(ParseState::Complete(block))
}

fn needs_more_input(input: &str) -> bool {
    let mut depth = 0usize;
    for line in input.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "end" {
            depth = depth.saturating_sub(1);
            continue;
        }
        if line == "else" || line.starts_with("else if ") {
            continue;
        }
        if line.starts_with("if ") {
            depth = depth.saturating_add(1);
        }
    }
    depth != 0
}

async fn execute_statement<H: ScriptHost>(
    host: &mut H,
    statement: &Statement,
) -> Result<CommandStatus> {
    match statement {
        Statement::Pipeline(pipeline) => execute_pipeline(host, pipeline).await,
        Statement::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let status = execute_pipeline(host, condition).await?;
            if status.should_exit() {
                return Ok(status);
            }
            if status.is_success() {
                Box::pin(execute_script(host, then_branch)).await
            } else {
                Box::pin(execute_script(host, else_branch)).await
            }
        }
    }
}

async fn execute_pipeline<H: ScriptHost>(
    host: &mut H,
    pipeline: &Pipeline,
) -> Result<CommandStatus> {
    let mut status = host.execute_line(&pipeline.head).await?;
    if status.should_exit() {
        return Ok(status);
    }
    for step in &pipeline.tail {
        let should_run = match step.operator {
            Operator::Sequence => true,
            Operator::AndIf => status.is_success(),
            Operator::OrIf => !status.is_success(),
        };
        if !should_run {
            continue;
        }
        status = host.execute_line(&step.command).await?;
        if status.should_exit() {
            return Ok(status);
        }
    }
    Ok(status)
}

/// Split a single shell line into a [`Pipeline`] on the top-level operators
/// `;`, `&&`, `||`. Operators inside single or double quotes are preserved
/// as part of the surrounding command.
fn split_into_pipeline(line: &str) -> Result<Pipeline> {
    let mut segments = Vec::new();
    let mut operators = Vec::new();
    let bytes = line.as_bytes();
    let mut start = 0usize;
    let mut idx = 0usize;
    let mut single_quote = false;
    let mut double_quote = false;
    let mut escaped = false;

    while idx < bytes.len() {
        let byte = bytes[idx];
        if escaped {
            escaped = false;
            idx += 1;
            continue;
        }
        if byte == b'\\' && !single_quote {
            escaped = true;
            idx += 1;
            continue;
        }
        if byte == b'\'' && !double_quote {
            single_quote = !single_quote;
            idx += 1;
            continue;
        }
        if byte == b'"' && !single_quote {
            double_quote = !double_quote;
            idx += 1;
            continue;
        }
        if single_quote || double_quote {
            idx += 1;
            continue;
        }
        if byte == b';' {
            segments.push(line[start..idx].trim().to_owned());
            operators.push(Operator::Sequence);
            idx += 1;
            start = idx;
            continue;
        }
        if byte == b'&' && idx + 1 < bytes.len() && bytes[idx + 1] == b'&' {
            segments.push(line[start..idx].trim().to_owned());
            operators.push(Operator::AndIf);
            idx += 2;
            start = idx;
            continue;
        }
        if byte == b'|' && idx + 1 < bytes.len() && bytes[idx + 1] == b'|' {
            segments.push(line[start..idx].trim().to_owned());
            operators.push(Operator::OrIf);
            idx += 2;
            start = idx;
            continue;
        }
        idx += 1;
    }

    if single_quote || double_quote || escaped {
        bail!("unterminated quoted string in shell line {line:?}");
    }

    let final_segment = line[start..].trim().to_owned();
    segments.push(final_segment);

    if segments.iter().any(|segment| segment.is_empty()) {
        bail!("empty command between operators in shell line {line:?}");
    }

    let mut iter = segments.into_iter();
    let head = iter.next().expect("at least one segment");
    let tail = iter
        .zip(operators)
        .map(|(command, operator)| PipelineStep { operator, command })
        .collect();
    Ok(Pipeline { head, tail })
}

struct BlockParser<'a> {
    lines: Vec<&'a str>,
    cursor: usize,
}

#[derive(Clone, Copy)]
enum BlockStop {
    TopLevel,
    ElseOrEnd,
    End,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Terminator {
    Else,
    End,
    Eof,
}

impl BlockParser<'_> {
    fn parse_block(&mut self, stop: BlockStop) -> Result<Vec<Statement>> {
        let mut statements = Vec::new();
        loop {
            match self.peek_terminator() {
                Some(Terminator::Else) => {
                    if matches!(stop, BlockStop::ElseOrEnd) {
                        return Ok(statements);
                    }
                    bail!("unexpected `else` without matching `if`");
                }
                Some(Terminator::End) => {
                    if matches!(stop, BlockStop::ElseOrEnd | BlockStop::End) {
                        return Ok(statements);
                    }
                    bail!("unexpected `end` without an open block");
                }
                Some(Terminator::Eof) => {
                    if matches!(stop, BlockStop::TopLevel) {
                        return Ok(statements);
                    }
                    return Ok(Vec::new());
                }
                None => {}
            }

            let line = self
                .next_line()
                .expect("terminator check guarantees a line")
                .to_owned();
            if let Some(condition) = line.strip_prefix("if ") {
                statements.push(self.parse_if(condition.trim())?);
                continue;
            }
            statements.push(Statement::Pipeline(split_into_pipeline(&line)?));
        }
    }

    fn parse_if(&mut self, condition: &str) -> Result<Statement> {
        if condition.is_empty() {
            bail!("`if` requires a condition command");
        }

        let condition = split_into_pipeline(condition)?;
        let then_branch = self.parse_block(BlockStop::ElseOrEnd)?;
        let else_branch = match self.peek_terminator() {
            Some(Terminator::Else) => {
                self.cursor += 1;
                if let Some(condition) = self
                    .lines
                    .get(self.cursor - 1)
                    .and_then(|line| line.strip_prefix("else if "))
                {
                    vec![self.parse_if(condition.trim())?]
                } else {
                    let branch = self.parse_block(BlockStop::End)?;
                    self.consume_end()?;
                    branch
                }
            }
            Some(Terminator::End) => {
                self.consume_end()?;
                Vec::new()
            }
            Some(Terminator::Eof) => bail!("missing `end` for if"),
            None => bail!("parser lost block state while parsing if"),
        };

        Ok(Statement::If {
            condition,
            then_branch,
            else_branch,
        })
    }

    fn consume_end(&mut self) -> Result<()> {
        match self.peek_terminator() {
            Some(Terminator::End) => {
                self.cursor += 1;
                Ok(())
            }
            Some(Terminator::Eof) => bail!("missing `end` to close the current block"),
            _ => bail!("expected `end` to close the current block"),
        }
    }

    fn peek_terminator(&self) -> Option<Terminator> {
        let Some(line) = self.lines.get(self.cursor).copied() else {
            return Some(Terminator::Eof);
        };
        if line == "end" {
            return Some(Terminator::End);
        }
        if line == "else" || line.starts_with("else if ") {
            return Some(Terminator::Else);
        }
        None
    }

    fn next_line(&mut self) -> Option<&str> {
        let line = self.lines.get(self.cursor).copied()?;
        self.cursor += 1;
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_semicolon_separated_commands() {
        let pipeline = split_into_pipeline("true ; echo hi ; false").unwrap();
        assert_eq!(pipeline.head, "true");
        assert_eq!(pipeline.tail.len(), 2);
        assert_eq!(pipeline.tail[0].operator, Operator::Sequence);
        assert_eq!(pipeline.tail[0].command, "echo hi");
        assert_eq!(pipeline.tail[1].operator, Operator::Sequence);
        assert_eq!(pipeline.tail[1].command, "false");
    }

    #[test]
    fn splits_short_circuit_operators() {
        let pipeline = split_into_pipeline("true && echo ok || echo fail").unwrap();
        assert_eq!(pipeline.head, "true");
        assert_eq!(pipeline.tail[0].operator, Operator::AndIf);
        assert_eq!(pipeline.tail[0].command, "echo ok");
        assert_eq!(pipeline.tail[1].operator, Operator::OrIf);
        assert_eq!(pipeline.tail[1].command, "echo fail");
    }

    #[test]
    fn preserves_operators_inside_quotes() {
        let pipeline = split_into_pipeline("echo 'a && b' \"c || d\"").unwrap();
        assert!(pipeline.tail.is_empty());
        assert_eq!(pipeline.head, "echo 'a && b' \"c || d\"");
    }

    #[test]
    fn rejects_empty_segment() {
        assert!(split_into_pipeline("echo hi ;;").is_err());
    }

    #[test]
    fn rejects_unterminated_quote() {
        assert!(split_into_pipeline("echo 'hi").is_err());
    }
}
