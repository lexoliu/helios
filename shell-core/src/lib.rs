use anyhow::{Result, bail};
use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    Command(String),
    If {
        condition: String,
        then_branch: Vec<Statement>,
        else_branch: Vec<Statement>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseState {
    Complete(Vec<Statement>),
    Incomplete,
}

#[async_trait(?Send)]
pub trait ScriptHost {
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

pub async fn execute_script<H: ScriptHost>(host: &mut H, program: &[Statement]) -> Result<CommandStatus> {
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
    if needs_more_input(input)? {
        return Ok(ParseState::Incomplete);
    }

    let mut parser = BlockParser { lines, cursor: 0 };
    let block = parser.parse_block(BlockStop::TopLevel)?;
    if parser.cursor != parser.lines.len() {
        return Ok(ParseState::Incomplete);
    }
    Ok(ParseState::Complete(block))
}

pub fn needs_more_input(input: &str) -> Result<bool> {
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
    Ok(depth != 0)
}

async fn execute_statement<H: ScriptHost>(host: &mut H, statement: &Statement) -> Result<CommandStatus> {
    match statement {
        Statement::Command(line) => host.execute_line(line).await,
        Statement::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let status = host.execute_line(condition).await?;
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
            statements.push(Statement::Command(line));
        }
    }

    fn parse_if(&mut self, condition: &str) -> Result<Statement> {
        if condition.is_empty() {
            bail!("`if` requires a condition command");
        }

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
            Some(Terminator::Eof) => bail!("missing `end` for `if {condition}`"),
            None => bail!("parser lost block state while parsing `if {condition}`"),
        };

        Ok(Statement::If {
            condition: condition.to_owned(),
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
    use super::{CommandStatus, ParseState, ScriptHost, Statement, execute_script, needs_more_input, parse};
    use anyhow::Result;
    use async_trait::async_trait;

    #[test]
    fn parses_simple_if_else_end_block() {
        let script = "if test -e /tmp/file\n echo yes\nelse\n echo no\nend";
        let ParseState::Complete(statements) = parse(script).expect("script parse must succeed")
        else {
            panic!("script must be complete");
        };

        assert_eq!(
            statements,
            vec![Statement::If {
                condition: "test -e /tmp/file".to_owned(),
                then_branch: vec![Statement::Command("echo yes".to_owned())],
                else_branch: vec![Statement::Command("echo no".to_owned())],
            }]
        );
    }

    #[test]
    fn detects_incomplete_if_block() {
        assert!(needs_more_input("if test -e /tmp/file\n echo hi").expect("parse must succeed"));
    }

    #[test]
    fn parses_else_if_as_nested_if() {
        let script = "if test -e /a\n echo a\nelse if test -e /b\n echo b\nend";
        let ParseState::Complete(statements) = parse(script).expect("script parse must succeed")
        else {
            panic!("script must be complete");
        };

        assert!(matches!(
            &statements[0],
            Statement::If {
                else_branch,
                ..
            } if matches!(&else_branch[0], Statement::If { condition, .. } if condition == "test -e /b")
        ));
    }

    struct ScriptRecordingHost {
        lines: Vec<String>,
    }

    #[async_trait(?Send)]
    impl ScriptHost for ScriptRecordingHost {
        async fn execute_line(&mut self, line: &str) -> Result<CommandStatus> {
            self.lines.push(line.to_owned());
            Ok(CommandStatus::SUCCESS)
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn executes_block_script_via_host_trait() {
        let ParseState::Complete(program) = parse("if test\n echo yes\nelse\n echo no\nend")
            .expect("script parse must succeed")
        else {
            panic!("script must be complete");
        };
        let mut host = ScriptRecordingHost { lines: Vec::new() };
        let status = execute_script(&mut host, &program)
            .await
            .expect("script execution must succeed");
        assert!(status.is_success());
        assert_eq!(host.lines, vec!["test", "echo yes"]);
    }
}
