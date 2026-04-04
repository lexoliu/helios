use std::fmt::Write as _;
use std::path::{Component, Path};

use anyhow::{Result, bail};
use helios_shell_protocol::debugger::filesystem::{self, DirectoryEntry, EntryKind};

use crate::serial::RpcClient;

pub enum EchoTarget {
    Stdout(Vec<u8>),
    File {
        path: String,
        bytes: Vec<u8>,
        append: bool,
    },
}

pub fn pwd() -> &'static str {
    "/"
}

pub async fn list(client: &mut RpcClient, path: Option<&str>) -> Result<String> {
    let path = normalize_path(path.unwrap_or("/"))?;
    let entries = filesystem::list(client, &path).await?;
    render_entries(&entries)
}

pub async fn cat(client: &mut RpcClient, path: &str) -> Result<Vec<u8>> {
    let path = normalize_path(path)?;
    filesystem::read(client, &path).await
}

pub async fn mkdir(client: &mut RpcClient, path: &str) -> Result<()> {
    let path = normalize_path(path)?;
    filesystem::mkdir(client, &path).await
}

pub async fn remove(client: &mut RpcClient, path: &str) -> Result<()> {
    let path = normalize_path(path)?;
    filesystem::remove(client, &path).await
}

pub async fn touch(client: &mut RpcClient, path: &str) -> Result<()> {
    let path = normalize_path(path)?;
    filesystem::touch(client, &path).await
}

pub async fn write(client: &mut RpcClient, path: &str, bytes: &[u8], append: bool) -> Result<()> {
    let path = normalize_path(path)?;
    filesystem::write(client, &path, bytes, append).await
}

pub fn parse_echo(words: &[String]) -> Result<EchoTarget> {
    let redirects = words
        .iter()
        .enumerate()
        .filter(|(_, word)| matches!(word.as_str(), ">" | ">>"))
        .collect::<Vec<_>>();
    if redirects.is_empty() {
        return Ok(EchoTarget::Stdout(render_echo_bytes(words)));
    }

    if redirects.len() != 1 {
        bail!("echo accepts at most one redirection target");
    }

    let (index, operator) = redirects[0];
    if index + 2 != words.len() {
        bail!("echo redirection must end with exactly one destination path");
    }

    Ok(EchoTarget::File {
        path: normalize_path(&words[index + 1])?,
        bytes: render_echo_bytes(&words[..index]),
        append: operator == ">>",
    })
}

fn render_entries(entries: &[DirectoryEntry]) -> Result<String> {
    let mut output = String::new();
    for entry in entries {
        writeln!(&mut output, "{}", display_entry(entry))?;
    }
    Ok(output)
}

fn render_echo_bytes(words: &[String]) -> Vec<u8> {
    let mut bytes = words.join(" ").into_bytes();
    bytes.push(b'\n');
    bytes
}

fn display_entry(entry: &DirectoryEntry) -> String {
    match entry.kind {
        EntryKind::Directory => format!("{}/", entry.name),
        EntryKind::File | EntryKind::Other => entry.name.clone(),
    }
}

fn normalize_path(input: &str) -> Result<String> {
    let path = Path::new(input);
    let mut segments = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(segment) => segments.push(
                segment
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("path {input:?} contains a non-utf8 segment"))?
                    .to_owned(),
            ),
            Component::ParentDir => bail!("path {input:?} contains unsupported parent traversal"),
            Component::Prefix(_) => bail!("path {input:?} uses an unsupported path prefix"),
        }
    }

    if segments.is_empty() {
        return Ok("/".to_owned());
    }

    Ok(format!("/{}", segments.join("/")))
}

#[cfg(test)]
mod tests {
    use super::{EchoTarget, normalize_path, parse_echo};

    #[test]
    fn normalizes_relative_paths_from_root() {
        assert_eq!(normalize_path("tmp/file").unwrap(), "/tmp/file");
    }

    #[test]
    fn normalizes_absolute_paths() {
        assert_eq!(normalize_path("/tmp/file").unwrap(), "/tmp/file");
    }

    #[test]
    fn rejects_parent_traversal() {
        assert!(normalize_path("../etc").is_err());
    }

    #[test]
    fn parses_echo_stdout() {
        match parse_echo(&["hello".to_owned(), "world".to_owned()]).unwrap() {
            EchoTarget::Stdout(bytes) => assert_eq!(bytes, b"hello world\n"),
            _ => panic!("echo without redirection must target stdout"),
        }
    }

    #[test]
    fn parses_echo_overwrite_redirection() {
        match parse_echo(&["hello".to_owned(), ">".to_owned(), "/tmp/file".to_owned()]).unwrap() {
            EchoTarget::File {
                path,
                bytes,
                append,
            } => {
                assert_eq!(path, "/tmp/file");
                assert_eq!(bytes, b"hello\n");
                assert!(!append);
            }
            _ => panic!("echo redirection must target a file"),
        }
    }

    #[test]
    fn parses_echo_append_redirection() {
        match parse_echo(&["x".to_owned(), ">>".to_owned(), "tmp/file".to_owned()]).unwrap() {
            EchoTarget::File {
                path,
                bytes,
                append,
            } => {
                assert_eq!(path, "/tmp/file");
                assert_eq!(bytes, b"x\n");
                assert!(append);
            }
            _ => panic!("echo append redirection must target a file"),
        }
    }
}
