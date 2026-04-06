use std::fmt::Write as _;
use std::path::{Component, Path};

use anyhow::{bail, Result};
use globset::Glob;
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
    let path = path.unwrap_or("/");
    if has_glob(path) {
        return list_glob(client, path).await;
    }
    let path = normalize_path(path)?;
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

pub async fn copy(client: &mut RpcClient, source: &str, destination: &str) -> Result<()> {
    let source = normalize_path(source)?;
    let destination = normalize_path(destination)?;
    filesystem::copy(client, &source, &destination).await
}

pub async fn move_path(client: &mut RpcClient, source: &str, destination: &str) -> Result<()> {
    let source = normalize_path(source)?;
    let destination = normalize_path(destination)?;
    filesystem::move_path(client, &source, &destination).await
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

async fn list_glob(client: &mut RpcClient, pattern: &str) -> Result<String> {
    let pattern = normalize_glob_pattern(pattern)?;
    let matcher = Glob::new(&pattern)
        .map_err(|error| anyhow::anyhow!("invalid glob pattern {pattern:?}: {error}"))?
        .compile_matcher();
    let mut paths = Vec::new();
    collect_paths(client, "/", &mut paths).await?;
    paths.sort_by(|left, right| left.0.cmp(&right.0));

    let mut output = String::new();
    for (path, kind) in paths {
        if matcher.is_match(path.trim_start_matches('/')) {
            writeln!(&mut output, "{}", display_path(&path, kind))?;
        }
    }
    Ok(output)
}

async fn collect_paths(
    client: &mut RpcClient,
    directory: &str,
    output: &mut Vec<(String, EntryKind)>,
) -> Result<()> {
    let entries = filesystem::list(client, directory).await?;
    for entry in entries {
        let path = join_path(directory, &entry.name);
        output.push((path.clone(), entry.kind));
        if matches!(entry.kind, EntryKind::Directory) {
            Box::pin(collect_paths(client, &path, output)).await?;
        }
    }
    Ok(())
}

fn join_path(parent: &str, child: &str) -> String {
    if parent == "/" {
        format!("/{child}")
    } else {
        format!("{parent}/{child}")
    }
}

fn display_path(path: &str, kind: EntryKind) -> String {
    match kind {
        EntryKind::Directory => format!("{path}/"),
        EntryKind::File | EntryKind::Other => path.to_owned(),
    }
}

fn has_glob(input: &str) -> bool {
    input.contains('*') || input.contains('?') || input.contains('[')
}

fn normalize_glob_pattern(input: &str) -> Result<String> {
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

    let mut normalized = Vec::new();
    for segment in segments {
        if segment.starts_with("**") && segment.len() > 2 {
            normalized.push("**".to_owned());
            normalized.push(format!("*{}", &segment[2..]));
        } else {
            normalized.push(segment);
        }
    }

    Ok(normalized.join("/"))
}

pub fn normalize_path(input: &str) -> Result<String> {
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
    use super::{has_glob, normalize_glob_pattern, normalize_path, parse_echo, EchoTarget};

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

    #[test]
    fn detects_globs() {
        assert!(has_glob("**.jpg"));
        assert!(has_glob("/tmp/*.txt"));
        assert!(!has_glob("/tmp/file"));
    }

    #[test]
    fn normalizes_recursive_glob_suffixes() {
        assert_eq!(normalize_glob_pattern("**.jpg").unwrap(), "**/*.jpg");
        assert_eq!(
            normalize_glob_pattern("/tmp/**.log").unwrap(),
            "tmp/**/*.log"
        );
    }
}
