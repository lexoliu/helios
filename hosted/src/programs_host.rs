use helios_hal::resource::WasiRights;
use helios_kernel::{ProgramLaunchError, ProgramLaunchErrorKind};

use crate::init_program::StoreData;

fn convert_error_kind(
    kind: ProgramLaunchErrorKind,
) -> crate::program_bindings::bindings::helios::system::programs::LaunchErrorKind {
    match kind {
        ProgramLaunchErrorKind::InvalidBinary => {
            crate::program_bindings::bindings::helios::system::programs::LaunchErrorKind::InvalidBinary
        }
        ProgramLaunchErrorKind::MissingEntry => {
            crate::program_bindings::bindings::helios::system::programs::LaunchErrorKind::MissingEntry
        }
        ProgramLaunchErrorKind::UnsupportedImport => {
            crate::program_bindings::bindings::helios::system::programs::LaunchErrorKind::UnsupportedImport
        }
        ProgramLaunchErrorKind::QueueSaturated => {
            crate::program_bindings::bindings::helios::system::programs::LaunchErrorKind::QueueSaturated
        }
        ProgramLaunchErrorKind::Unavailable => {
            crate::program_bindings::bindings::helios::system::programs::LaunchErrorKind::Unavailable
        }
        ProgramLaunchErrorKind::Internal => {
            crate::program_bindings::bindings::helios::system::programs::LaunchErrorKind::Internal
        }
    }
}

fn convert_error_for_program(
    error: ProgramLaunchError,
) -> crate::program_bindings::bindings::helios::system::programs::LaunchError {
    crate::program_bindings::bindings::helios::system::programs::LaunchError {
        kind: convert_error_kind(error.kind),
        detail: error.detail,
    }
}

fn convert_error_for_debugger(
    error: ProgramLaunchError,
) -> crate::debugger_bindings::bindings::helios::system::programs::LaunchError {
    crate::debugger_bindings::bindings::helios::system::programs::LaunchError {
        kind: match error.kind {
            ProgramLaunchErrorKind::InvalidBinary => {
                crate::debugger_bindings::bindings::helios::system::programs::LaunchErrorKind::InvalidBinary
            }
            ProgramLaunchErrorKind::MissingEntry => {
                crate::debugger_bindings::bindings::helios::system::programs::LaunchErrorKind::MissingEntry
            }
            ProgramLaunchErrorKind::UnsupportedImport => {
                crate::debugger_bindings::bindings::helios::system::programs::LaunchErrorKind::UnsupportedImport
            }
            ProgramLaunchErrorKind::QueueSaturated => {
                crate::debugger_bindings::bindings::helios::system::programs::LaunchErrorKind::QueueSaturated
            }
            ProgramLaunchErrorKind::Unavailable => {
                crate::debugger_bindings::bindings::helios::system::programs::LaunchErrorKind::Unavailable
            }
            ProgramLaunchErrorKind::Internal => {
                crate::debugger_bindings::bindings::helios::system::programs::LaunchErrorKind::Internal
            }
        },
        detail: error.detail,
    }
}

macro_rules! impl_program_bindings {
    ($bindings:ident, $convert_error:ident) => {
        impl crate::$bindings::bindings::helios::system::programs::Host for StoreData {
            async fn launch(
                &mut self,
                request: crate::$bindings::bindings::helios::system::programs::LaunchRequest,
            ) -> wasmtime::Result<
                Result<
                    crate::$bindings::bindings::helios::system::programs::InstanceId,
                    crate::$bindings::bindings::helios::system::programs::LaunchError,
                >,
            > {
                let Some(program_service) = self.program_service.as_ref() else {
                    return Ok(Err($convert_error(ProgramLaunchError {
                        kind: ProgramLaunchErrorKind::Unavailable,
                        detail: "program launching is unavailable in this component context"
                            .to_owned(),
                    })));
                };
                Ok(
                    match program_service
                        .launch(request.name, &request.wasm, WasiRights::empty())
                        .await
                    {
                        Ok(instance_id) => Ok(instance_id.raw()),
                        Err(error) => Err($convert_error(error)),
                    },
                )
            }
        }
    };
}

impl_program_bindings!(program_bindings, convert_error_for_program);
impl_program_bindings!(debugger_bindings, convert_error_for_debugger);
