use thiserror::Error;

use crate::error::{Result, ShellError};
use crate::ifs::IfsConfig;
use crate::streams::{InputStream, read_byte};

#[derive(Debug, Error)]
pub(crate) enum ReadCommandError {
    #[error("arg count")]
    ArgCount,
    #[error("Illegal option -{0}")]
    IllegalOption(char),
}

pub(crate) struct ReadCommand {
    pub(crate) raw_mode: bool,
    pub(crate) variable_names: Vec<String>,
}

pub(crate) struct ReadOutcome {
    pub(crate) values: Vec<String>,
    pub(crate) status: i32,
}

impl ReadCommand {
    pub(crate) fn parse(args: &[String]) -> std::result::Result<Self, ReadCommandError> {
        let mut raw_mode = false;
        let mut index = 0;

        while index < args.len() {
            let arg = &args[index];
            if arg == "--" {
                index += 1;
                break;
            }
            let Some(flags) = arg.strip_prefix('-') else {
                break;
            };
            if flags.is_empty() {
                break;
            }
            for flag in flags.chars() {
                match flag {
                    'r' => raw_mode = true,
                    other => return Err(ReadCommandError::IllegalOption(other)),
                }
            }
            index += 1;
        }

        let variable_names = args[index..].to_vec();
        if variable_names.is_empty() {
            return Err(ReadCommandError::ArgCount);
        }

        Ok(Self {
            raw_mode,
            variable_names,
        })
    }
}

pub(crate) async fn read_fields(
    input: &mut InputStream,
    command: &ReadCommand,
    ifs: IfsConfig<'_>,
) -> Result<ReadOutcome> {
    let (line, terminated_with_newline) = read_logical_line(input, command.raw_mode).await?;
    Ok(ReadOutcome {
        values: split_line_for_read(&line, command.variable_names.len(), ifs),
        status: if terminated_with_newline { 0 } else { 1 },
    })
}

async fn read_logical_line(input: &mut InputStream, raw_mode: bool) -> Result<(String, bool)> {
    let mut bytes = Vec::new();
    let mut pending_escape = false;

    loop {
        let Some(byte) = read_byte(input).await? else {
            return decode_read_bytes(bytes, false);
        };

        if raw_mode {
            if byte == b'\n' {
                return decode_read_bytes(bytes, true);
            }
            bytes.push(byte);
            continue;
        }

        if pending_escape {
            pending_escape = false;
            if byte == b'\n' {
                continue;
            }
            bytes.push(byte);
            continue;
        }

        match byte {
            b'\\' => pending_escape = true,
            b'\n' => return decode_read_bytes(bytes, true),
            _ => bytes.push(byte),
        }
    }
}

fn decode_read_bytes(bytes: Vec<u8>, terminated_with_newline: bool) -> Result<(String, bool)> {
    let line = String::from_utf8(bytes)
        .map_err(|error| ShellError::message(format!("read input is not valid utf-8: {error}")))?;
    Ok((line, terminated_with_newline))
}

fn split_line_for_read(line: &str, variable_count: usize, ifs: IfsConfig<'_>) -> Vec<String> {
    let chars = line.chars().collect::<Vec<_>>();
    let mut cursor = 0;
    let mut values = Vec::with_capacity(variable_count);

    for index in 0..variable_count {
        if index + 1 == variable_count {
            values.push(take_read_remainder(&chars, cursor, ifs));
        } else {
            let (field, next_cursor) = take_read_field(&chars, cursor, ifs);
            values.push(field);
            cursor = next_cursor;
        }
    }

    values
}

fn take_read_field(chars: &[char], cursor: usize, ifs: IfsConfig<'_>) -> (String, usize) {
    if ifs.is_empty() {
        return (chars[cursor..].iter().collect(), chars.len());
    }

    let mut index = skip_leading_ifs_whitespace(chars, cursor, ifs);
    if index >= chars.len() {
        return (String::new(), chars.len());
    }

    let start = index;
    while index < chars.len() && !ifs.contains(chars[index]) {
        index += 1;
    }

    let field = chars[start..index].iter().collect();
    (field, consume_read_delimiter(chars, index, ifs))
}

fn take_read_remainder(chars: &[char], cursor: usize, ifs: IfsConfig<'_>) -> String {
    if ifs.is_empty() {
        return chars[cursor..].iter().collect();
    }

    let start = skip_leading_ifs_whitespace(chars, cursor, ifs);
    let mut end = chars.len();
    while end > start && ifs.is_whitespace(chars[end - 1]) {
        end -= 1;
    }

    chars[start..end].iter().collect()
}

fn skip_leading_ifs_whitespace(chars: &[char], mut index: usize, ifs: IfsConfig<'_>) -> usize {
    while index < chars.len() && ifs.is_whitespace(chars[index]) {
        index += 1;
    }
    index
}

fn consume_read_delimiter(chars: &[char], mut index: usize, ifs: IfsConfig<'_>) -> usize {
    if index >= chars.len() {
        return index;
    }

    if ifs.is_whitespace(chars[index]) {
        while index < chars.len() && ifs.is_whitespace(chars[index]) {
            index += 1;
        }
        if index < chars.len() && ifs.is_non_whitespace(chars[index]) {
            index += 1;
            while index < chars.len() && ifs.is_whitespace(chars[index]) {
                index += 1;
            }
        }
        return index;
    }

    if ifs.is_non_whitespace(chars[index]) {
        index += 1;
        while index < chars.len() && ifs.is_whitespace(chars[index]) {
            index += 1;
        }
    }

    index
}
