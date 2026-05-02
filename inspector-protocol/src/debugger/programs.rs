use serde::{Deserialize, Serialize};

pub use crate::system::programs::{ExecError, ExecErrorKind, ExecResult};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExecPathRequest {
    path: String,
    args: Vec<String>,
}

#[cfg(feature = "host")]
mod host {
    use super::*;
    use crate::debugger::methods::{EXEC_PATH, PROGRAMS_INSTANCE};
    use crate::error::{RpcError, TransportError};
    use crate::transport::Client;
    use futures_io::{AsyncRead, AsyncWrite};

    impl ExecPathRequest {
        pub(crate) fn new(path: &str, args: &[String]) -> Self {
            Self {
                path: path.to_owned(),
                args: args.to_vec(),
            }
        }
    }

    /// Outcome of `programs.exec-path`: outer `Result` covers transport /
    /// encode / decode faults, inner `Result` carries the typed service
    /// error contract.
    pub type ExecPathOutcome = Result<Result<ExecResult, ExecError>, RpcError>;

    pub async fn exec_path<R, W>(
        client: &Client<R, W>,
        path: &str,
        args: &[String],
    ) -> ExecPathOutcome
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let request =
            postcard::to_allocvec(&ExecPathRequest::new(path, args)).map_err(|source| {
                RpcError::Encode {
                    instance: PROGRAMS_INSTANCE,
                    func: EXEC_PATH,
                    source,
                }
            })?;
        let bytes = client
            .invoke_raw(PROGRAMS_INSTANCE, EXEC_PATH, request)
            .await
            .map_err(|source: TransportError| RpcError::Invoke {
                instance: PROGRAMS_INSTANCE,
                func: EXEC_PATH,
                source,
            })?;
        postcard::from_bytes::<Result<ExecResult, ExecError>>(&bytes).map_err(|source| {
            RpcError::Decode {
                instance: PROGRAMS_INSTANCE,
                func: EXEC_PATH,
                source,
            }
        })
    }
}

#[cfg(feature = "host")]
pub use host::*;

#[cfg(feature = "guest")]
mod guest {
    use super::*;
    use crate::debugger::methods::{EXEC_PATH, PROGRAMS_INSTANCE};
    use crate::error::DispatchError;
    use helios_api::programs as host_programs;
    use std::path::{Path, PathBuf};

    pub(crate) fn supports(instance: &str, func: &str) -> bool {
        matches!((instance, func), (PROGRAMS_INSTANCE, EXEC_PATH))
    }

    pub(crate) async fn dispatch(func: &str, payload: &[u8]) -> Result<Vec<u8>, DispatchError> {
        match func {
            EXEC_PATH => {
                let request =
                    postcard::from_bytes::<ExecPathRequest>(payload).map_err(|source| {
                        DispatchError::Decode {
                            operation: "debugger programs.exec-path",
                            source,
                        }
                    })?;
                let path = validate_program_path(Path::new(&request.path))?;
                let response = host_programs::exec(host_programs::ExecRequest {
                    name: path.to_string_lossy().into_owned(),
                    args: request.args,
                    env: Vec::new(),
                    path: request.path,
                    stdin: Vec::new(),
                    hint: None,
                    capability_grants: vec![
                        host_programs::root_directory_grant(),
                        host_programs::root_link_grant(),
                        host_programs::network_grant(vec![
                            host_programs::NetworkRight::Tcp,
                            host_programs::NetworkRight::Udp,
                            host_programs::NetworkRight::Dns,
                            host_programs::NetworkRight::PrivilegedBind,
                            host_programs::NetworkRight::Multicast,
                        ]),
                        host_programs::root_terminal_grant(),
                        host_programs::root_process_grant(),
                    ],
                })
                .await
                .map(|result| ExecResult {
                    instance_id: result.instance_id,
                    exit_code: result.exit_code,
                    output: crate::system::programs::ExecOutput {
                        stdout: result.output.stdout,
                        stderr: result.output.stderr,
                    },
                })
                .map_err(|error| ExecError {
                    kind: convert_exec_error_kind(error.kind),
                    detail: error.detail.to_string(),
                });
                postcard::to_allocvec(&response).map_err(|source| DispatchError::Encode {
                    operation: "debugger programs.exec-path",
                    source,
                })
            }
            _ => unreachable!("supports must reject unsupported debugger program requests"),
        }
    }

    fn convert_exec_error_kind(kind: host_programs::ExecErrorKind) -> ExecErrorKind {
        match kind {
            host_programs::ExecErrorKind::InvalidBinary => ExecErrorKind::InvalidBinary,
            host_programs::ExecErrorKind::MissingEntry => ExecErrorKind::MissingEntry,
            host_programs::ExecErrorKind::UnsupportedImport => ExecErrorKind::UnsupportedImport,
            host_programs::ExecErrorKind::InvalidSignature => ExecErrorKind::InvalidSignature,
            host_programs::ExecErrorKind::InvalidPath => ExecErrorKind::InvalidPath,
            host_programs::ExecErrorKind::PermissionDenied => ExecErrorKind::PermissionDenied,
            host_programs::ExecErrorKind::InvalidHint => ExecErrorKind::InvalidHint,
            host_programs::ExecErrorKind::OutOfMemory => ExecErrorKind::OutOfMemory,
            host_programs::ExecErrorKind::Unavailable => ExecErrorKind::Unavailable,
            host_programs::ExecErrorKind::Internal => ExecErrorKind::Internal,
        }
    }

    fn validate_program_path(path: &Path) -> Result<PathBuf, DispatchError> {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return Err(DispatchError::protocol(
                "program path does not end with a valid utf-8 file name",
            ));
        };
        if name.is_empty() {
            return Err(DispatchError::protocol(
                "program path does not name an executable",
            ));
        }
        Ok(path.to_path_buf())
    }
}

#[cfg(feature = "guest")]
pub(crate) use guest::{dispatch, supports};
