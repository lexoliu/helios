use std::path::{Component, Path};

use anyhow::{Context as _, Result, bail};
use helios_inspector_protocol::debugger::programs as debugger_programs;
use helios_inspector_protocol::system::programs::ExecResult;

use crate::serial::RpcClient;

pub(crate) const REMOTE_SHELL_PATH: &str = "/bin/dash";

pub async fn exec(client: &mut RpcClient, path: &str, args: &[String]) -> Result<ExecResult> {
    let path = normalize_absolute(path)?;
    let outcome = debugger_programs::exec_path(&*client, &path, args)
        .await
        .context("failed to invoke remote programs.exec-path")?;
    outcome.map_err(|error| anyhow::anyhow!("{:?}: {}", error.kind, error.detail))
}

fn normalize_absolute(input: &str) -> Result<String> {
    let mut segments = Vec::new();
    for component in Path::new(input).components() {
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
    use super::normalize_absolute;

    #[test]
    fn normalizes_relative_paths_from_root() {
        assert_eq!(
            normalize_absolute("bin/dash").expect("relative path must normalize"),
            "/bin/dash"
        );
    }

    #[test]
    fn preserves_absolute_paths() {
        assert_eq!(
            normalize_absolute("/bin/dash").expect("absolute path must normalize"),
            "/bin/dash"
        );
    }

    #[test]
    fn rejects_parent_traversal() {
        assert!(normalize_absolute("../bin/dash").is_err());
    }
}
