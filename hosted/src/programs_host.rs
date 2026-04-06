use helios_hal::resource::WasiRights;
use helios_kernel::{ProgramExecError, ProgramExecErrorKind};

use crate::init_program::StoreData;

fn convert_error_kind(
    kind: ProgramExecErrorKind,
) -> crate::program_bindings::bindings::helios::system::programs::ExecErrorKind {
    match kind {
        ProgramExecErrorKind::InvalidBinary => {
            crate::program_bindings::bindings::helios::system::programs::ExecErrorKind::InvalidBinary
        }
        ProgramExecErrorKind::MissingEntry => {
            crate::program_bindings::bindings::helios::system::programs::ExecErrorKind::MissingEntry
        }
        ProgramExecErrorKind::UnsupportedImport => {
            crate::program_bindings::bindings::helios::system::programs::ExecErrorKind::UnsupportedImport
        }
        ProgramExecErrorKind::QueueSaturated => {
            crate::program_bindings::bindings::helios::system::programs::ExecErrorKind::QueueSaturated
        }
        ProgramExecErrorKind::Unavailable => {
            crate::program_bindings::bindings::helios::system::programs::ExecErrorKind::Unavailable
        }
        ProgramExecErrorKind::Internal => {
            crate::program_bindings::bindings::helios::system::programs::ExecErrorKind::Internal
        }
    }
}

fn convert_error_for_program(
    error: ProgramExecError,
) -> crate::program_bindings::bindings::helios::system::programs::ExecError {
    crate::program_bindings::bindings::helios::system::programs::ExecError {
        kind: convert_error_kind(error.kind),
        detail: error.detail,
    }
}

fn convert_error_for_debugger(
    error: ProgramExecError,
) -> crate::debugger_bindings::bindings::helios::system::programs::ExecError {
    crate::debugger_bindings::bindings::helios::system::programs::ExecError {
        kind: match error.kind {
            ProgramExecErrorKind::InvalidBinary => {
                crate::debugger_bindings::bindings::helios::system::programs::ExecErrorKind::InvalidBinary
            }
            ProgramExecErrorKind::MissingEntry => {
                crate::debugger_bindings::bindings::helios::system::programs::ExecErrorKind::MissingEntry
            }
            ProgramExecErrorKind::UnsupportedImport => {
                crate::debugger_bindings::bindings::helios::system::programs::ExecErrorKind::UnsupportedImport
            }
            ProgramExecErrorKind::QueueSaturated => {
                crate::debugger_bindings::bindings::helios::system::programs::ExecErrorKind::QueueSaturated
            }
            ProgramExecErrorKind::Unavailable => {
                crate::debugger_bindings::bindings::helios::system::programs::ExecErrorKind::Unavailable
            }
            ProgramExecErrorKind::Internal => {
                crate::debugger_bindings::bindings::helios::system::programs::ExecErrorKind::Internal
            }
        },
        detail: error.detail,
    }
}

macro_rules! impl_program_bindings {
    ($bindings:ident, $convert_error:ident) => {
        impl crate::$bindings::bindings::helios::system::programs::Host for StoreData {
            async fn exec(
                &mut self,
                request: crate::$bindings::bindings::helios::system::programs::ExecRequest,
            ) -> wasmtime::Result<
                Result<
                    crate::$bindings::bindings::helios::system::programs::ExecResult,
                    crate::$bindings::bindings::helios::system::programs::ExecError,
                >,
            > {
                let Some(program_service) = self.program_service.as_ref() else {
                    return Ok(Err($convert_error(ProgramExecError {
                        kind: ProgramExecErrorKind::Unavailable,
                        detail: "program exec is unavailable in this component context"
                            .to_owned(),
                    })));
                };
                Ok(
                    match program_service
                        .exec(request.name, &request.wasm, WasiRights::empty())
                        .await
                    {
                        Ok(result) => Ok(crate::$bindings::bindings::helios::system::programs::ExecResult {
                            instance_id: result.instance_id.raw(),
                            exit_code: result.exit_code,
                            output: crate::$bindings::bindings::helios::system::programs::ExecOutput {
                                stdout: result.output.stdout,
                                stderr: result.output.stderr,
                            },
                        }),
                        Err(error) => Err($convert_error(error)),
                    },
                )
            }
        }
    };
}

impl_program_bindings!(program_bindings, convert_error_for_program);
impl_program_bindings!(debugger_bindings, convert_error_for_debugger);
