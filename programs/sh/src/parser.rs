//! Thin wrapper around `brush-parser` to produce the AST our executor
//! consumes. We intentionally keep brush's AST as the canonical shape —
//! the executor walks it directly rather than re-lowering it into a
//! custom representation.

use brush_parser::ast::Program;
use brush_parser::{ParserOptions, SourceInfo};

use crate::error::{Result, ShellError};

pub fn parse(source: &str) -> Result<Program> {
    let options = ParserOptions {
        posix_mode: true,
        sh_mode: true,
        enable_extended_globbing: false,
        tilde_expansion: false,
    };
    let source_info = SourceInfo {
        source: "shell".to_owned(),
    };

    let tokens = brush_parser::tokenize_str_with_options(source, &options.tokenizer_options())
        .map_err(|error| {
            ShellError::message(format!("failed to tokenize shell script: {error}"))
        })?;
    brush_parser::parse_tokens(&tokens, &options, &source_info)
        .map_err(|error| ShellError::message(format!("failed to parse shell script: {error}")))
}
