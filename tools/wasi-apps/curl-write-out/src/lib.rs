//! `--write-out` template expansion shared by both curl builds in
//! `tools/wasi-apps/` — `curl` on Helios and `wasi-curl` under Wasmtime
//! on Linux — so the two sides of the benchmark emit the same bytes.

#![no_std]

extern crate alloc;

use alloc::string::{String, ToString};
use thiserror::Error;

/// A `--write-out` expansion failure.
#[derive(Debug, Error)]
pub enum Error {
    /// The template names a `%{…}` variable outside the supported subset.
    #[error("unsupported write-out variable in `{template}`")]
    UnsupportedVariable { template: String },
}

/// `--write-out` interpolation, the curl subset this tree's callers use:
/// `%{size_download}` for the received body length, `%%` for a literal
/// percent sign, and the `\\n`, `\\r`, `\\t` and `\\\\` escapes curl
/// expands. An unknown `%{…}` variable is an error; an unknown `\\x`
/// escape passes both characters through.
pub fn expand_write_out(template: &str, size_download: usize) -> Result<String, Error> {
    const SIZE_DOWNLOAD: &str = "%{size_download}";
    let mut rendered = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(ch) = rest.chars().next() {
        if let Some(tail) = rest.strip_prefix(SIZE_DOWNLOAD) {
            rendered.push_str(&size_download.to_string());
            rest = tail;
            continue;
        }
        if let Some(tail) = rest.strip_prefix("%%") {
            rendered.push('%');
            rest = tail;
            continue;
        }
        if rest.starts_with("%{") {
            return Err(Error::UnsupportedVariable {
                template: String::from(template),
            });
        }
        if ch != '\\' {
            rendered.push(ch);
            rest = &rest[ch.len_utf8()..];
            continue;
        }
        rest = &rest[1..];
        let Some(escape) = rest.chars().next() else {
            rendered.push('\\');
            break;
        };
        rest = &rest[escape.len_utf8()..];
        match escape {
            'n' => rendered.push('\n'),
            'r' => rendered.push('\r'),
            't' => rendered.push('\t'),
            '\\' => rendered.push('\\'),
            other => {
                rendered.push('\\');
                rendered.push(other);
            }
        }
    }
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_out_interprets_curl_escapes() {
        assert_eq!(
            expand_write_out("curl-http-throughput:%{size_download}\\n", 67108864).unwrap(),
            "curl-http-throughput:67108864\n"
        );
        assert_eq!(expand_write_out("\\ta\\rb\\\\c", 0).unwrap(), "\ta\rb\\c");
        assert_eq!(expand_write_out("100%%", 0).unwrap(), "100%");
        assert!(expand_write_out("%{unknown}", 0).is_err());
    }
}
