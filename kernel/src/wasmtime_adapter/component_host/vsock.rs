//! `helios:system/vsock` for the component host.
//!
//! The two worlds the kernel serves — `init` for ordinary programs and
//! `debugger` for the system component — generate their own Rust types
//! for the same WIT interface, so the registration is written once and
//! parameterised over a [`VsockBindings`] marker that names each world's
//! value types. The alternative the network interface took, one copy of
//! every host function per world, is exactly the duplication this
//! avoids.
//!
//! Capability: vsock is the machine's link to whatever is hosting it,
//! and the capability that says a component may talk to its host
//! controller is the same one that hands out the debug serial port. A
//! component without it gets `permission-denied` from every entry point
//! rather than a device that quietly is not there.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use helios_hal::cpu::Cpu;
use helios_hal::vsock::{VsockAddress, VsockShutdown};
use wasmtime::component::{Accessor, ComponentType, Lift, Linker, Lower, Resource, ResourceType};

use crate::wasmtime_adapter::bindings::debugger::bindings as debugger_bindings;
use crate::wasmtime_adapter::bindings::program::bindings as program_bindings;
use crate::{ComponentHostVsockService, VsockError, VsockListenerId, VsockStreamId};

use super::StoreData;

pub(super) const VSOCK_INSTANCE: &str = "helios:system/vsock@0.1.0";

/// The kernel-side view of an open `vsock-stream` resource.
pub struct ComponentVsockStream {
    pub service: ComponentHostVsockService,
    pub stream: VsockStreamId,
}

/// The kernel-side view of a bound `vsock-listener` resource.
pub struct ComponentVsockListener {
    pub service: ComponentHostVsockService,
    pub listener: VsockListenerId,
}

/// One world's generated types for `helios:system/vsock`.
///
/// Implementors are zero-sized markers; everything they carry is in the
/// associated types and the two constructors that build them.
pub(super) trait VsockBindings: Send + Sync + 'static {
    type Error: ComponentType + Lower + Send + Sync + 'static;
    type Address: ComponentType + Lower + Lift + Send + Sync + 'static;

    fn error(kind: VsockErrorKind, detail: String) -> Self::Error;
    fn address(address: VsockAddress) -> Self::Address;
    fn to_address(address: &Self::Address) -> VsockAddress;
}

/// The error kinds the WIT interface names, independent of which world's
/// enum they are lowered into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum VsockErrorKind {
    Unavailable,
    AddressInUse,
    ConnectionRefused,
    ConnectionReset,
    Closed,
    Timeout,
    PermissionDenied,
    Internal,
}

impl VsockErrorKind {
    /// Which kind a kernel-side error reports as.
    fn of(error: VsockError) -> Self {
        match error {
            VsockError::Unavailable => Self::Unavailable,
            VsockError::PortInUse { .. }
            | VsockError::NoEphemeralPort
            | VsockError::ListenerTableFull
            | VsockError::ConnectionTableFull => Self::AddressInUse,
            VsockError::ConnectionRefused => Self::ConnectionRefused,
            VsockError::ConnectionReset => Self::ConnectionReset,
            VsockError::Closed | VsockError::UnknownHandle => Self::Closed,
            VsockError::Timeout => Self::Timeout,
            VsockError::Device(_) => Self::Internal,
        }
    }
}

/// Lowers a kernel error into a world's error record, keeping the
/// kernel's own message as the detail so provenance survives the
/// boundary.
fn convert_error<Bindings: VsockBindings>(error: VsockError) -> Bindings::Error {
    Bindings::error(VsockErrorKind::of(error), error.to_string())
}

fn unavailable<Bindings: VsockBindings>() -> Bindings::Error {
    Bindings::error(
        VsockErrorKind::Unavailable,
        "this machine has no vsock device".to_string(),
    )
}

fn denied<Bindings: VsockBindings>() -> Bindings::Error {
    Bindings::error(
        VsockErrorKind::PermissionDenied,
        "this component does not hold the host-link capability".to_string(),
    )
}

/// Marker for the `debugger` world's generated vsock types.
pub(super) struct DebuggerVsock;

impl VsockBindings for DebuggerVsock {
    type Error = debugger_bindings::helios::system::vsock::VsockError;
    type Address = debugger_bindings::helios::system::vsock::VsockAddress;

    fn error(kind: VsockErrorKind, detail: String) -> Self::Error {
        use debugger_bindings::helios::system::vsock::VsockErrorKind as Wit;
        Self::Error {
            kind: match kind {
                VsockErrorKind::Unavailable => Wit::Unavailable,
                VsockErrorKind::AddressInUse => Wit::AddressInUse,
                VsockErrorKind::ConnectionRefused => Wit::ConnectionRefused,
                VsockErrorKind::ConnectionReset => Wit::ConnectionReset,
                VsockErrorKind::Closed => Wit::Closed,
                VsockErrorKind::Timeout => Wit::Timeout,
                VsockErrorKind::PermissionDenied => Wit::PermissionDenied,
                VsockErrorKind::Internal => Wit::Internal,
            },
            detail,
        }
    }

    fn address(address: VsockAddress) -> Self::Address {
        Self::Address {
            cid: address.cid,
            port: address.port,
        }
    }

    fn to_address(address: &Self::Address) -> VsockAddress {
        VsockAddress::new(address.cid, address.port)
    }
}

/// Marker for the `init` world's generated vsock types.
pub(super) struct ProgramVsock;

impl VsockBindings for ProgramVsock {
    type Error = program_bindings::helios::system::vsock::VsockError;
    type Address = program_bindings::helios::system::vsock::VsockAddress;

    fn error(kind: VsockErrorKind, detail: String) -> Self::Error {
        use program_bindings::helios::system::vsock::VsockErrorKind as Wit;
        Self::Error {
            kind: match kind {
                VsockErrorKind::Unavailable => Wit::Unavailable,
                VsockErrorKind::AddressInUse => Wit::AddressInUse,
                VsockErrorKind::ConnectionRefused => Wit::ConnectionRefused,
                VsockErrorKind::ConnectionReset => Wit::ConnectionReset,
                VsockErrorKind::Closed => Wit::Closed,
                VsockErrorKind::Timeout => Wit::Timeout,
                VsockErrorKind::PermissionDenied => Wit::PermissionDenied,
                VsockErrorKind::Internal => Wit::Internal,
            },
            detail,
        }
    }

    fn address(address: VsockAddress) -> Self::Address {
        Self::Address {
            cid: address.cid,
            port: address.port,
        }
    }

    fn to_address(address: &Self::Address) -> VsockAddress {
        VsockAddress::new(address.cid, address.port)
    }
}

/// The service a call may use, once the caller's capability and the
/// machine's device have both been checked.
type Authorised<Bindings> = Result<ComponentHostVsockService, <Bindings as VsockBindings>::Error>;

fn authorise<Bindings, CpuImpl, HostFs>(store: &StoreData<CpuImpl, HostFs>) -> Authorised<Bindings>
where
    Bindings: VsockBindings,
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if store.debug_port().is_none() {
        return Err(denied::<Bindings>());
    }
    store
        .runtime_state
        .vsock_service()
        .ok_or_else(unavailable::<Bindings>)
}

pub(super) fn add_vsock_to_linker<Bindings, CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> wasmtime::Result<()>
where
    Bindings: VsockBindings,
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let mut instance = linker.instance(VSOCK_INSTANCE)?;
    instance.resource_concurrent(
        "vsock-stream",
        ResourceType::host::<ComponentVsockStream>(),
        |accessor, rep| {
            Box::pin(async move {
                let stream = accessor.with(|mut access| {
                    let resource = Resource::<ComponentVsockStream>::new_own(rep);
                    access.get().table.delete(resource)
                })?;
                // A dropped handle is a closed connection; the peer is
                // told rather than left waiting on a stream nothing will
                // ever read again.
                if let Err(error) = stream.service.close(stream.stream).await {
                    tracing::debug!(%error, "dropped vsock stream could not be closed cleanly");
                }
                Ok::<_, wasmtime::Error>(())
            })
        },
    )?;
    instance.resource_concurrent(
        "vsock-listener",
        ResourceType::host::<ComponentVsockListener>(),
        |accessor, rep| {
            Box::pin(async move {
                let listener = accessor.with(|mut access| {
                    let resource = Resource::<ComponentVsockListener>::new_own(rep);
                    access.get().table.delete(resource)
                })?;
                if let Err(error) = listener.service.close_listener(listener.listener) {
                    tracing::debug!(%error, "dropped vsock listener was already gone");
                }
                Ok::<_, wasmtime::Error>(())
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "guest-cid",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>, (): ()| {
            Box::pin(async move {
                let cid = accessor.with(|mut access| {
                    let store = access.get();
                    Ok::<_, wasmtime::Error>(
                        (store.debug_port().is_some())
                            .then(|| store.runtime_state.vsock_service())
                            .flatten()
                            .map(|service| service.guest_cid()),
                    )
                })?;
                Ok::<_, wasmtime::Error>((cid,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "listen",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>, (port, backlog): (u32, u32)| {
            Box::pin(async move {
                let response = accessor.with(|mut access| {
                    let service = match authorise::<Bindings, _, _>(access.get()) {
                        Ok(service) => service,
                        Err(error) => return Ok::<_, wasmtime::Error>(Err(error)),
                    };
                    let listener = match service.listen(port, backlog as usize) {
                        Ok(listener) => listener,
                        Err(error) => {
                            return Ok::<_, wasmtime::Error>(Err(convert_error::<Bindings>(error)));
                        }
                    };
                    Ok(Ok(access
                        .get()
                        .table
                        .push(ComponentVsockListener { service, listener })?))
                })?;
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "connect",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (address, timeout): (Bindings::Address, u64)| {
            Box::pin(async move {
                let peer = Bindings::to_address(&address);
                let service = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(authorise::<Bindings, _, _>(access.get()))
                })?;
                let service = match service {
                    Ok(service) => service,
                    Err(error) => return Ok::<_, wasmtime::Error>((Err(error),)),
                };
                let response = match service.connect(peer, timeout).await {
                    Ok(stream) => {
                        let pushed = accessor.with(|mut access| {
                            access.get().table.push(ComponentVsockStream {
                                service: service.clone(),
                                stream,
                            })
                        })?;
                        Ok(pushed)
                    }
                    Err(error) => Err(convert_error::<Bindings>(error)),
                };
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]vsock-listener.port",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource,): (Resource<ComponentVsockListener>,)| {
            Box::pin(async move {
                let response = accessor.with(|mut access| {
                    let listener = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>(
                        listener
                            .service
                            .listener_port(listener.listener)
                            .map_err(convert_error::<Bindings>),
                    )
                })?;
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]vsock-listener.accept",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource, timeout): (Resource<ComponentVsockListener>, u64)| {
            Box::pin(async move {
                let listener = accessor.with(|mut access| {
                    let listener = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((listener.service.clone(), listener.listener))
                })?;
                tracing::info!(timeout, "vsock listener accept requested");
                let accepted = listener.0.accept(listener.1, timeout).await;
                tracing::info!(ok = accepted.is_ok(), "vsock listener accept returned");
                let response = match accepted {
                    Ok(stream) => {
                        let pushed = accessor.with(|mut access| {
                            access.get().table.push(ComponentVsockStream {
                                service: listener.0.clone(),
                                stream,
                            })
                        })?;
                        Ok(pushed)
                    }
                    Err(error) => Err(convert_error::<Bindings>(error)),
                };
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]vsock-listener.close",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource,): (Resource<ComponentVsockListener>,)| {
            Box::pin(async move {
                let response = accessor.with(|mut access| {
                    let listener = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>(
                        listener
                            .service
                            .close_listener(listener.listener)
                            .map_err(convert_error::<Bindings>),
                    )
                })?;
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]vsock-stream.peer",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource,): (Resource<ComponentVsockStream>,)| {
            Box::pin(async move {
                let response = accessor.with(|mut access| {
                    let stream = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>(
                        stream
                            .service
                            .peer(stream.stream)
                            .map(Bindings::address)
                            .map_err(convert_error::<Bindings>),
                    )
                })?;
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]vsock-stream.read",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource, max_bytes, timeout): (Resource<ComponentVsockStream>, u32, u64)| {
            Box::pin(async move {
                let stream = accessor.with(|mut access| {
                    let stream = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((stream.service.clone(), stream.stream))
                })?;
                tracing::info!(max_bytes, timeout, "vsock stream read requested");
                let response = stream
                    .0
                    .read(stream.1, max_bytes as usize, timeout)
                    .await
                    .map_err(convert_error::<Bindings>);
                match &response {
                    Ok(Some(bytes)) => {
                        tracing::info!(len = bytes.len(), "vsock stream read returned");
                    }
                    Ok(None) => tracing::info!("vsock stream read reached end of file"),
                    Err(_) => tracing::info!("vsock stream read failed"),
                }
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]vsock-stream.write",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource, bytes, timeout): (Resource<ComponentVsockStream>, Vec<u8>, u64)| {
            Box::pin(async move {
                let stream = accessor.with(|mut access| {
                    let stream = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((stream.service.clone(), stream.stream))
                })?;
                tracing::info!(len = bytes.len(), timeout, "vsock stream write requested");
                let response = stream
                    .0
                    .write(stream.1, &bytes, timeout)
                    .await
                    .map(|written| written as u64)
                    .map_err(convert_error::<Bindings>);
                tracing::info!(
                    written = response.as_ref().ok().copied(),
                    "vsock stream write returned"
                );
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]vsock-stream.shutdown-send",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource,): (Resource<ComponentVsockStream>,)| {
            Box::pin(async move {
                let stream = accessor.with(|mut access| {
                    let stream = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((stream.service.clone(), stream.stream))
                })?;
                let response = stream
                    .0
                    .shutdown(
                        stream.1,
                        VsockShutdown {
                            receive: false,
                            send: true,
                        },
                    )
                    .await
                    .map_err(convert_error::<Bindings>);
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]vsock-stream.close",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource,): (Resource<ComponentVsockStream>,)| {
            Box::pin(async move {
                let stream = accessor.with(|mut access| {
                    let stream = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((stream.service.clone(), stream.stream))
                })?;
                let response = stream
                    .0
                    .close(stream.1)
                    .await
                    .map_err(convert_error::<Bindings>);
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    Ok(())
}
