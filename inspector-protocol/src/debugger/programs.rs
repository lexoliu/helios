#[cfg(feature = "host")]
use crate::transport::Client;
use anyhow::Context as _;
#[cfg(all(feature = "host", not(feature = "guest")))]
use anyhow::Result;
#[cfg(feature = "guest")]
use anyhow::Result;
#[cfg(feature = "host")]
use futures_io::{AsyncRead, AsyncWrite};
#[cfg(feature = "guest")]
use helios_api::fs;
#[cfg(feature = "guest")]
use helios_api::programs as host_programs;
use serde::{Deserialize, Serialize};
#[cfg(feature = "guest")]
use std::path::Path;

pub use crate::system::programs::{ExecError, ExecErrorKind, ExecResult};

const PROGRAMS_INSTANCE: &str = "helios:debugger/programs@0.1.0";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExecPathRequest {
    path: String,
    args: Vec<String>,
}

impl ExecPathRequest {
    #[cfg(feature = "host")]
    pub(crate) fn new(path: &str, args: &[String]) -> Self {
        Self {
            path: path.to_owned(),
            args: args.to_vec(),
        }
    }
}

#[cfg(feature = "host")]
pub async fn exec_path<R, W>(
    client: &Client<R, W>,
    path: &str,
    args: &[String],
) -> Result<ExecResult>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let request = postcard::to_allocvec(&ExecPathRequest::new(path, args))
        .context("failed to encode debugger programs.exec-path request")?;
    let bytes = client
        .invoke_raw(PROGRAMS_INSTANCE, "exec-path", request)
        .await
        .context("failed to invoke debugger programs.exec-path")?;
    let response = postcard::from_bytes::<core::result::Result<ExecResult, ExecError>>(&bytes)
        .context("failed to decode debugger programs.exec-path response")?;
    response.map_err(|error| anyhow::anyhow!("{:?}: {}", error.kind, error.detail))
}

#[cfg(feature = "guest")]
pub(crate) fn supports(instance: &str, func: &str) -> bool {
    matches!((instance, func), (PROGRAMS_INSTANCE, "exec-path"))
}

#[cfg(feature = "guest")]
pub(crate) async fn dispatch(func: &str, payload: &[u8]) -> Result<Vec<u8>> {
    match func {
        "exec-path" => {
            let request = postcard::from_bytes::<ExecPathRequest>(payload)
                .context("failed to decode debugger programs.exec-path request payload")?;
            let name = infer_program_name(Path::new(&request.path))?;
            let wasm = fs::read(&request.path)
                .await
                .with_context(|| format!("failed to read executable {}", request.path))?;
            let response = host_programs::exec(&host_programs::ExecRequest {
                name,
                args: request.args,
                wasm,
            })
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
                detail: error.detail,
            });
            postcard::to_allocvec(&response)
                .context("failed to encode debugger programs.exec-path response")
        }
        _ => unreachable!("supports must reject unsupported debugger program requests"),
    }
}

#[cfg(feature = "guest")]
fn convert_exec_error_kind(kind: host_programs::ExecErrorKind) -> ExecErrorKind {
    match kind {
        host_programs::ExecErrorKind::InvalidBinary => ExecErrorKind::InvalidBinary,
        host_programs::ExecErrorKind::MissingEntry => ExecErrorKind::MissingEntry,
        host_programs::ExecErrorKind::UnsupportedImport => ExecErrorKind::UnsupportedImport,
        host_programs::ExecErrorKind::QueueSaturated => ExecErrorKind::QueueSaturated,
        host_programs::ExecErrorKind::Unavailable => ExecErrorKind::Unavailable,
        host_programs::ExecErrorKind::Internal => ExecErrorKind::Internal,
    }
}

#[cfg(feature = "guest")]
fn infer_program_name(path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("program path does not end with a valid utf-8 file name")?;
    if name.is_empty() {
        anyhow::bail!("program path does not name an executable")
    }
    Ok(name.strip_suffix(".wasm").unwrap_or(name).to_owned())
}
