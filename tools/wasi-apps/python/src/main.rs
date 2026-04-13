use std::fs;
use std::io::{self, Write};

use anyhow::{Context as _, Result};
use evalexpr::eval;

fn run_source(source: &str) -> Result<()> {
    for raw_line in source.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        let expr = line
            .strip_prefix("print(")
            .and_then(|rest| rest.strip_suffix(')'))
            .map(str::trim)
            .with_context(|| format!("unsupported statement: {line}"))?;

        let value = eval(expr).with_context(|| format!("failed to evaluate expression: {expr}"))?;
        writeln!(io::stdout(), "{value}").context("failed to write python output")?;
    }

    Ok(())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let source = match args.next() {
        Some(flag) if flag == "-c" => match args.next() {
            Some(code) => code,
            None => {
                let _ = writeln!(io::stderr(), "python: -c requires an argument");
                return;
            }
        },
        Some(path) => match fs::read_to_string(&path) {
            Ok(code) => code,
            Err(error) => {
                let _ = writeln!(io::stderr(), "python: failed to read {path}: {error}");
                return;
            }
        },
        None => {
            let _ = writeln!(io::stderr(), "python: usage: python -c <code> | python <script>");
            return;
        }
    };

    if let Err(error) = run_source(&source) {
        let _ = writeln!(io::stderr(), "{error:#}");
    }
}
