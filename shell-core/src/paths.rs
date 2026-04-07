use std::path::{Component, Path};

use anyhow::{Result, bail};

/// Normalizes a guest path into the shell's root-anchored absolute form.
///
/// The Helios shell never permits parent traversal because guest commands are
/// evaluated relative to the debugger's synthetic root.
pub fn normalize_absolute(input: &str) -> Result<String> {
    let segments = normalize_segments(input)?;
    if segments.is_empty() {
        return Ok("/".to_owned());
    }

    Ok(format!("/{}", segments.join("/")))
}

pub fn normalize_segments(input: &str) -> Result<Vec<String>> {
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
    Ok(segments)
}

#[cfg(test)]
mod tests {
    use super::{normalize_absolute, normalize_segments};

    #[test]
    fn normalizes_relative_paths_from_root() {
        assert_eq!(normalize_absolute("tmp/file").unwrap(), "/tmp/file");
    }


    #[test]
    fn normalizes_relative_segments() {
        assert_eq!(normalize_segments("tmp/file").unwrap(), vec!["tmp", "file"]);
    }
    #[test]
    fn normalizes_absolute_paths() {
        assert_eq!(normalize_absolute("/tmp/file").unwrap(), "/tmp/file");
    }

    #[test]
    fn rejects_parent_traversal() {
        assert!(normalize_absolute("../etc").is_err());
    }
}
