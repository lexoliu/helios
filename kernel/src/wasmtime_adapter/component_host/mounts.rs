//! `helios:system/mounts` for the component host.
//!
//! A request runs under the caller's process authority: the kernel turns
//! it into a mount capability ([`crate::MountCap`]) before it touches the
//! mount table, so a program without the mount right, or without
//! mutate-directory authority over the point, changes nothing.
//!
//! # Concurrency contract
//!
//! The store hands over the caller's authority, its namespace and the
//! host-share client, then is released; the mount itself runs under the
//! mount table's own contract (a writer lock held to publish a slot, and
//! an unmount that yields until the slot's readers drain).

use alloc::format;
use alloc::string::String;

use helios_hal::cpu::Cpu;
use helios_hal::fs::BlockSerial;
use wasmtime::component::{Accessor, HasSelf, Linker};

use super::{StoreData, debugger_bindings, program_bindings};
use crate::{ComponentHostNetwork, ErofsError, MountError, MountRequestError, MountSource};

/// The kind of refusal the guest sees, and the detail it reads.
fn refusal(error: &MountRequestError) -> (RefusalKind, String) {
    let kind = match error {
        MountRequestError::Authority(_) => RefusalKind::PermissionDenied,
        MountRequestError::HostShareUnavailable
        | MountRequestError::InvalidSerial(_)
        | MountRequestError::DiskUnavailable { .. } => RefusalKind::Unavailable,
        MountRequestError::Erofs {
            error: ErofsError::Device(_),
            ..
        } => RefusalKind::Io,
        MountRequestError::Erofs {
            error: ErofsError::Checksum { .. } | ErofsError::Format(_),
            ..
        } => RefusalKind::InvalidFilesystem,
        MountRequestError::Mount(MountError::InvalidPoint(_) | MountError::RootKind) => {
            RefusalKind::InvalidPoint
        }
        MountRequestError::Mount(MountError::AlreadyMounted(_)) => RefusalKind::AlreadyMounted,
        MountRequestError::Mount(MountError::NotMounted(_)) => RefusalKind::NotMounted,
        MountRequestError::Mount(MountError::Busy(_)) => RefusalKind::Busy,
        MountRequestError::Mount(MountError::TableFull) => RefusalKind::TableFull,
        MountRequestError::Mount(MountError::Filesystem { .. }) => RefusalKind::InvalidPath,
    };
    (kind, format!("{error}"))
}

enum RefusalKind {
    PermissionDenied,
    InvalidPoint,
    AlreadyMounted,
    NotMounted,
    Busy,
    TableFull,
    Unavailable,
    InvalidPath,
    InvalidFilesystem,
    Io,
}

/// The source a request names, or the refusal of a disk serial no disk
/// can report.
fn disk_source(serial: String) -> Result<MountSource, MountRequestError> {
    BlockSerial::new(serial.as_bytes())
        .map(|serial| MountSource::Disk { serial })
        .ok_or(MountRequestError::InvalidSerial(serial))
}

macro_rules! impl_mount_bindings {
    ($bindings:ident, $module:ident, $add_to_linker:ident) => {
        mod $module {
            use super::*;
            use $bindings::helios::system::mounts as wit;

            fn mount_error(error: &MountRequestError) -> wit::MountError {
                let (kind, detail) = refusal(error);
                wit::MountError {
                    kind: match kind {
                        RefusalKind::PermissionDenied => wit::MountErrorKind::PermissionDenied,
                        RefusalKind::InvalidPoint => wit::MountErrorKind::InvalidPoint,
                        RefusalKind::AlreadyMounted => wit::MountErrorKind::AlreadyMounted,
                        RefusalKind::NotMounted => wit::MountErrorKind::NotMounted,
                        RefusalKind::Busy => wit::MountErrorKind::Busy,
                        RefusalKind::TableFull => wit::MountErrorKind::TableFull,
                        RefusalKind::Unavailable => wit::MountErrorKind::Unavailable,
                        RefusalKind::InvalidPath => wit::MountErrorKind::InvalidPath,
                        RefusalKind::InvalidFilesystem => wit::MountErrorKind::InvalidFilesystem,
                        RefusalKind::Io => wit::MountErrorKind::Io,
                    },
                    detail,
                }
            }

            fn mount_source(source: wit::MountSource) -> Result<MountSource, MountRequestError> {
                Ok(match source {
                    wit::MountSource::Memory => MountSource::Memory,
                    wit::MountSource::BootImage => MountSource::BootImage,
                    wit::MountSource::HostShare => MountSource::HostShare,
                    wit::MountSource::Disk(serial) => return disk_source(serial),
                })
            }

            impl<CpuImpl, Net, StorageImpl> wit::Host for StoreData<CpuImpl, Net, StorageImpl>
            where
                CpuImpl: Cpu + Clone,
                Net: ComponentHostNetwork,
                StorageImpl: crate::Storage,
            {
            }

            impl<CpuImpl, Net, StorageImpl, U> wit::HostWithStore<U>
                for HasSelf<StoreData<CpuImpl, Net, StorageImpl>>
            where
                CpuImpl: Cpu + Clone,
                Net: ComponentHostNetwork,
                StorageImpl: crate::Storage,
            {
                async fn mount(
                    accessor: &Accessor<U, Self>,
                    request: wit::MountRequest,
                ) -> wasmtime::Result<Result<(), wit::MountError>> {
                    let (authority, namespace, attached) = accessor.with(|mut access| {
                        let store = access.get();
                        (
                            store.process_authority().clone(),
                            store.filesystem().clone(),
                            store.runtime_state.attached_storage(),
                        )
                    });
                    let outcome = match mount_source(request.source) {
                        Ok(source) => {
                            crate::request_mount(
                                &authority,
                                &namespace,
                                attached,
                                source,
                                &request.path,
                                request.read_only,
                            )
                            .await
                        }
                        Err(error) => Err(error),
                    };
                    Ok(outcome.map_err(|error| {
                        tracing::debug!(%error, path = %request.path, "mount refused");
                        mount_error(&error)
                    }))
                }

                async fn unmount(
                    accessor: &Accessor<U, Self>,
                    path: String,
                ) -> wasmtime::Result<Result<(), wit::MountError>> {
                    let (authority, namespace) = accessor.with(|mut access| {
                        let store = access.get();
                        (
                            store.process_authority().clone(),
                            store.filesystem().clone(),
                        )
                    });
                    let outcome = crate::request_unmount(&authority, &namespace, &path).await;
                    Ok(outcome.map_err(|error| {
                        tracing::debug!(%error, path = %path, "unmount refused");
                        mount_error(&error)
                    }))
                }
            }

            pub(in crate::wasmtime_adapter::component_host) fn add_to_linker<
                CpuImpl,
                Net,
                StorageImpl,
            >(
                linker: &mut Linker<StoreData<CpuImpl, Net, StorageImpl>>,
            ) -> wasmtime::Result<()>
            where
                CpuImpl: Cpu + Clone,
                Net: ComponentHostNetwork,
                StorageImpl: crate::Storage,
            {
                wit::add_to_linker::<_, HasSelf<StoreData<CpuImpl, Net, StorageImpl>>>(
                    linker,
                    |state| state,
                )
            }
        }

        pub(super) use $module::add_to_linker as $add_to_linker;
    };
}

impl_mount_bindings!(debugger_bindings, debugger_mounts, add_mounts_to_linker);
impl_mount_bindings!(
    program_bindings,
    program_mounts,
    add_mounts_to_program_linker
);
