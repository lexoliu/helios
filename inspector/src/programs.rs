use std::path::{Component, Path};

use helios_inspector_protocol::RpcError;
use helios_inspector_protocol::debugger::programs as debugger_programs;
use helios_inspector_protocol::system::programs::{ExecError, ExecErrorKind, ExecResult};

use crate::serial::RpcClient;

pub(crate) const REMOTE_SHELL_PATH: &str = "/bin/dash";

/// Why a program the inspector asked the guest to run did not run.
///
/// The path faults and the remote faults are separate variants because
/// they are answered differently: a path this side refuses never reaches
/// the guest, while a refusal from the guest carries the guest's own
/// classification of what went wrong.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ProgramError {
    #[error("path {input:?} contains a non-utf8 segment")]
    PathNotUtf8 { input: String },
    #[error("path {input:?} contains unsupported parent traversal")]
    PathParentTraversal { input: String },
    #[error("path {input:?} uses an unsupported path prefix")]
    PathPrefix { input: String },
    #[error("failed to invoke remote programs.exec-path: {source}")]
    Invoke {
        #[source]
        source: RpcError,
    },
    #[error("{kind:?}: {detail}")]
    Refused { kind: ExecErrorKind, detail: String },
}

impl ProgramError {
    /// The guest's panic report when this failure is a dead guest.
    ///
    /// The walk is typed rather than a downcast through an erased chain:
    /// only the RPC layer can carry a panic report, so only that variant
    /// is asked for one.
    pub(crate) fn guest_panic(&self) -> Option<&str> {
        match self {
            Self::Invoke { source } => source.guest_panic(),
            Self::PathNotUtf8 { .. }
            | Self::PathParentTraversal { .. }
            | Self::PathPrefix { .. }
            | Self::Refused { .. } => None,
        }
    }
}

impl From<ExecError> for ProgramError {
    fn from(error: ExecError) -> Self {
        Self::Refused {
            kind: error.kind,
            detail: error.detail,
        }
    }
}

pub async fn exec(
    client: &mut RpcClient,
    path: &str,
    args: &[String],
) -> Result<ExecResult, ProgramError> {
    let path = normalize_absolute(path)?;
    let outcome = debugger_programs::exec_path(&*client, &path, args)
        .await
        .map_err(|source| ProgramError::Invoke { source })?;
    outcome.map_err(ProgramError::from)
}

fn normalize_absolute(input: &str) -> Result<String, ProgramError> {
    let mut segments = Vec::new();
    for component in Path::new(input).components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(segment) => segments.push(
                segment
                    .to_str()
                    .ok_or_else(|| ProgramError::PathNotUtf8 {
                        input: input.to_owned(),
                    })?
                    .to_owned(),
            ),
            Component::ParentDir => {
                return Err(ProgramError::PathParentTraversal {
                    input: input.to_owned(),
                });
            }
            Component::Prefix(_) => {
                return Err(ProgramError::PathPrefix {
                    input: input.to_owned(),
                });
            }
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
