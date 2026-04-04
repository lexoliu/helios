use std::fmt::Write as _;
use std::path::{Component, Path};

use anyhow::{bail, Result};
use helios_shell_protocol::debugger::filesystem::{self, DirectoryEntry, EntryKind};

use crate::serial::RpcClient;

pub async fn list(client: &mut RpcClient, path: Option<&str>) -> Result<String> {
    let path = normalize_path(path.unwrap_or("/"))?;
    let entries = filesystem::list(client, &path).await?;
    render_entries(&entries)
}

pub async fn remove(client: &mut RpcClient, path: &str) -> Result<()> {
    let path = normalize_path(path)?;
    filesystem::remove(client, &path).await
}

pub async fn touch(client: &mut RpcClient, path: &str) -> Result<()> {
    let path = normalize_path(path)?;
    filesystem::touch(client, &path).await
}

fn render_entries(entries: &[DirectoryEntry]) -> Result<String> {
    let mut output = String::new();
    for entry in entries {
        writeln!(&mut output, "{}", display_entry(entry))?;
    }
    Ok(output)
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
    use super::normalize_path;

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
}
