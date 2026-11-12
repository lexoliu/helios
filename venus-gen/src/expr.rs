//! The arithmetic of `len`/`altlen` attributes: integers, API constants, one
//! member path and `+ - * /` with parentheses.

use crate::registry::RegistryError;

type Result<T> = std::result::Result<T, RegistryError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    Int(u64),
    /// An API constant such as `VK_UUID_SIZE`.
    Const(String),
    /// The member path the length is read from (`Len::name`).
    Path,
    Bin(Op, Box<Expr>, Box<Expr>),
    Paren(Box<Expr>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Int(u64),
    Ident(String),
    Path,
    Sym(char),
}

/// Parses `text`, in which `path` (when not empty) names the member path.
pub fn parse(text: &str, path: &str) -> Result<Expr> {
    let tokens = tokenize(text, path)?;
    let mut parser = Parser { tokens, pos: 0 };
    let expr = parser.sum()?;
    if parser.pos != parser.tokens.len() {
        return Err(RegistryError::unsupported(format!(
            "length expression `{text}` has trailing tokens"
        )));
    }
    Ok(expr)
}

fn tokenize(text: &str, path: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if c.is_whitespace() {
            rest = &rest[c.len_utf8()..];
        } else if !path.is_empty() && rest.starts_with(path) {
            tokens.push(Token::Path);
            rest = &rest[path.len()..];
        } else if c.is_ascii_digit() {
            let end = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            let value = rest[..end].parse().map_err(|_| {
                RegistryError::unsupported(format!("length expression `{text}` has a bad integer"))
            })?;
            tokens.push(Token::Int(value));
            rest = &rest[end..];
        } else if c.is_ascii_alphabetic() || c == '_' {
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            tokens.push(Token::Ident(rest[..end].to_owned()));
            rest = &rest[end..];
        } else if "+-*/()".contains(c) {
            tokens.push(Token::Sym(c));
            rest = &rest[1..];
        } else {
            return Err(RegistryError::unsupported(format!(
                "length expression `{text}` has the character `{c}`"
            )));
        }
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn sum(&mut self) -> Result<Expr> {
        let mut left = self.product()?;
        while let Some(Token::Sym(c @ ('+' | '-'))) = self.peek().cloned() {
            self.pos += 1;
            let right = self.product()?;
            let op = if c == '+' { Op::Add } else { Op::Sub };
            left = Expr::Bin(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn product(&mut self) -> Result<Expr> {
        let mut left = self.atom()?;
        while let Some(Token::Sym(c @ ('*' | '/'))) = self.peek().cloned() {
            self.pos += 1;
            let right = self.atom()?;
            let op = if c == '*' { Op::Mul } else { Op::Div };
            left = Expr::Bin(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn atom(&mut self) -> Result<Expr> {
        let token = self
            .peek()
            .cloned()
            .ok_or_else(|| RegistryError::unsupported("a length expression ends early"))?;
        self.pos += 1;
        match token {
            Token::Int(value) => Ok(Expr::Int(value)),
            Token::Ident(name) => Ok(Expr::Const(name)),
            Token::Path => Ok(Expr::Path),
            Token::Sym('(') => {
                let inner = self.sum()?;
                match self.peek() {
                    Some(Token::Sym(')')) => {
                        self.pos += 1;
                        Ok(Expr::Paren(Box::new(inner)))
                    }
                    _ => Err(RegistryError::unsupported("a length expression lacks `)`")),
                }
            }
            Token::Sym(c) => Err(RegistryError::unsupported(format!(
                "a length expression has an unexpected `{c}`"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_expressions_of_the_registry() {
        let samples = [
            ("(rasterizationSamples + 31) / 32", "rasterizationSamples"),
            ("2*VK_UUID_SIZE", ""),
            ("codeSize / 4", "codeSize"),
            ("pInfos[i].geometryCount", "pInfos[i].geometryCount"),
            ("32", ""),
        ];
        for (text, path) in samples {
            parse(text, path).unwrap_or_else(|e| panic!("{text}: {e}"));
        }
        let expr = parse("2*VK_UUID_SIZE", "").unwrap();
        assert_eq!(
            expr,
            Expr::Bin(
                Op::Mul,
                Box::new(Expr::Int(2)),
                Box::new(Expr::Const("VK_UUID_SIZE".into()))
            )
        );
    }
}
