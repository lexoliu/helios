extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Context, Poll};
use core::time::Duration;

use crate::wasmtime_adapter::wasi::net::ipv6_address_groups;

use bytes::Bytes;

use crate::{
    ClockAuthorityRights, DirectoryAuthorityRights, LinkAuthorityRights, NetworkAuthorityRights,
    ProcessAuthority, ProcessAuthorityError, TerminalAuthorityRights,
};
use crate::{
    ComponentCache, ComponentNetworkService, ComponentOutputMode, ComponentOutputRoute,
    ComponentOutputStreamKind, ComponentStoreData, DeadlinePollable, EmbeddedComponent, ExecResult,
    ProgramExecError, ProgramExecErrorDetail, ProgramExecErrorKind, RawMutex,
    RawMutexGuardResource, RawMutexResource, RawRwLock, RawRwLockReadGuardResource,
    RawRwLockResource, RawRwLockWriteGuardResource, SerialPortResource, elapsed_millis,
    emit_serial_stage_marker, heap_stats, largest_servable_user_bytes, monotonic_nanos,
    user_heap_stats,
};
use helios_hal::cpu::Cpu;
use helios_hal::serial::ByteSerial;
use spin::Mutex;
use thiserror::Error;
use wasmtime::component::{
    Access, Accessor, Component, Destination, FutureReader, HasSelf, Linker, Resource,
    ResourceTable, ResourceType, StreamProducer, StreamReader, StreamResult,
};
use wasmtime::{self, Engine, Store, StoreContextMut};

use crate::runtime::ComponentHostFilesystemState;
use crate::wasmtime_adapter::bindings::debugger::bindings as debugger_bindings;
use crate::wasmtime_adapter::bindings::program::bindings as program_bindings;
use crate::wasmtime_adapter::config::AotCompileHint;
use crate::wasmtime_adapter::cwasm::{self, ArtifactTrustError, UntrustedCwasm};
use crate::wasmtime_adapter::wasi::ChannelStreamProducer;
use crate::wasmtime_adapter::wasi::bindings::filesystem::types::ErrorCode as FsErrorCode;
use crate::{
    HeapStats, PerfMetricFilter, PerfSample, ProfileFilter, ProfileScope, StatsSample, TraceEvent,
    TraceField, TraceFilter, TraceLevel, TraceValue,
};

const SYNC_INSTANCE: &str = "helios:system/sync@0.1.0";
const STATS_INSTANCE: &str = "helios:system/stats@0.1.0";
const NET_INSTANCE: &str = "helios:system/net@0.1.0";
const TRACING_INSTANCE: &str = "helios:system/tracing@0.1.0";
const PROFILING_INSTANCE: &str = "helios:system/profiling@0.1.0";
const INSTANCES_INSTANCE: &str = "helios:system/instances@0.1.0";
const COMPONENT_CACHE_FRACTION: usize = 8;
const COMPONENT_PHASE_HEARTBEAT_INTERVAL_NANOS: u64 = 5_000_000_000;

fn lower_bytes_to_vec(bytes: Bytes) -> Vec<u8> {
    // `Bytes::to_vec` always copies. The owned conversion can reuse unique
    // receive buffers, which matters in the profiled component-host TCP read
    // lowering path (`program-net-tcp-read-lower-bytes`).
    Vec::from(bytes)
}

mod network;
pub mod service;
mod topology;
mod vsock;

struct SerialFmtWriter {
    write_serial: fn(&[u8]),
}

impl Write for SerialFmtWriter {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        (self.write_serial)(text.as_bytes());
        Ok(())
    }
}

fn write_serial_fmt(write_serial: fn(&[u8]), arguments: fmt::Arguments<'_>) {
    let mut writer = SerialFmtWriter { write_serial };
    // `write_fmt` hands the sink one fragment per format piece, so the gate
    // spans the whole message rather than each fragment.
    crate::io::emit_console_line(|| {
        writer
            .write_fmt(arguments)
            .expect("serial formatting should not fail");
    });
}

pub use network::{
    ComponentHostNetworkService, ComponentHostTcpListenerToken, ComponentHostTcpStreamToken,
    ComponentHostUdpSocketToken,
};
pub use service::{
    ChildExit, ChildHandle, UserProgramService, install_component_host_program_service,
    install_program_service, run_component_host_processor_forever, run_embedded_component_forever,
    run_program_workers_forever,
};
pub(crate) use service::{ProgramArgv, ProgramExecContext, ProgramSource};
pub use topology::{
    ComponentHostProcessorRole, component_host_kernel_processor_count,
    component_host_processor_role, component_host_processors_to_start,
    component_host_system_processor, component_host_worker_count, system_component_should_run_on,
};

pub type SbiSerialPort = crate::ComponentSerialPort;

pub use vsock::{ComponentVsockListener, ComponentVsockStream};

pub type NetworkTcpBackend = crate::ComponentTcpBackend<ComponentHostNetworkService>;
pub type NetworkUdpBackend = crate::ComponentUdpBackend<ComponentHostNetworkService>;
pub type SbiTcpStream = crate::ComponentTcpStream<NetworkTcpBackend>;
pub type SbiUdpSocket = crate::ComponentUdpSocket<NetworkUdpBackend>;
pub type HostRuntimeState<CpuImpl, HostFs> =
    crate::RuntimeState<UserProgramService<CpuImpl, HostFs>, ComponentHostNetworkService, HostFs>;
pub type StoreData<CpuImpl, HostFs> = ComponentStoreData<
    CpuImpl,
    HostRuntimeState<CpuImpl, HostFs>,
    crate::wasmtime_adapter::wasi::DebugFileSystem<HostRuntimeState<CpuImpl, HostFs>, HostFs>,
    ResourceTable,
>;
pub type OutputMode = ComponentOutputMode;
pub type OutputRoute = ComponentOutputRoute;
pub type OutputStreamKind = ComponentOutputStreamKind;
pub type RuntimeDeadlinePollable<CpuImpl, HostFs> =
    DeadlinePollable<CpuImpl, HostRuntimeState<CpuImpl, HostFs>>;

/// A long-running bring-up phase the heartbeat reports on.
///
/// The name, its start, and the flag that ends it are one description
/// of the same phase, so they travel together.
pub(super) struct ComponentPhase<'a> {
    pub(super) name: &'static str,
    pub(super) started_at: u64,
    pub(super) done: &'a Arc<AtomicBool>,
}

fn spawn_component_phase_heartbeat<CpuImpl>(
    spawner: &crate::InstanceSpawner<CpuImpl>,
    cpu: &CpuImpl,
    timer: &crate::Timer<CpuImpl>,
    progress: &helios_hal::watchdog::ProgressCounter,
    write_serial: fn(&[u8]),
    phase: ComponentPhase<'_>,
) -> Result<(), crate::TaskCapacityError>
where
    CpuImpl: Cpu + Clone,
{
    let ComponentPhase {
        name: phase,
        started_at,
        done,
    } = phase;
    spawner.try_spawn_detached({
        let done = done.clone();
        let cpu = cpu.clone();
        let timer = timer.clone();
        let progress = progress.clone();
        async move {
            let interval = Duration::from_nanos(COMPONENT_PHASE_HEARTBEAT_INTERVAL_NANOS);
            loop {
                if done.load(Ordering::Acquire) {
                    return;
                }

                timer.sleep_for(interval).await;
                if done.load(Ordering::Acquire) {
                    return;
                }

                let now = monotonic_nanos(&cpu);
                progress.record_progress();
                if cfg!(debug_assertions) {
                    let elapsed_ms = elapsed_millis(started_at, now);
                    write_serial_fmt(
                        write_serial,
                        format_args!("\n[KDBG {phase}-progress elapsed_ms={elapsed_ms}]\n"),
                    );
                }
            }
        }
    })
}

#[derive(Clone, Copy)]
pub enum ComponentBindingSet {
    System,
    Program,
}
pub type SbiRawMutex = crate::ComponentRawMutex;
pub type SbiRawMutexGuard = crate::ComponentRawMutexGuard;
pub type SbiRawRwLock = crate::ComponentRawRwLock;
pub type SbiRawRwLockReadGuard = crate::ComponentRawRwLockReadGuard;
pub type SbiRawRwLockWriteGuard = crate::ComponentRawRwLockWriteGuard;

macro_rules! impl_program_bindings {
    ($bindings:ident, $convert_result:ident, $convert_error:ident, $build_authority:ident) => {
        impl<CpuImpl, HostFs> $bindings::helios::system::programs::Host
            for StoreData<CpuImpl, HostFs>
        where
            CpuImpl: Cpu + Clone,
            HostFs: crate::HostFileSystem,
        {
        }

        impl<CpuImpl, HostFs> $bindings::helios::system::programs::HostChild
            for StoreData<CpuImpl, HostFs>
        where
            CpuImpl: Cpu + Clone,
            HostFs: crate::HostFileSystem,
        {
        }

        impl<CpuImpl, HostFs, U> $bindings::helios::system::programs::HostChildWithStore<U>
            for HasSelf<StoreData<CpuImpl, HostFs>>
        where
            CpuImpl: Cpu + Clone,
            HostFs: crate::HostFileSystem,
        {
            async fn drop(
                accessor: &Accessor<U, Self>,
                child: wasmtime::component::Resource<ChildHandle>,
            ) -> wasmtime::Result<()> {
                accessor.with(|mut access| {
                    let _ = access.get().table.delete(child)?;
                    Ok::<_, wasmtime::Error>(())
                })?;
                Ok(())
            }

            async fn wait(
                accessor: &Accessor<U, Self>,
                child: wasmtime::component::Resource<ChildHandle>,
            ) -> wasmtime::Result<
                Result<
                    $bindings::helios::system::programs::ExitStatus,
                    $bindings::helios::system::programs::SpawnError,
                >,
            > {
                // Per WIT, `wait` borrows the child; we must not remove
                // it from the table here — the guest's `Drop` impl does
                // that separately once it releases the handle.
                let wait_future = accessor.with(|mut access| {
                    let handle = access
                        .get()
                        .table
                        .get_mut(&child)
                        .map_err(wasmtime::Error::from)?;
                    Ok::<_, wasmtime::Error>(handle.take_wait())
                })?;
                let Some(wait_future) = wait_future else {
                    return Ok(Err($bindings::helios::system::programs::SpawnError {
                        kind: $bindings::helios::system::programs::SpawnErrorKind::Internal,
                        detail: "wait was already consumed for this child".to_owned(),
                    }));
                };
                match wait_future.await {
                    Ok(result) => match result {
                        Ok(exit) => Ok(Ok($bindings::helios::system::programs::ExitStatus {
                            instance_id: exit.instance_id.raw(),
                            code: exit.exit_code,
                        })),
                        Err(error) => Ok(Err($convert_error(error))),
                    },
                    Err(_) => Ok(Err($bindings::helios::system::programs::SpawnError {
                        kind: $bindings::helios::system::programs::SpawnErrorKind::Internal,
                        detail: "child exit channel dropped before signalling completion"
                            .to_owned(),
                    })),
                }
            }

            #[allow(unused_mut)]
            fn stdin(
                mut access: Access<'_, U, Self>,
                child: wasmtime::component::Resource<ChildHandle>,
                mut data: StreamReader<u8>,
            ) -> wasmtime::Result<FutureReader<core::result::Result<(), ()>>> {
                use futures::channel::oneshot;
                let handle = access
                    .get()
                    .table
                    .get_mut(&child)
                    .map_err(wasmtime::Error::from)?;
                let writer = handle.take_stdin();
                let Some(writer) = writer else {
                    return FutureReader::new(&mut access, async move {
                        Ok::<_, wasmtime::Error>(Ok::<(), ()>(()))
                    });
                };
                let (tx, rx) = oneshot::channel();
                data.pipe(
                    &mut access,
                    crate::wasmtime_adapter::wasi::ChannelStreamConsumer::new(writer, tx),
                )?;
                FutureReader::new(&mut access, async move {
                    match rx.await {
                        Ok(result) => Ok::<_, wasmtime::Error>(result),
                        Err(_) => Ok::<_, wasmtime::Error>(Ok::<(), ()>(())),
                    }
                })
            }

            fn stdout(
                mut access: Access<'_, U, Self>,
                child: wasmtime::component::Resource<ChildHandle>,
            ) -> wasmtime::Result<(StreamReader<u8>, FutureReader<core::result::Result<(), ()>>)>
            {
                use futures::channel::oneshot;

                let handle = access
                    .get()
                    .table
                    .get_mut(&child)
                    .map_err(wasmtime::Error::from)?;
                let reader = handle.take_stdout();
                let stream = match reader {
                    Some(reader) => {
                        let (tx, rx) = oneshot::channel();
                        let stream = StreamReader::new(
                            &mut access,
                            ChannelStreamProducer::new_with_completion(reader, tx),
                        )?;
                        let future = FutureReader::new(&mut access, async move {
                            match rx.await {
                                Ok(()) => Ok::<_, wasmtime::Error>(Ok::<(), ()>(())),
                                Err(_) => Ok::<_, wasmtime::Error>(Ok::<(), ()>(())),
                            }
                        })?;
                        return Ok((stream, future));
                    }
                    None => StreamReader::new(&mut access, Vec::<u8>::new())?,
                };
                let future = FutureReader::new(&mut access, async {
                    Ok::<_, wasmtime::Error>(Ok::<(), ()>(()))
                })?;
                Ok((stream, future))
            }

            fn stderr(
                mut access: Access<'_, U, Self>,
                child: wasmtime::component::Resource<ChildHandle>,
            ) -> wasmtime::Result<(StreamReader<u8>, FutureReader<core::result::Result<(), ()>>)>
            {
                use futures::channel::oneshot;

                let handle = access
                    .get()
                    .table
                    .get_mut(&child)
                    .map_err(wasmtime::Error::from)?;
                let reader = handle.take_stderr();
                let stream = match reader {
                    Some(reader) => {
                        let (tx, rx) = oneshot::channel();
                        let stream = StreamReader::new(
                            &mut access,
                            ChannelStreamProducer::new_with_completion(reader, tx),
                        )?;
                        let future = FutureReader::new(&mut access, async move {
                            match rx.await {
                                Ok(()) => Ok::<_, wasmtime::Error>(Ok::<(), ()>(())),
                                Err(_) => Ok::<_, wasmtime::Error>(Ok::<(), ()>(())),
                            }
                        })?;
                        return Ok((stream, future));
                    }
                    None => StreamReader::new(&mut access, Vec::<u8>::new())?,
                };
                let future = FutureReader::new(&mut access, async {
                    Ok::<_, wasmtime::Error>(Ok::<(), ()>(()))
                })?;
                Ok((stream, future))
            }
        }

        impl<CpuImpl, HostFs, U> $bindings::helios::system::programs::HostWithStore<U>
            for HasSelf<StoreData<CpuImpl, HostFs>>
        where
            CpuImpl: Cpu + Clone,
            HostFs: crate::HostFileSystem,
        {
            fn spawn(
                accessor: &Accessor<U, Self>,
                request: $bindings::helios::system::programs::SpawnRequest,
            ) -> impl core::future::Future<
                Output = wasmtime::Result<
                    Result<
                        wasmtime::component::Resource<ChildHandle>,
                        $bindings::helios::system::programs::SpawnError,
                    >,
                >,
            > + Send {
                let snapshot = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>((
                        access.get().runtime_state.program_service(),
                        ProgramExecContext::from_store(access.get()),
                        access.get().process_authority().clone(),
                    ))
                });
                async move {
                    let (service, context, caller_authority) = snapshot?;
                    let Some(service) = service else {
                        return Ok(Err($bindings::helios::system::programs::SpawnError {
                            kind: $bindings::helios::system::programs::SpawnErrorKind::Unavailable,
                            detail: "program spawn is unavailable on this machine".to_owned(),
                        }));
                    };
                    if let Err(error) = require_spawn_authority(&caller_authority) {
                        return Ok(Err($convert_error(error)));
                    }
                    let child_authority =
                        match $build_authority(&caller_authority, request.capability_grants) {
                            Ok(authority) => authority,
                            Err(error) => return Ok(Err($convert_error(error))),
                        };
                    let source =
                        match read_program_source(accessor, &request.path, &caller_authority).await
                        {
                            Ok(Ok(source)) => source,
                            Ok(Err(error)) => return Ok(Err($convert_error(error))),
                            Err(error) => {
                                return Ok(Err($convert_error(map_program_host_error(
                                    ProgramHostOperation::ReadSpawnSource,
                                    error,
                                ))));
                            }
                        };
                    match service
                        .spawn(
                            context,
                            source,
                            None,
                            service::ProgramLaunch::new(
                                ProgramArgv::launched(request.name, request.args),
                                request.env,
                                child_authority,
                                None,
                            ),
                        )
                        .await
                    {
                        Ok(child) => {
                            let handle = accessor.with(|mut access| {
                                access
                                    .get()
                                    .table
                                    .push(child)
                                    .map_err(wasmtime::Error::from)
                            })?;
                            Ok(Ok(handle))
                        }
                        Err(error) => Ok(Err($convert_error(error))),
                    }
                }
            }

            fn exec(
                accessor: &Accessor<U, Self>,
                request: $bindings::helios::system::programs::ExecRequest,
            ) -> impl core::future::Future<
                Output = wasmtime::Result<
                    Result<
                        $bindings::helios::system::programs::ExecResult,
                        $bindings::helios::system::programs::ExecError,
                    >,
                >,
            > + Send {
                let snapshot = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>((
                        access.get().runtime_state.program_service(),
                        ProgramExecContext::from_store(access.get()),
                        access.get().process_authority().clone(),
                    ))
                });
                async move {
                    let (service, context, caller_authority) = snapshot?;
                    let Some(service) = service else {
                        return Ok(Err($bindings::helios::system::programs::ExecError {
                            kind: $bindings::helios::system::programs::ExecErrorKind::Unavailable,
                            detail: "program exec is unavailable on this machine".to_owned(),
                        }));
                    };
                    if let Err(error) = require_exec_authority(&caller_authority) {
                        return Ok(Err($convert_error(error)));
                    }
                    let child_authority =
                        match $build_authority(&caller_authority, request.capability_grants) {
                            Ok(authority) => authority,
                            Err(error) => return Ok(Err($convert_error(error))),
                        };
                    let source =
                        match read_program_source(accessor, &request.path, &caller_authority).await
                        {
                            Ok(Ok(source)) => source,
                            Ok(Err(error)) => return Ok(Err($convert_error(error))),
                            Err(error) => {
                                return Ok(Err($convert_error(map_program_host_error(
                                    ProgramHostOperation::ReadExecSource,
                                    error,
                                ))));
                            }
                        };
                    let hint = match request.hint {
                        Some($bindings::helios::system::programs::AotHint::Fast) => {
                            Some(AotCompileHint::Fast)
                        }
                        Some($bindings::helios::system::programs::AotHint::Balanced) => {
                            Some(AotCompileHint::Balanced)
                        }
                        Some($bindings::helios::system::programs::AotHint::Performance) => {
                            Some(AotCompileHint::Performance)
                        }
                        None => None,
                    };
                    Ok(service
                        .exec_buffered(
                            context,
                            source,
                            hint,
                            request.stdin,
                            service::ProgramLaunch::new(
                                ProgramArgv::launched(request.name, request.args),
                                request.env,
                                child_authority,
                                None,
                            ),
                        )
                        .await
                        .map($convert_result)
                        .map_err($convert_error))
                }
            }

            fn aot(
                accessor: &Accessor<U, Self>,
                request: $bindings::helios::system::programs::AotRequest,
            ) -> impl core::future::Future<
                Output = wasmtime::Result<
                    Result<
                        $bindings::helios::system::programs::AotResult,
                        $bindings::helios::system::programs::ExecError,
                    >,
                >,
            > + Send {
                let snapshot = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>((
                        access.get().runtime_state.program_service(),
                        ProgramExecContext::from_store(access.get()),
                        access.get().process_authority().clone(),
                    ))
                });
                async move {
                    let (service, context, caller_authority) = snapshot?;
                    let Some(service) = service else {
                        return Ok(Err($bindings::helios::system::programs::ExecError {
                            kind: $bindings::helios::system::programs::ExecErrorKind::Unavailable,
                            detail: "program AOT is unavailable on this machine".to_owned(),
                        }));
                    };
                    let source = match read_program_source(
                        accessor,
                        &request.source_path,
                        &caller_authority,
                    )
                    .await
                    {
                        Ok(Ok(source)) => source,
                        Ok(Err(error)) => return Ok(Err($convert_error(error))),
                        Err(error) => {
                            return Ok(Err($convert_error(map_program_host_error(
                                ProgramHostOperation::ReadAotSource,
                                error,
                            ))));
                        }
                    };
                    let ProgramSource::RawWasm(wasm) = source else {
                        return Ok(Err($bindings::helios::system::programs::ExecError {
                            kind: $bindings::helios::system::programs::ExecErrorKind::InvalidHint,
                            detail: "aot only accepts raw wasm inputs".to_owned(),
                        }));
                    };
                    let hint = match request.hint {
                        $bindings::helios::system::programs::AotHint::Fast => AotCompileHint::Fast,
                        $bindings::helios::system::programs::AotHint::Balanced => {
                            AotCompileHint::Balanced
                        }
                        $bindings::helios::system::programs::AotHint::Performance => {
                            AotCompileHint::Performance
                        }
                    };
                    let spawner = context.spawner();
                    let (artifact_tx, artifact_rx) = futures::channel::oneshot::channel();
                    spawner.spawn_detached({
                        let service = service.clone();
                        let context = context.clone();
                        let profile = request.profile;
                        async move {
                            let result = service.aot(&context, &wasm, hint, profile).await;
                            let _ = artifact_tx.send(result);
                        }
                    });
                    let artifact = match artifact_rx.await {
                        Ok(Ok(artifact)) => artifact,
                        Ok(Err(error)) => return Ok(Err($convert_error(error))),
                        Err(_) => {
                            return Ok(Err($bindings::helios::system::programs::ExecError {
                                kind: $bindings::helios::system::programs::ExecErrorKind::Internal,
                                detail: "program AOT worker dropped before producing an artifact"
                                    .to_owned(),
                            }));
                        }
                    };
                    match write_program_artifact(
                        accessor,
                        &request.destination_path,
                        &artifact,
                        &caller_authority,
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => return Ok(Err($convert_error(error))),
                        Err(error) => {
                            return Ok(Err($convert_error(map_program_host_error(
                                ProgramHostOperation::WriteAotArtifact,
                                error,
                            ))));
                        }
                    }
                    Ok(Ok($bindings::helios::system::programs::AotResult {
                        destination_path: request.destination_path,
                    }))
                }
            }
        }
    };
}

macro_rules! impl_authority_conversion {
    ($name:ident, $bindings:ident) => {
        fn $name(
            caller_authority: &ProcessAuthority,
            grants: Vec<$bindings::helios::system::programs::CapabilityGrant>,
        ) -> Result<ProcessAuthority, ProgramExecError> {
            let mut authority = ProcessAuthority::empty();
            for grant in grants {
                match grant {
                    $bindings::helios::system::programs::CapabilityGrant::Directory(
                        directory,
                    ) => {
                        let mut rights = DirectoryAuthorityRights::empty();
                        for right in directory.rights {
                            rights |= match right {
                                $bindings::helios::system::programs::FilesystemRight::Read => {
                                    DirectoryAuthorityRights::READ
                                }
                                $bindings::helios::system::programs::FilesystemRight::Write => {
                                    DirectoryAuthorityRights::WRITE
                                }
                                $bindings::helios::system::programs::FilesystemRight::MutateDirectory => {
                                    DirectoryAuthorityRights::MUTATE_DIRECTORY
                                }
                                $bindings::helios::system::programs::FilesystemRight::Execute => {
                                    DirectoryAuthorityRights::EXECUTE
                                }
                            };
                        }
                        let preopen = caller_authority
                            .derive_directory_preopen(
                                directory.source_path,
                                directory.guest_name,
                                rights,
                            )
                            .map_err(map_process_authority_error)?;
                        if authority.cwd().is_none()
                            && preopen.guest_name() == "/"
                            && preopen.rights().contains(DirectoryAuthorityRights::READ)
                        {
                            let cwd = caller_authority
                                .derive_directory_cap(
                                    preopen.source_path(),
                                    preopen.guest_name(),
                                    DirectoryAuthorityRights::READ,
                                )
                                .map_err(map_process_authority_error)?;
                            authority.chdir(cwd);
                        }
                        authority.insert_directory_preopen(preopen);
                    }
                    $bindings::helios::system::programs::CapabilityGrant::Network(network) => {
                        let mut rights = NetworkAuthorityRights::empty();
                        for right in network.rights {
                            rights |= match right {
                                $bindings::helios::system::programs::NetworkRight::Tcp => {
                                    NetworkAuthorityRights::TCP
                                }
                                $bindings::helios::system::programs::NetworkRight::Udp => {
                                    NetworkAuthorityRights::UDP
                                }
                                $bindings::helios::system::programs::NetworkRight::Dns => {
                                    NetworkAuthorityRights::DNS
                                }
                                $bindings::helios::system::programs::NetworkRight::PrivilegedBind => {
                                    NetworkAuthorityRights::PRIVILEGED_BIND
                                }
                                $bindings::helios::system::programs::NetworkRight::Multicast => {
                                    NetworkAuthorityRights::MULTICAST
                                }
                                $bindings::helios::system::programs::NetworkRight::Admin => {
                                    NetworkAuthorityRights::ADMIN
                                }
                            };
                        }
                        let rights = caller_authority
                            .derive_network_rights(rights)
                            .map_err(map_process_authority_error)?;
                        authority.grant_network_rights(rights);
                    }
                    $bindings::helios::system::programs::CapabilityGrant::Clock(clock) => {
                        let mut rights = ClockAuthorityRights::empty();
                        for right in clock.rights {
                            rights |= match right {
                                $bindings::helios::system::programs::ClockRight::SetWallClock => {
                                    ClockAuthorityRights::SET_WALL_CLOCK
                                }
                            };
                        }
                        let rights = caller_authority
                            .derive_clock_rights(rights)
                            .map_err(map_process_authority_error)?;
                        authority.grant_clock_rights(rights);
                    }
                    $bindings::helios::system::programs::CapabilityGrant::Terminal(terminal) => {
                        let mut rights = TerminalAuthorityRights::empty();
                        for right in terminal.rights {
                            rights |= match right {
                                $bindings::helios::system::programs::TerminalRight::Input => {
                                    TerminalAuthorityRights::INPUT
                                }
                                $bindings::helios::system::programs::TerminalRight::Output => {
                                    TerminalAuthorityRights::OUTPUT
                                }
                                $bindings::helios::system::programs::TerminalRight::Control => {
                                    TerminalAuthorityRights::CONTROL
                                }
                            };
                        }
                        let rights = caller_authority
                            .derive_terminal_rights(rights)
                            .map_err(map_process_authority_error)?;
                        authority.grant_terminal_rights(rights);
                    }
                    $bindings::helios::system::programs::CapabilityGrant::Process(process) => {
                        let mut rights = crate::ProcessAuthorityRights::empty();
                        for right in process.rights {
                            rights |= match right {
                                $bindings::helios::system::programs::ProcessRight::Spawn => {
                                    crate::ProcessAuthorityRights::SPAWN
                                }
                                $bindings::helios::system::programs::ProcessRight::Exec => {
                                    crate::ProcessAuthorityRights::EXEC
                                }
                                $bindings::helios::system::programs::ProcessRight::Fork => {
                                    crate::ProcessAuthorityRights::FORK
                                }
                                $bindings::helios::system::programs::ProcessRight::Join => {
                                    crate::ProcessAuthorityRights::JOIN
                                }
                                $bindings::helios::system::programs::ProcessRight::Signal => {
                                    crate::ProcessAuthorityRights::SIGNAL
                                }
                            };
                        }
                        let rights = caller_authority
                            .derive_process_rights(rights)
                            .map_err(map_process_authority_error)?;
                        authority.grant_process_rights(rights);
                    }
                    $bindings::helios::system::programs::CapabilityGrant::Link(link) => {
                        let mut rights = LinkAuthorityRights::empty();
                        for right in link.rights {
                            rights |= match right {
                                $bindings::helios::system::programs::LinkRight::Source => {
                                    LinkAuthorityRights::SOURCE
                                }
                                $bindings::helios::system::programs::LinkRight::TargetDirectory => {
                                    LinkAuthorityRights::TARGET_DIRECTORY
                                }
                                $bindings::helios::system::programs::LinkRight::SymlinkCreate => {
                                    LinkAuthorityRights::SYMLINK_CREATE
                                }
                                $bindings::helios::system::programs::LinkRight::SymlinkRead => {
                                    LinkAuthorityRights::SYMLINK_READ
                                }
                            };
                        }
                        let rights = caller_authority
                            .derive_link_rights(rights)
                            .map_err(map_process_authority_error)?;
                        authority.grant_link_rights(rights);
                    }
                }
            }
            Ok(authority)
        }
    };
}

impl_authority_conversion!(build_debugger_child_authority, debugger_bindings);
impl_authority_conversion!(build_program_child_authority, program_bindings);

fn require_spawn_authority(
    caller_authority: &ProcessAuthority,
) -> Result<(), crate::ProgramExecError> {
    caller_authority
        .derive_spawn_authority()
        .map(drop)
        .map_err(map_process_authority_error)
}

fn require_exec_authority(
    caller_authority: &ProcessAuthority,
) -> Result<(), crate::ProgramExecError> {
    caller_authority
        .derive_exec_authority()
        .map(drop)
        .map_err(map_process_authority_error)
}

impl_program_bindings!(
    debugger_bindings,
    convert_launch_result,
    convert_launch_error,
    build_debugger_child_authority
);
impl_program_bindings!(
    program_bindings,
    convert_program_launch_result,
    convert_program_launch_error,
    build_program_child_authority
);

#[cfg(test)]
mod lowering_tests {
    use alloc::vec::Vec;

    use bytes::{Bytes, BytesMut};

    use super::lower_bytes_to_vec;

    #[test]
    fn owned_bytes_lowering_reuses_unique_vec_buffer() {
        let source = Vec::from([1_u8, 2, 3, 4]);
        let source_ptr = source.as_ptr();
        let bytes = Bytes::from(source);

        let lowered = lower_bytes_to_vec(bytes);

        assert_eq!(lowered.as_slice(), [1, 2, 3, 4]);
        assert_eq!(lowered.as_ptr(), source_ptr);
    }

    #[test]
    fn frozen_bytes_mut_lowering_reuses_unique_vec_buffer() {
        let mut source = BytesMut::with_capacity(4096);
        source.extend_from_slice(&[1_u8, 2, 3, 4]);
        let source_ptr = source.as_ptr();
        let bytes = source.freeze();

        let lowered = lower_bytes_to_vec(bytes);

        assert_eq!(lowered.as_slice(), [1, 2, 3, 4]);
        assert_eq!(lowered.as_ptr(), source_ptr);
    }
}

#[cfg(test)]
mod authority_tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{
        build_program_child_authority, dns_network_rights, program_bindings,
        require_exec_authority, require_spawn_authority, tcp_connect_network_rights,
        udp_bind_network_rights,
    };
    use crate::{
        ClockAuthorityRights, DirectoryAuthorityRights, DirectoryPreopen, LinkAuthorityRights,
        NetworkAuthorityRights, ProcessAuthority, ProcessAuthorityRights, TerminalAuthorityRights,
    };

    use program_bindings::helios::system::programs::{
        CapabilityGrant, ClockGrant, ClockRight, DirectoryGrant, FilesystemRight, LinkGrant,
        LinkRight, NetworkGrant, NetworkRight, ProcessGrant, ProcessRight, TerminalGrant,
        TerminalRight,
    };

    fn directory_grant(
        source_path: &str,
        guest_name: &str,
        rights: Vec<FilesystemRight>,
    ) -> CapabilityGrant {
        CapabilityGrant::Directory(DirectoryGrant {
            source_path: source_path.into(),
            guest_name: guest_name.into(),
            rights,
        })
    }

    fn network_grant(rights: Vec<NetworkRight>) -> CapabilityGrant {
        CapabilityGrant::Network(NetworkGrant { rights })
    }

    fn clock_grant(rights: Vec<ClockRight>) -> CapabilityGrant {
        CapabilityGrant::Clock(ClockGrant { rights })
    }

    fn terminal_grant(rights: Vec<TerminalRight>) -> CapabilityGrant {
        CapabilityGrant::Terminal(TerminalGrant { rights })
    }

    fn process_grant(rights: Vec<ProcessRight>) -> CapabilityGrant {
        CapabilityGrant::Process(ProcessGrant { rights })
    }

    fn link_grant(rights: Vec<LinkRight>) -> CapabilityGrant {
        CapabilityGrant::Link(LinkGrant { rights })
    }

    #[test]
    fn empty_grants_create_zero_child_authority() {
        let child = build_program_child_authority(&ProcessAuthority::root(), Vec::new())
            .expect("empty grants must be a valid zero-authority child");

        assert!(child.directory_preopens().is_empty());
        assert!(child.network_rights().is_empty());
        assert!(child.clock_rights().is_empty());
        assert!(child.terminal_rights().is_empty());
        assert!(child.process_rights().is_empty());
        assert!(child.link_rights().is_empty());
        assert!(child.cwd().is_none());
    }

    #[test]
    fn explicit_grant_derives_subset_from_caller_authority() {
        let child = build_program_child_authority(
            &ProcessAuthority::root(),
            vec![directory_grant(
                "/bin",
                "/tools",
                vec![FilesystemRight::Read, FilesystemRight::Execute],
            )],
        )
        .expect("root authority may derive a narrower executable directory");

        let [preopen] = child.directory_preopens() else {
            panic!("expected exactly one derived preopen");
        };
        assert_eq!(preopen.source_path(), "/bin");
        assert_eq!(preopen.guest_name(), "/tools");
        assert_eq!(
            preopen.rights(),
            DirectoryAuthorityRights::READ | DirectoryAuthorityRights::EXECUTE
        );
        assert!(child.cwd().is_none());
    }

    #[test]
    fn explicit_root_directory_grant_sets_child_cwd_from_capability() {
        let child = build_program_child_authority(
            &ProcessAuthority::root(),
            vec![directory_grant("/", "/", vec![FilesystemRight::Read])],
        )
        .expect("explicit root directory grant may establish initial cwd");

        assert_eq!(
            child.cwd().expect("root directory grant should set cwd"),
            &DirectoryPreopen::new("/", "/", DirectoryAuthorityRights::READ)
                .expect("expected cwd should be valid")
        );
    }

    #[test]
    fn non_read_root_directory_grant_does_not_create_cwd() {
        let child = build_program_child_authority(
            &ProcessAuthority::root(),
            vec![directory_grant("/", "/", vec![FilesystemRight::Execute])],
        )
        .expect("executable root directory grant is valid");

        assert!(child.cwd().is_none());
    }

    #[test]
    fn explicit_grant_rejects_right_widening() {
        let mut caller = ProcessAuthority::empty();
        caller.insert_directory_preopen(
            DirectoryPreopen::new("/bin", "/", DirectoryAuthorityRights::READ)
                .expect("test authority must be valid"),
        );

        let error = build_program_child_authority(
            &caller,
            vec![directory_grant(
                "/bin",
                "/",
                vec![FilesystemRight::Read, FilesystemRight::Write],
            )],
        )
        .expect_err("write must not be derived from read-only caller authority");

        assert_eq!(error.kind, crate::ProgramExecErrorKind::PermissionDenied);
    }

    #[test]
    fn explicit_grant_rejects_path_escape() {
        let error = build_program_child_authority(
            &ProcessAuthority::root(),
            vec![directory_grant("../bin", "/", vec![FilesystemRight::Read])],
        )
        .expect_err("relative escape must not be accepted as a grant source");

        assert_eq!(error.kind, crate::ProgramExecErrorKind::InvalidPath);
    }

    #[test]
    fn network_grant_derives_subset_from_caller_authority() {
        let mut caller = ProcessAuthority::empty();
        caller.grant_network_rights(NetworkAuthorityRights::TCP | NetworkAuthorityRights::DNS);

        let child =
            build_program_child_authority(&caller, vec![network_grant(vec![NetworkRight::Tcp])])
                .expect("network child grant may derive a subset");

        assert_eq!(child.network_rights(), NetworkAuthorityRights::TCP);
    }

    #[test]
    fn network_grant_rejects_right_widening() {
        let mut caller = ProcessAuthority::empty();
        caller.grant_network_rights(NetworkAuthorityRights::DNS);

        let error =
            build_program_child_authority(&caller, vec![network_grant(vec![NetworkRight::Tcp])])
                .expect_err("TCP must not be created from DNS-only authority");

        assert_eq!(error.kind, crate::ProgramExecErrorKind::PermissionDenied);
    }

    #[test]
    fn clock_grant_derives_subset_from_caller_authority() {
        let mut caller = ProcessAuthority::empty();
        caller.grant_clock_rights(ClockAuthorityRights::SET_WALL_CLOCK);

        let child = build_program_child_authority(
            &caller,
            vec![clock_grant(vec![ClockRight::SetWallClock])],
        )
        .expect("clock child grant may derive a subset");

        assert_eq!(child.clock_rights(), ClockAuthorityRights::SET_WALL_CLOCK);
    }

    #[test]
    fn terminal_grant_derives_subset_from_caller_authority() {
        let mut caller = ProcessAuthority::empty();
        caller.grant_terminal_rights(TerminalAuthorityRights::OUTPUT);

        let child = build_program_child_authority(
            &caller,
            vec![terminal_grant(vec![TerminalRight::Output])],
        )
        .expect("terminal child grant may derive a subset");

        assert_eq!(child.terminal_rights(), TerminalAuthorityRights::OUTPUT);
    }

    #[test]
    fn process_grant_derives_subset_from_caller_authority() {
        let mut caller = ProcessAuthority::empty();
        caller.grant_process_rights(ProcessAuthorityRights::SPAWN | ProcessAuthorityRights::JOIN);

        let child =
            build_program_child_authority(&caller, vec![process_grant(vec![ProcessRight::Spawn])])
                .expect("process child grant may derive a subset");

        assert_eq!(child.process_rights(), ProcessAuthorityRights::SPAWN);
    }

    #[test]
    fn process_grant_rejects_right_widening() {
        let mut caller = ProcessAuthority::empty();
        caller.grant_process_rights(ProcessAuthorityRights::JOIN);

        let error =
            build_program_child_authority(&caller, vec![process_grant(vec![ProcessRight::Spawn])])
                .expect_err("spawn must not be created from join-only authority");

        assert_eq!(error.kind, crate::ProgramExecErrorKind::PermissionDenied);
    }

    #[test]
    fn link_grant_derives_subset_from_caller_authority() {
        let mut caller = ProcessAuthority::empty();
        caller.grant_link_rights(LinkAuthorityRights::SOURCE | LinkAuthorityRights::SYMLINK_READ);

        let child = build_program_child_authority(
            &caller,
            vec![link_grant(vec![LinkRight::Source, LinkRight::SymlinkRead])],
        )
        .expect("link child grant may derive a subset");

        assert_eq!(
            child.link_rights(),
            LinkAuthorityRights::SOURCE | LinkAuthorityRights::SYMLINK_READ
        );
    }

    #[test]
    fn link_grant_rejects_right_widening() {
        let mut caller = ProcessAuthority::empty();
        caller.grant_link_rights(LinkAuthorityRights::SYMLINK_READ);

        let error = build_program_child_authority(
            &caller,
            vec![link_grant(vec![LinkRight::SymlinkCreate])],
        )
        .expect_err("symlink-create must not be created from readlink-only authority");

        assert_eq!(error.kind, crate::ProgramExecErrorKind::PermissionDenied);
    }

    #[test]
    fn network_operations_require_typed_capability_sets() {
        assert_eq!(dns_network_rights(), NetworkAuthorityRights::DNS);
        assert_eq!(
            tcp_connect_network_rights(),
            NetworkAuthorityRights::TCP | NetworkAuthorityRights::DNS
        );
        assert_eq!(udp_bind_network_rights(0), NetworkAuthorityRights::UDP);
        assert_eq!(udp_bind_network_rights(1024), NetworkAuthorityRights::UDP);
        assert_eq!(
            udp_bind_network_rights(80),
            NetworkAuthorityRights::UDP | NetworkAuthorityRights::PRIVILEGED_BIND
        );
    }

    #[test]
    fn program_spawn_and_exec_require_process_authority() {
        let caller = ProcessAuthority::empty();
        assert_eq!(
            require_spawn_authority(&caller)
                .expect_err("spawn authority must be explicit")
                .kind,
            crate::ProgramExecErrorKind::PermissionDenied
        );
        assert_eq!(
            require_exec_authority(&caller)
                .expect_err("exec authority must be explicit")
                .kind,
            crate::ProgramExecErrorKind::PermissionDenied
        );

        let mut caller = ProcessAuthority::empty();
        caller.grant_process_rights(ProcessAuthorityRights::SPAWN | ProcessAuthorityRights::EXEC);
        require_spawn_authority(&caller).expect("spawn authority should be accepted");
        require_exec_authority(&caller).expect("exec authority should be accepted");
    }
}

async fn read_program_source<T, CpuImpl, HostFs>(
    accessor: &Accessor<T, HasSelf<StoreData<CpuImpl, HostFs>>>,
    path: &str,
    authority: &ProcessAuthority,
) -> wasmtime::Result<Result<service::ProgramSource, crate::ProgramExecError>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let absolute =
        crate::resolve_guest_path("/", path).map_err(map_component_fs_path_error_to_wasmtime)?;
    if !authority.can_load_program(&absolute) {
        return Ok(Err(ProgramExecError {
            kind: ProgramExecErrorKind::PermissionDenied,
            detail: ProgramExecErrorDetail::ProgramSourceNotGranted,
        }));
    }
    let host_path = crate::guest_host_share_path(&absolute).map(str::to_owned);
    if let Some(host_path) = host_path {
        let host_service = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().runtime_state.host_filesystem_service())
        })?;
        let Some(host_service) = host_service else {
            return Ok(Err(ProgramExecError {
                kind: ProgramExecErrorKind::Unavailable,
                detail: ProgramExecErrorDetail::HostFilesystemUnavailable,
            }));
        };
        return Ok(host_service
            .read_file(&host_path)
            .await
            .map(|bytes| classify_program_source(bytes::Bytes::from(bytes), false))
            .map_err(|error| {
                tracing::error!(path = %absolute, ?error, "failed to read program source");
                ProgramExecError {
                    kind: ProgramExecErrorKind::InvalidPath,
                    detail: ProgramExecErrorDetail::FilesystemOperationFailed,
                }
            }));
    }

    accessor.with(|mut access| {
        let readonly = access
            .get()
            .filesystem()
            .is_readonly_path(&absolute)
            .map_err(map_fs_error_to_program_exec)?;
        let bytes = access
            .get()
            .filesystem()
            .read_program_file_bytes(&absolute)
            .map_err(map_fs_error_to_program_exec)?;
        Ok::<_, wasmtime::Error>(Ok(classify_program_source(bytes, readonly)))
    })
}

async fn write_program_artifact<T, CpuImpl, HostFs>(
    accessor: &Accessor<T, HasSelf<StoreData<CpuImpl, HostFs>>>,
    path: &str,
    bytes: &[u8],
    authority: &ProcessAuthority,
) -> wasmtime::Result<Result<(), crate::ProgramExecError>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let absolute =
        crate::resolve_guest_path("/", path).map_err(map_component_fs_path_error_to_wasmtime)?;
    if !authority.can_create_or_replace_path(&absolute) {
        return Ok(Err(ProgramExecError {
            kind: ProgramExecErrorKind::PermissionDenied,
            detail: ProgramExecErrorDetail::ProgramArtifactDestinationNotGranted,
        }));
    }
    let host_path = crate::guest_host_share_path(&absolute).map(str::to_owned);
    if let Some(host_path) = host_path {
        let host_service = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().runtime_state.host_filesystem_service())
        })?;
        let Some(host_service) = host_service else {
            return Ok(Err(ProgramExecError {
                kind: ProgramExecErrorKind::Unavailable,
                detail: ProgramExecErrorDetail::HostFilesystemUnavailable,
            }));
        };
        if let Err(truncate_error) = host_service.truncate_file(&host_path).await {
            host_service
                .create_file(&host_path)
                .await
                .map_err(|create_error| {
                    tracing::error!(
                        path = %absolute,
                        ?truncate_error,
                        ?create_error,
                        "failed to prepare program artifact destination"
                    );
                    ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidPath,
                        detail: ProgramExecErrorDetail::FilesystemOperationFailed,
                    }
                })?;
        }
        host_service
            .write_file(&host_path, 0, bytes)
            .await
            .map_err(|error| {
                tracing::error!(path = %absolute, ?error, "failed to write program artifact");
                ProgramExecError {
                    kind: ProgramExecErrorKind::InvalidPath,
                    detail: ProgramExecErrorDetail::FilesystemOperationFailed,
                }
            })?;
        return Ok(Ok(()));
    }

    accessor.with(|mut access| {
        let store_data = access.get();
        let now_nanos = store_data.now_nanos();
        store_data
            .filesystem_mut()
            .write_program_file(&absolute, bytes, now_nanos)
            .map_err(map_fs_error_to_program_exec)?;
        Ok::<_, wasmtime::Error>(Ok(()))
    })
}

fn classify_program_source(bytes: bytes::Bytes, readonly: bool) -> ProgramSource {
    let is_cwasm = cwasm::is_cwasm(&bytes);
    if is_cwasm {
        if readonly {
            return ProgramSource::BootfsArtifact(bytes);
        }
        return ProgramSource::SignedArtifact(bytes);
    }
    ProgramSource::RawWasm(bytes)
}

fn map_fs_error_to_program_exec(error: FsErrorCode) -> crate::ProgramExecError {
    tracing::error!(?error, "filesystem operation failed during program exec");
    let kind = match error {
        FsErrorCode::NoEntry | FsErrorCode::NotDirectory | FsErrorCode::BadDescriptor => {
            crate::ProgramExecErrorKind::InvalidPath
        }
        FsErrorCode::ReadOnly
        | FsErrorCode::NotPermitted
        | FsErrorCode::Exist
        | FsErrorCode::IsDirectory
        | FsErrorCode::IllegalByteSequence
        | FsErrorCode::Invalid
        | FsErrorCode::Overflow => crate::ProgramExecErrorKind::InvalidPath,
        _ => crate::ProgramExecErrorKind::Internal,
    };
    crate::ProgramExecError {
        kind,
        detail: ProgramExecErrorDetail::FilesystemOperationFailed,
    }
}

fn map_component_fs_path_error_to_wasmtime(error: crate::ComponentFsPathError) -> wasmtime::Error {
    wasmtime::Error::new(error)
}

fn map_process_authority_error(error: ProcessAuthorityError) -> crate::ProgramExecError {
    tracing::error!(?error, "process authority rejected program operation");
    let kind = match error {
        ProcessAuthorityError::InvalidPath(_) | ProcessAuthorityError::EmptyRights => {
            crate::ProgramExecErrorKind::InvalidPath
        }
        ProcessAuthorityError::EmptyNetworkRights
        | ProcessAuthorityError::EmptyClockRights
        | ProcessAuthorityError::EmptyTerminalRights
        | ProcessAuthorityError::EmptyProcessRights
        | ProcessAuthorityError::DirectoryGrantExceedsAuthority(_)
        | ProcessAuthorityError::NetworkGrantExceedsAuthority(_)
        | ProcessAuthorityError::ClockGrantExceedsAuthority(_)
        | ProcessAuthorityError::TerminalGrantExceedsAuthority(_)
        | ProcessAuthorityError::ProcessGrantExceedsAuthority(_)
        | ProcessAuthorityError::EmptyLinkRights
        | ProcessAuthorityError::LinkGrantExceedsAuthority(_) => {
            crate::ProgramExecErrorKind::PermissionDenied
        }
    };
    crate::ProgramExecError {
        kind,
        detail: ProgramExecErrorDetail::ProcessAuthorityDenied,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProgramHostOperation {
    ReadSpawnSource,
    ReadExecSource,
    ReadAotSource,
    WriteAotArtifact,
}

fn map_program_host_error(
    operation: ProgramHostOperation,
    error: wasmtime::Error,
) -> crate::ProgramExecError {
    tracing::error!(?operation, ?error, "program host operation failed");
    crate::ProgramExecError {
        kind: crate::ProgramExecErrorKind::Internal,
        detail: ProgramExecErrorDetail::HostOperationFailed,
    }
}

/// The kernel handles a system component borrows for its whole run.
pub(super) struct SystemComponentHost<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    pub(super) cpu: CpuImpl,
    pub(super) timer: crate::Timer<CpuImpl>,
    pub(super) spawner: crate::Spawner<CpuImpl>,
    pub(super) debug_state: HostRuntimeState<CpuImpl, HostFs>,
    pub(super) read_serial: crate::SerialReader,
    pub(super) write_serial: fn(&[u8]),
}

async fn run_system_component<CpuImpl, HostFs>(
    component: EmbeddedComponent,
    world: ComponentBindingSet,
    host: SystemComponentHost<CpuImpl, HostFs>,
) -> Result<(), DebuggerError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    use crate::{
        ComponentExecContext, ComponentExecutor, ComponentExitStatus, ComponentRuntimeFactory,
        ComponentWorld,
    };

    let SystemComponentHost {
        cpu,
        timer,
        spawner,
        debug_state,
        read_serial,
        write_serial,
    } = host;

    let component_name = component.name();
    let runtime = crate::wasmtime_adapter::WasmtimeComponentRuntime::new(cpu.clone());

    emit_stage_marker(write_serial, "engine:new");
    tracing::info!(
        component = component_name,
        "creating system component engine"
    );
    let engine =
        <crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl> as ComponentRuntimeFactory<
            CpuImpl,
            HostRuntimeState<CpuImpl, HostFs>,
            HostFs,
        >>::create_engine(&runtime)
        .map_err(DebuggerError::CreateEngine)?;
    emit_stage_marker(write_serial, "engine:ok");

    tracing::info!(
        component = component_name,
        "loading embedded system component artifact"
    );
    let trusted = cwasm::trust_bootfs_artifact(UntrustedCwasm::new(component.bytes()))
        .map_err(DebuggerError::TrustComponent)?;
    let compiled = crate::wasmtime_adapter::WasmtimeCompiledComponent {
        component: unsafe {
            wasmtime::component::Component::deserialize(engine.raw(), trusted.payload())
        }
        .map_err(DebuggerError::LoadComponent)?,
    };
    emit_stage_marker(write_serial, "component:ok");

    emit_stage_marker(write_serial, "instantiate:begin");
    tracing::info!(component = component_name, "instantiating system component");
    let instance_registry = debug_state.instance_registry();
    let instance = instance_registry.register_with_policy(
        component_name,
        debug_state.uptime_nanos(cpu.now().ticks()),
        crate::OomPolicy::SystemComponent,
    );

    let component_world = match world {
        ComponentBindingSet::System => ComponentWorld::System,
        ComponentBindingSet::Program => ComponentWorld::Program,
    };
    let instantiate_cpu = cpu.clone();
    let instantiate_timer = timer.clone();
    // A system component is kernel infrastructure, so its tasks are
    // funded from the arena's kernel reserve: user-mode load fills the
    // instance share and stops there.
    let instantiate_spawner = spawner.instance_spawner(crate::TaskFunding::Kernel);

    let context = ComponentExecContext::new(
        cpu,
        timer,
        spawner,
        debug_state.clone(),
        instance_registry,
        instance,
        true,
        debug_state.clone(),
        Vec::new(),
        Vec::new(),
        ProcessAuthority::root(),
        OutputMode::Serial,
        read_serial,
        write_serial,
    );

    let executor = runtime.instantiate(&engine, &compiled, component_world, context);
    let instantiate_done = Arc::new(AtomicBool::new(false));
    let instantiate_started_at = monotonic_nanos(&instantiate_cpu);
    spawn_component_phase_heartbeat(
        &instantiate_spawner,
        &instantiate_cpu,
        &instantiate_timer,
        &instantiate_spawner.progress_counter(),
        write_serial,
        ComponentPhase {
            name: "instantiate",
            started_at: instantiate_started_at,
            done: &instantiate_done,
        },
    )
    .map_err(DebuggerError::TaskCapacity)?;
    let executor = executor.await;
    instantiate_done.store(true, Ordering::Release);
    let executor = executor.map_err(DebuggerError::InstantiateComponent)?;
    emit_stage_marker(write_serial, "instantiate:ok");

    tracing::info!(component = component_name, "entering wasi:cli/run");
    emit_stage_marker(write_serial, "run:begin");

    let result = executor.run().await.map_err(DebuggerError::RunComponent)?;

    emit_stage_marker(write_serial, "run:ok");
    tracing::info!(component = component_name, "wasi:cli/run returned");

    match result.status {
        ComponentExitStatus::Ok => Ok(()),
        ComponentExitStatus::Failed => Err(DebuggerError::GuestFailed),
    }
}

pub(crate) fn component_linker<CpuImpl, HostFs>(
    engine: &Engine,
    world: ComponentBindingSet,
    component: &Component,
) -> wasmtime::Result<Linker<StoreData<CpuImpl, HostFs>>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let mut linker = Linker::<StoreData<CpuImpl, HostFs>>::new(engine);
    let wasi_imports =
        crate::wasmtime_adapter::wasi::WasiImportSet::from_component(engine, component);
    linker.allow_shadowing(true);
    wasmtime_wasi_io::add_to_linker_async(&mut linker)?;
    crate::wasmtime_adapter::wasi::preview2::add_to_linker(&mut linker, &wasi_imports)?;
    crate::wasmtime_adapter::wasi::preview3::add_to_linker(&mut linker, &wasi_imports)?;
    crate::wasmtime_adapter::wasi::http::add_to_linker(&mut linker, &wasi_imports)?;
    add_serial_to_linker(&mut linker)?;
    add_sync_to_linker(&mut linker)?;
    match world {
        ComponentBindingSet::System => add_system_to_linker(&mut linker)?,
        ComponentBindingSet::Program => add_program_world_to_linker(&mut linker)?,
    }
    Ok(linker)
}

pub(crate) fn store_with_state<CpuImpl, HostFs>(
    engine: &Engine,
    state: StoreData<CpuImpl, HostFs>,
) -> Store<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let mut store = Store::new(engine, state);
    store.limiter(|state| state);
    store.call_hook(
        |mut caller: StoreContextMut<'_, StoreData<CpuImpl, HostFs>>, hook| {
            let transition = crate::wasmtime_adapter::store::translate_call_hook(hook);
            caller.data_mut().record_transition(transition);
            if let Some(reason) = caller.data().check_pending_kill() {
                return Err(wasmtime::Error::from(crate::InstanceKilled { reason }));
            }
            Ok(())
        },
    );
    store.set_epoch_deadline(1);
    // The epoch tick is also the only place a CPU-bound instance can
    // observe a pending kill: host-call hooks never run during long pure
    // wasm stretches (e.g. an AOT compile), and cancelling the future
    // instead would surface a bare interrupt trap that loses the kill
    // reason (OOM vs supervisor restart).
    store.epoch_deadline_callback(|caller| {
        if let Some(reason) = caller.data().check_pending_kill() {
            return Err(wasmtime::Error::from(crate::InstanceKilled { reason }));
        }
        Ok(wasmtime::UpdateDeadline::Yield(1))
    });
    store
}

fn add_system_to_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    add_programs_to_linker(linker)?;
    add_net_to_linker(linker)?;
    vsock::add_vsock_to_linker::<vsock::DebuggerVsock, _, _>(linker)?;
    add_stats_to_linker(linker)?;
    add_instances_to_linker(linker)?;
    add_tracing_to_linker(linker)?;
    add_profiling_to_linker(linker)?;
    Ok(())
}

struct ComponentHostProfile<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    runtime_state: HostRuntimeState<CpuImpl, HostFs>,
    cpu: CpuImpl,
    started_ticks: u64,
    counters: helios_hal::cpu::HardwarePerfCounters,
    started_heap: HeapStats,
}

impl<CpuImpl, HostFs> ComponentHostProfile<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn restarted(&self) -> Self {
        Self {
            runtime_state: self.runtime_state.clone(),
            cpu: self.cpu.clone(),
            started_ticks: self.cpu.now().ticks(),
            counters: self.cpu.hardware_perf_counters(),
            started_heap: crate::heap_stats(),
        }
    }
}

fn component_host_profile<CpuImpl, HostFs>(
    store: &StoreData<CpuImpl, HostFs>,
) -> Option<ComponentHostProfile<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    store
        .runtime_state
        .profiling_enabled()
        .then(|| ComponentHostProfile {
            runtime_state: store.runtime_state.clone(),
            cpu: store.cpu.clone(),
            started_ticks: store.cpu.now().ticks(),
            counters: store.cpu.hardware_perf_counters(),
            started_heap: crate::heap_stats(),
        })
}

fn record_component_host_kernel_profile<CpuImpl, HostFs>(
    profile: Option<ComponentHostProfile<CpuImpl, HostFs>>,
    phase: &'static str,
) where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if let Some(profile) = profile {
        profile.runtime_state.record_profile_stack_parts(
            ProfileScope::Kernel,
            "kernel;component-host;",
            phase,
            profile
                .cpu
                .now()
                .ticks()
                .saturating_sub(profile.started_ticks),
        );
    }
}

fn record_component_host_kernel_profile_events_bytes<CpuImpl, HostFs>(
    profile: Option<ComponentHostProfile<CpuImpl, HostFs>>,
    phase: &'static str,
    events: u64,
    bytes: u64,
) where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if let Some(profile) = profile {
        let ended_ticks = profile.cpu.now().ticks();
        let elapsed_ticks = ended_ticks.saturating_sub(profile.started_ticks);
        profile.runtime_state.record_profile_stack_parts(
            ProfileScope::Kernel,
            "kernel;component-host;",
            phase,
            elapsed_ticks,
        );
        let elapsed_nanos = profile
            .runtime_state
            .uptime_nanos(ended_ticks)
            .saturating_sub(profile.runtime_state.uptime_nanos(profile.started_ticks));
        let counter_delta = profile
            .cpu
            .hardware_perf_counters()
            .delta_since(profile.counters);
        profile.runtime_state.record_perf_metric_parts(
            ProfileScope::Kernel,
            "kernel;component-host;",
            phase,
            PerfSample {
                events,
                elapsed_nanos,
                counters: counter_delta,
                bytes,
            },
        );
        let heap = crate::heap_stats();
        record_component_host_heap_delta(
            &profile.runtime_state,
            phase,
            "heap-alloc",
            heap.allocation_count
                .saturating_sub(profile.started_heap.allocation_count),
            heap.total_allocation_bytes
                .saturating_sub(profile.started_heap.total_allocation_bytes),
        );
        record_component_host_heap_delta(
            &profile.runtime_state,
            phase,
            "heap-realloc",
            heap.reallocation_count
                .saturating_sub(profile.started_heap.reallocation_count),
            heap.total_reallocation_bytes
                .saturating_sub(profile.started_heap.total_reallocation_bytes),
        );
        record_component_host_heap_delta(
            &profile.runtime_state,
            phase,
            "heap-dealloc",
            heap.deallocation_count
                .saturating_sub(profile.started_heap.deallocation_count),
            heap.total_deallocation_bytes
                .saturating_sub(profile.started_heap.total_deallocation_bytes),
        );
    }
}

fn record_component_host_heap_delta<CpuImpl, HostFs>(
    runtime_state: &HostRuntimeState<CpuImpl, HostFs>,
    phase: &'static str,
    kind: &'static str,
    events: u64,
    bytes: u64,
) where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if events == 0 && bytes == 0 {
        return;
    }
    let phase_prefix = match kind {
        "heap-alloc" => "kernel;component-host-heap;alloc;",
        "heap-realloc" => "kernel;component-host-heap;realloc;",
        "heap-dealloc" => "kernel;component-host-heap;dealloc;",
        _ => panic!("unknown component-host heap metric kind {kind}"),
    };
    runtime_state.record_perf_metric_parts(
        ProfileScope::Kernel,
        phase_prefix,
        phase,
        PerfSample {
            events,
            elapsed_nanos: 0,
            counters: helios_hal::cpu::HardwarePerfCounterDelta::default(),
            bytes,
        },
    );
}

fn component_host_usize_to_u64(value: usize, label: &'static str) -> u64 {
    u64::try_from(value).unwrap_or_else(|_| panic!("{label} does not fit into u64"))
}

pub(crate) fn add_program_world_to_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    add_programs_to_program_linker(linker)?;
    add_net_to_program_linker(linker)?;
    vsock::add_vsock_to_linker::<vsock::ProgramVsock, _, _>(linker)?;
    add_stats_to_program_linker(linker)?;
    add_tracing_to_program_linker(linker)?;
    Ok(())
}

pub(crate) fn add_serial_to_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    debugger_bindings::helios::system::serial::add_to_linker::<
        _,
        HasSelf<StoreData<CpuImpl, HostFs>>,
    >(linker, |state| state)?;
    Ok(())
}

pub(crate) fn add_sync_to_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let mut instance = linker.instance(SYNC_INSTANCE)?;
    instance.resource_concurrent(
        "raw-mutex",
        ResourceType::host::<SbiRawMutex>(),
        |accessor, rep| {
            Box::pin(async move {
                accessor.with(|mut access| {
                    let resource = Resource::<SbiRawMutex>::new_own(rep);
                    let _ = access.get().table.delete(resource)?;
                    Ok::<_, wasmtime::Error>(())
                })
            })
        },
    )?;
    instance.resource_concurrent(
        "raw-mutex-guard",
        ResourceType::host::<SbiRawMutexGuard>(),
        |accessor, rep| {
            Box::pin(async move {
                accessor.with(|mut access| {
                    let resource = Resource::<SbiRawMutexGuard>::new_own(rep);
                    let _ = access.get().table.delete(resource)?;
                    Ok::<_, wasmtime::Error>(())
                })
            })
        },
    )?;
    instance.resource_concurrent(
        "raw-rw-lock",
        ResourceType::host::<SbiRawRwLock>(),
        |accessor, rep| {
            Box::pin(async move {
                accessor.with(|mut access| {
                    let resource = Resource::<SbiRawRwLock>::new_own(rep);
                    let _ = access.get().table.delete(resource)?;
                    Ok::<_, wasmtime::Error>(())
                })
            })
        },
    )?;
    instance.resource_concurrent(
        "raw-rw-lock-read-guard",
        ResourceType::host::<SbiRawRwLockReadGuard>(),
        |accessor, rep| {
            Box::pin(async move {
                accessor.with(|mut access| {
                    let resource = Resource::<SbiRawRwLockReadGuard>::new_own(rep);
                    let _ = access.get().table.delete(resource)?;
                    Ok::<_, wasmtime::Error>(())
                })
            })
        },
    )?;
    instance.resource_concurrent(
        "raw-rw-lock-write-guard",
        ResourceType::host::<SbiRawRwLockWriteGuard>(),
        |accessor, rep| {
            Box::pin(async move {
                accessor.with(|mut access| {
                    let resource = Resource::<SbiRawRwLockWriteGuard>::new_own(rep);
                    let _ = access.get().table.delete(resource)?;
                    Ok::<_, wasmtime::Error>(())
                })
            })
        },
    )?;
    instance.func_wrap(
        "[constructor]raw-mutex",
        |mut caller: StoreContextMut<'_, StoreData<CpuImpl, HostFs>>, (): ()| {
            let resource = caller.data_mut().table.push(SbiRawMutex {
                resource: RawMutexResource {
                    inner: Arc::new(RawMutex::new()),
                },
            })?;
            Ok((resource,))
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]raw-mutex.lock",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>, (resource,): (Resource<SbiRawMutex>,)| {
            Box::pin(async move {
                let mutex = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(
                        access.get().table.get(&resource)?.resource.inner.clone(),
                    )
                })?;
                let lease = mutex.lock_owned().await;
                accessor.with(|mut access| {
                    let guard = access.get().table.push(SbiRawMutexGuard {
                        _resource: RawMutexGuardResource { _lease: lease },
                    })?;
                    Ok::<_, wasmtime::Error>((guard,))
                })
            })
        },
    )?;
    instance.func_wrap(
        "[constructor]raw-rw-lock",
        |mut caller: StoreContextMut<'_, StoreData<CpuImpl, HostFs>>, (): ()| {
            let resource = caller.data_mut().table.push(SbiRawRwLock {
                resource: RawRwLockResource {
                    inner: Arc::new(RawRwLock::new()),
                },
            })?;
            Ok((resource,))
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]raw-rw-lock.read",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource,): (Resource<SbiRawRwLock>,)| {
            Box::pin(async move {
                let rwlock = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(
                        access.get().table.get(&resource)?.resource.inner.clone(),
                    )
                })?;
                let lease = rwlock.read_owned().await;
                accessor.with(|mut access| {
                    let guard = access.get().table.push(SbiRawRwLockReadGuard {
                        _resource: RawRwLockReadGuardResource { _lease: lease },
                    })?;
                    Ok::<_, wasmtime::Error>((guard,))
                })
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]raw-rw-lock.write",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource,): (Resource<SbiRawRwLock>,)| {
            Box::pin(async move {
                let rwlock = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(
                        access.get().table.get(&resource)?.resource.inner.clone(),
                    )
                })?;
                let lease = rwlock.write_owned().await;
                accessor.with(|mut access| {
                    let guard = access.get().table.push(SbiRawRwLockWriteGuard {
                        _resource: RawRwLockWriteGuardResource { _lease: lease },
                    })?;
                    Ok::<_, wasmtime::Error>((guard,))
                })
            })
        },
    )?;
    Ok(())
}

fn add_stats_to_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let mut instance = linker.instance(STATS_INSTANCE)?;
    instance.func_wrap("snapshot", |caller, (): ()| {
        Ok((snapshot_sample(caller.data()),))
    })?;
    instance.func_wrap("subscribe", |mut caller, (period,): (u64,)| {
        let reader = StreamReader::new(&mut caller, StatsStreamProducer::new(period))?;
        Ok((reader,))
    })?;
    Ok(())
}

fn add_programs_to_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    debugger_bindings::helios::system::programs::add_to_linker::<
        _,
        HasSelf<StoreData<CpuImpl, HostFs>>,
    >(linker, |state| state)?;
    Ok(())
}

fn add_programs_to_program_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    program_bindings::helios::system::programs::add_to_linker::<
        _,
        HasSelf<StoreData<CpuImpl, HostFs>>,
    >(linker, |state| state)?;
    Ok(())
}

fn dns_network_rights() -> NetworkAuthorityRights {
    NetworkAuthorityRights::DNS
}

fn tcp_connect_network_rights() -> NetworkAuthorityRights {
    NetworkAuthorityRights::TCP | NetworkAuthorityRights::DNS
}

fn udp_bind_network_rights(local_port: u16) -> NetworkAuthorityRights {
    if local_port != 0 && local_port < 1024 {
        NetworkAuthorityRights::UDP | NetworkAuthorityRights::PRIVILEGED_BIND
    } else {
        NetworkAuthorityRights::UDP
    }
}

fn add_net_to_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let mut instance = linker.instance(NET_INSTANCE)?;
    instance.resource_concurrent(
        "tcp-stream",
        ResourceType::host::<SbiTcpStream>(),
        |accessor, rep| {
            Box::pin(async move {
                let stream = accessor.with(|mut access| {
                    let resource = Resource::<SbiTcpStream>::new_own(rep);
                    let stream = access.get().table.delete(resource)?;
                    Ok::<_, wasmtime::Error>(stream)
                })?;
                stream
                    .resource
                    .backend
                    .service
                    .tcp_close(stream.resource.backend.stream)
                    .await;
                Ok::<_, wasmtime::Error>(())
            })
        },
    )?;
    instance.resource_concurrent(
        "udp-socket",
        ResourceType::host::<SbiUdpSocket>(),
        |accessor, rep| {
            Box::pin(async move {
                let socket = accessor.with(|mut access| {
                    let resource = Resource::<SbiUdpSocket>::new_own(rep);
                    let socket = access.get().table.delete(resource)?;
                    Ok::<_, wasmtime::Error>(socket)
                })?;
                socket
                    .resource
                    .backend
                    .service
                    .udp_close(socket.resource.backend.socket)
                    .await;
                Ok::<_, wasmtime::Error>(())
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "ping",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>, (host, timeout): (String, u64)| {
            Box::pin(async move {
                let has_authority = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(
                        access
                            .get()
                            .process_authority()
                            .network_rights()
                            .contains(dns_network_rights()),
                    )
                })?;
                if !has_authority {
                    return Ok::<_, wasmtime::Error>((Err(
                        debugger_bindings::helios::system::net::PingError {
                            kind:
                                debugger_bindings::helios::system::net::PingErrorKind::Unavailable,
                            detail: "network authority is missing DNS rights".to_owned(),
                        },
                    ),));
                }
                let service = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(access.get().runtime_state.network_service())
                })?;
                let Some(service) = service else {
                    return Ok::<_, wasmtime::Error>((Err(
                        debugger_bindings::helios::system::net::PingError {
                            kind:
                                debugger_bindings::helios::system::net::PingErrorKind::Unavailable,
                            detail: "network service is unavailable on this machine".to_owned(),
                        },
                    ),));
                };
                let response = service
                    .ping(&host, timeout)
                    .await
                    .map(convert_ping_reply)
                    .map_err(convert_ping_error);
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "tcp-connect",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (host, port, timeout): (String, u16, u64)| {
            Box::pin(async move {
                let has_authority = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(
                        access
                            .get()
                            .process_authority()
                            .network_rights()
                            .contains(tcp_connect_network_rights()),
                    )
                })?;
                if !has_authority {
                    return Ok::<_, wasmtime::Error>((Err(unavailable_tcp_authority_error()),));
                }
                let (service, profile) = accessor.with(|mut access| {
                    let store = access.get();
                    Ok::<_, wasmtime::Error>((
                        store.runtime_state.network_service(),
                        component_host_profile(store),
                    ))
                })?;
                let Some(service) = service else {
                    return Ok::<_, wasmtime::Error>((Err(unavailable_tcp_error()),));
                };
                let connected = service.tcp_connect(&host, port, timeout).await;
                record_component_host_kernel_profile(profile, "system-net-tcp-connect");
                let response = match connected {
                    Ok(stream) => {
                        let resource = accessor.with(|mut access| {
                            access
                                .get()
                                .table
                                .push(SbiTcpStream::new(NetworkTcpBackend {
                                    service: service.clone(),
                                    stream,
                                }))
                        })?;
                        Ok(resource)
                    }
                    Err(error) => Err(convert_tcp_error(error)),
                };
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "udp-bind",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>, (local_port,): (u16,)| {
            Box::pin(async move {
                let required = udp_bind_network_rights(local_port);
                let has_authority = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(
                        access
                            .get()
                            .process_authority()
                            .network_rights()
                            .contains(required),
                    )
                })?;
                if !has_authority {
                    return Ok::<_, wasmtime::Error>((Err(unavailable_udp_authority_error()),));
                }
                let service = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(access.get().runtime_state.network_service())
                })?;
                let Some(service) = service else {
                    return Ok::<_, wasmtime::Error>((Err(unavailable_udp_error()),));
                };
                let bound = service.udp_bind(local_port).await;
                let response = match bound {
                    Ok(binding) => {
                        let resource = accessor.with(|mut access| {
                            access
                                .get()
                                .table
                                .push(SbiUdpSocket::new(NetworkUdpBackend {
                                    service: service.clone(),
                                    socket: binding.socket,
                                }))
                        })?;
                        Ok(resource)
                    }
                    Err(error) => Err(convert_udp_error(error)),
                };
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]tcp-stream.read",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource, max_bytes, timeout): (Resource<SbiTcpStream>, u32, u64)| {
            Box::pin(async move {
                let (socket, profile) = accessor.with(|mut access| {
                    let socket = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((
                        (
                            socket.resource.backend.service.clone(),
                            socket.resource.backend.stream,
                        ),
                        component_host_profile(access.get()),
                    ))
                })?;
                let response = socket.0.tcp_read(socket.1, max_bytes, timeout).await;
                let bytes = response
                    .as_ref()
                    .ok()
                    .and_then(Option::as_ref)
                    .map_or(0, Bytes::len);
                let response = response
                    .map(|bytes| bytes.map(lower_bytes_to_vec))
                    .map_err(convert_tcp_error);
                record_component_host_kernel_profile_events_bytes(
                    profile,
                    "system-net-tcp-read",
                    1,
                    component_host_usize_to_u64(bytes, "system TCP read byte count"),
                );
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]tcp-stream.write",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource, bytes, timeout): (Resource<SbiTcpStream>, Vec<u8>, u64)| {
            Box::pin(async move {
                let (socket, profile) = accessor.with(|mut access| {
                    let socket = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((
                        (
                            socket.resource.backend.service.clone(),
                            socket.resource.backend.stream,
                        ),
                        component_host_profile(access.get()),
                    ))
                })?;
                let written = bytes.len() as u64;
                let response = socket
                    .0
                    .tcp_write_all_bytes(socket.1, Bytes::from(bytes), timeout)
                    .await
                    .map(|()| written)
                    .map_err(convert_tcp_error);
                record_component_host_kernel_profile(profile, "system-net-tcp-write");
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]tcp-stream.close",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource,): (Resource<SbiTcpStream>,)| {
            Box::pin(async move {
                let socket = accessor.with(|mut access| {
                    let socket = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((
                        socket.resource.backend.service.clone(),
                        socket.resource.backend.stream,
                    ))
                })?;
                socket.0.tcp_close(socket.1).await;
                Ok::<_, wasmtime::Error>((
                    Ok::<(), debugger_bindings::helios::system::net::TcpError>(()),
                ))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]udp-socket.receive",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource, max_bytes, timeout): (Resource<SbiUdpSocket>, u32, u64)| {
            Box::pin(async move {
                let socket = accessor.with(|mut access| {
                    let socket = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((
                        socket.resource.backend.service.clone(),
                        socket.resource.backend.socket,
                    ))
                })?;
                let response = socket
                    .0
                    .udp_receive(socket.1, max_bytes, timeout)
                    .await
                    .map(|datagram| datagram.map(convert_udp_datagram))
                    .map_err(convert_udp_error);
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]udp-socket.send",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource, host, port, bytes, timeout): (
            Resource<SbiUdpSocket>,
            String,
            u16,
            Vec<u8>,
            u64,
        )| {
            Box::pin(async move {
                let socket = accessor.with(|mut access| {
                    let socket = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((
                        socket.resource.backend.service.clone(),
                        socket.resource.backend.socket,
                    ))
                })?;
                let response = socket
                    .0
                    .udp_send(socket.1, &host, port, &bytes, timeout)
                    .await
                    .map_err(convert_udp_error);
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]udp-socket.close",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource,): (Resource<SbiUdpSocket>,)| {
            Box::pin(async move {
                let socket = accessor.with(|mut access| {
                    let socket = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((
                        socket.resource.backend.service.clone(),
                        socket.resource.backend.socket,
                    ))
                })?;
                socket.0.udp_close(socket.1).await;
                Ok::<_, wasmtime::Error>((
                    Ok::<(), debugger_bindings::helios::system::net::UdpError>(()),
                ))
            })
        },
    )?;
    Ok(())
}

fn add_net_to_program_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let mut instance = linker.instance(NET_INSTANCE)?;
    instance.resource_concurrent(
        "tcp-stream",
        ResourceType::host::<SbiTcpStream>(),
        |accessor, rep| {
            Box::pin(async move {
                let stream = accessor.with(|mut access| {
                    let resource = Resource::<SbiTcpStream>::new_own(rep);
                    let stream = access.get().table.delete(resource)?;
                    Ok::<_, wasmtime::Error>(stream)
                })?;
                stream
                    .resource
                    .backend
                    .service
                    .tcp_close(stream.resource.backend.stream)
                    .await;
                Ok::<_, wasmtime::Error>(())
            })
        },
    )?;
    instance.resource_concurrent(
        "udp-socket",
        ResourceType::host::<SbiUdpSocket>(),
        |accessor, rep| {
            Box::pin(async move {
                let socket = accessor.with(|mut access| {
                    let resource = Resource::<SbiUdpSocket>::new_own(rep);
                    let socket = access.get().table.delete(resource)?;
                    Ok::<_, wasmtime::Error>(socket)
                })?;
                socket
                    .resource
                    .backend
                    .service
                    .udp_close(socket.resource.backend.socket)
                    .await;
                Ok::<_, wasmtime::Error>(())
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "ping",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>, (host, timeout): (String, u64)| {
            Box::pin(async move {
                let has_authority = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(
                        access
                            .get()
                            .process_authority()
                            .network_rights()
                            .contains(dns_network_rights()),
                    )
                })?;
                if !has_authority {
                    return Ok::<_, wasmtime::Error>((Err(
                        program_bindings::helios::system::net::PingError {
                            kind: program_bindings::helios::system::net::PingErrorKind::Unavailable,
                            detail: "network authority is missing DNS rights".to_owned(),
                        },
                    ),));
                }
                let service = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(access.get().runtime_state.network_service())
                })?;
                let Some(service) = service else {
                    return Ok::<_, wasmtime::Error>((Err(
                        program_bindings::helios::system::net::PingError {
                            kind: program_bindings::helios::system::net::PingErrorKind::Unavailable,
                            detail: "network service is unavailable on this machine".to_owned(),
                        },
                    ),));
                };
                let response = service
                    .ping(&host, timeout)
                    .await
                    .map(convert_program_ping_reply)
                    .map_err(convert_program_ping_error);
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "tcp-connect",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (host, port, timeout): (String, u16, u64)| {
            Box::pin(async move {
                let has_authority = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(
                        access
                            .get()
                            .process_authority()
                            .network_rights()
                            .contains(tcp_connect_network_rights()),
                    )
                })?;
                if !has_authority {
                    return Ok::<_, wasmtime::Error>((Err(
                        unavailable_program_tcp_authority_error(),
                    ),));
                }
                let (service, profile) = accessor.with(|mut access| {
                    let store = access.get();
                    Ok::<_, wasmtime::Error>((
                        store.runtime_state.network_service(),
                        component_host_profile(store),
                    ))
                })?;
                let Some(service) = service else {
                    return Ok::<_, wasmtime::Error>((Err(unavailable_program_tcp_error()),));
                };
                let connected = service.tcp_connect(&host, port, timeout).await;
                record_component_host_kernel_profile(profile, "program-net-tcp-connect");
                let response = match connected {
                    Ok(stream) => {
                        let resource = accessor.with(|mut access| {
                            access
                                .get()
                                .table
                                .push(SbiTcpStream::new(NetworkTcpBackend {
                                    service: service.clone(),
                                    stream,
                                }))
                        })?;
                        Ok(resource)
                    }
                    Err(error) => Err(convert_program_tcp_error(error)),
                };
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "udp-bind",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>, (local_port,): (u16,)| {
            Box::pin(async move {
                let required = udp_bind_network_rights(local_port);
                let has_authority = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(
                        access
                            .get()
                            .process_authority()
                            .network_rights()
                            .contains(required),
                    )
                })?;
                if !has_authority {
                    return Ok::<_, wasmtime::Error>((Err(
                        unavailable_program_udp_authority_error(),
                    ),));
                }
                let service = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(access.get().runtime_state.network_service())
                })?;
                let Some(service) = service else {
                    return Ok::<_, wasmtime::Error>((Err(unavailable_program_udp_error()),));
                };
                let bound = service.udp_bind(local_port).await;
                let response = match bound {
                    Ok(binding) => {
                        let resource = accessor.with(|mut access| {
                            access
                                .get()
                                .table
                                .push(SbiUdpSocket::new(NetworkUdpBackend {
                                    service: service.clone(),
                                    socket: binding.socket,
                                }))
                        })?;
                        Ok(resource)
                    }
                    Err(error) => Err(convert_program_udp_error(error)),
                };
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]tcp-stream.read",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource, max_bytes, timeout): (Resource<SbiTcpStream>, u32, u64)| {
            Box::pin(async move {
                let (socket, profile) = accessor.with(|mut access| {
                    let socket = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((
                        (
                            socket.resource.backend.service.clone(),
                            socket.resource.backend.stream,
                        ),
                        component_host_profile(access.get()),
                    ))
                })?;
                let service_profile = profile.as_ref().map(ComponentHostProfile::restarted);
                let response = socket.0.tcp_read(socket.1, max_bytes, timeout).await;
                let bytes = response
                    .as_ref()
                    .ok()
                    .and_then(Option::as_ref)
                    .map_or(0, Bytes::len);
                record_component_host_kernel_profile_events_bytes(
                    service_profile,
                    "program-net-tcp-read-service",
                    1,
                    component_host_usize_to_u64(bytes, "program TCP read byte count"),
                );
                let lower_profile = profile.as_ref().map(ComponentHostProfile::restarted);
                let response = response
                    .map(|bytes| bytes.map(lower_bytes_to_vec))
                    .map_err(convert_program_tcp_error);
                record_component_host_kernel_profile_events_bytes(
                    lower_profile,
                    "program-net-tcp-read-lower-bytes",
                    1,
                    component_host_usize_to_u64(bytes, "program TCP read byte count"),
                );
                record_component_host_kernel_profile_events_bytes(
                    profile,
                    "program-net-tcp-read",
                    1,
                    component_host_usize_to_u64(bytes, "program TCP read byte count"),
                );
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]tcp-stream.write",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource, bytes, timeout): (Resource<SbiTcpStream>, Vec<u8>, u64)| {
            Box::pin(async move {
                let (socket, profile) = accessor.with(|mut access| {
                    let socket = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((
                        (
                            socket.resource.backend.service.clone(),
                            socket.resource.backend.stream,
                        ),
                        component_host_profile(access.get()),
                    ))
                })?;
                let written = bytes.len() as u64;
                let response = socket
                    .0
                    .tcp_write_all_bytes(socket.1, Bytes::from(bytes), timeout)
                    .await
                    .map(|()| written)
                    .map_err(convert_program_tcp_error);
                record_component_host_kernel_profile(profile, "program-net-tcp-write");
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]tcp-stream.close",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource,): (Resource<SbiTcpStream>,)| {
            Box::pin(async move {
                let socket = accessor.with(|mut access| {
                    let socket = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((
                        socket.resource.backend.service.clone(),
                        socket.resource.backend.stream,
                    ))
                })?;
                socket.0.tcp_close(socket.1).await;
                Ok::<_, wasmtime::Error>((
                    Ok::<(), program_bindings::helios::system::net::TcpError>(()),
                ))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]udp-socket.receive",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource, max_bytes, timeout): (Resource<SbiUdpSocket>, u32, u64)| {
            Box::pin(async move {
                let socket = accessor.with(|mut access| {
                    let socket = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((
                        socket.resource.backend.service.clone(),
                        socket.resource.backend.socket,
                    ))
                })?;
                let response = socket
                    .0
                    .udp_receive(socket.1, max_bytes, timeout)
                    .await
                    .map(|datagram| datagram.map(convert_program_udp_datagram))
                    .map_err(convert_program_udp_error);
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]udp-socket.send",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource, host, port, bytes, timeout): (
            Resource<SbiUdpSocket>,
            String,
            u16,
            Vec<u8>,
            u64,
        )| {
            Box::pin(async move {
                let socket = accessor.with(|mut access| {
                    let socket = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((
                        socket.resource.backend.service.clone(),
                        socket.resource.backend.socket,
                    ))
                })?;
                let response = socket
                    .0
                    .udp_send(socket.1, &host, port, &bytes, timeout)
                    .await
                    .map_err(convert_program_udp_error);
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]udp-socket.close",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource,): (Resource<SbiUdpSocket>,)| {
            Box::pin(async move {
                let socket = accessor.with(|mut access| {
                    let socket = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((
                        socket.resource.backend.service.clone(),
                        socket.resource.backend.socket,
                    ))
                })?;
                socket.0.udp_close(socket.1).await;
                Ok::<_, wasmtime::Error>((
                    Ok::<(), program_bindings::helios::system::net::UdpError>(()),
                ))
            })
        },
    )?;
    Ok(())
}

fn add_tracing_to_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let mut instance = linker.instance(TRACING_INSTANCE)?;
    instance.func_wrap(
        "recent",
        |caller, (filter, limit): (debugger_bindings::helios::system::tracing::Filter, u32)| {
            let filter = convert_filter(filter);
            let events = caller
                .data()
                .runtime_state
                .recent(&filter, limit)
                .into_iter()
                .map(convert_event)
                .collect::<Vec<_>>();
            Ok((events,))
        },
    )?;
    instance.func_wrap(
        "subscribe",
        |mut caller, (filter,): (debugger_bindings::helios::system::tracing::Filter,)| {
            let reader = StreamReader::new(
                &mut caller,
                TracingStreamProducer::new(convert_filter(filter)),
            )?;
            Ok((reader,))
        },
    )?;
    Ok(())
}

fn add_profiling_to_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let mut instance = linker.instance(PROFILING_INSTANCE)?;
    instance.func_wrap("set-enabled", |caller, (enabled,): (bool,)| {
        caller.data().runtime_state.set_profiling_enabled(enabled);
        Ok(())
    })?;
    instance.func_wrap("clear", |caller, (): ()| {
        caller.data().runtime_state.clear_profile();
        Ok(())
    })?;
    instance.func_wrap(
        "folded",
        |caller, (filter, limit): (debugger_bindings::helios::system::profiling::Filter, u32)| {
            let filter = convert_profile_filter(filter);
            let samples = caller
                .data()
                .runtime_state
                .folded_profile(caller.data().cpu.now().ticks(), &filter, limit)
                .into_iter()
                .map(convert_profile_sample)
                .collect::<Vec<_>>();
            Ok((samples,))
        },
    )?;
    instance.func_wrap(
        "metrics",
        |caller,
         (filter, limit): (
            debugger_bindings::helios::system::profiling::MetricFilter,
            u32,
        )| {
            let filter = convert_perf_metric_filter(filter);
            let samples = caller
                .data()
                .runtime_state
                .perf_metrics(&filter, limit)
                .into_iter()
                .map(convert_perf_metric_sample)
                .collect::<Vec<_>>();
            Ok((samples,))
        },
    )?;
    Ok(())
}

fn add_stats_to_program_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let mut instance = linker.instance(STATS_INSTANCE)?;
    instance.func_wrap("snapshot", |caller, (): ()| {
        Ok((snapshot_program_sample(caller.data()),))
    })?;
    instance.func_wrap("subscribe", |mut caller, (period,): (u64,)| {
        let reader = StreamReader::new(&mut caller, ProgramStatsStreamProducer::new(period))?;
        Ok((reader,))
    })?;
    Ok(())
}

fn add_tracing_to_program_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let mut instance = linker.instance(TRACING_INSTANCE)?;
    instance.func_wrap(
        "recent",
        |caller, (filter, limit): (program_bindings::helios::system::tracing::Filter, u32)| {
            let filter = convert_program_filter(filter);
            let events = caller
                .data()
                .runtime_state
                .recent(&filter, limit)
                .into_iter()
                .map(convert_program_event)
                .collect::<Vec<_>>();
            Ok((events,))
        },
    )?;
    instance.func_wrap(
        "subscribe",
        |mut caller, (filter,): (program_bindings::helios::system::tracing::Filter,)| {
            let reader = StreamReader::new(
                &mut caller,
                ProgramTracingStreamProducer::new(convert_program_filter(filter)),
            )?;
            Ok((reader,))
        },
    )?;
    Ok(())
}

fn convert_launch_error(
    error: crate::ProgramExecError,
) -> debugger_bindings::helios::system::programs::ExecError {
    debugger_bindings::helios::system::programs::ExecError {
        kind: match error.kind {
            ProgramExecErrorKind::InvalidBinary => {
                debugger_bindings::helios::system::programs::ExecErrorKind::InvalidBinary
            }
            ProgramExecErrorKind::MissingEntry => {
                debugger_bindings::helios::system::programs::ExecErrorKind::MissingEntry
            }
            ProgramExecErrorKind::UnsupportedImport => {
                debugger_bindings::helios::system::programs::ExecErrorKind::UnsupportedImport
            }
            ProgramExecErrorKind::InvalidSignature => {
                debugger_bindings::helios::system::programs::ExecErrorKind::InvalidSignature
            }
            ProgramExecErrorKind::InvalidPath => {
                debugger_bindings::helios::system::programs::ExecErrorKind::InvalidPath
            }
            ProgramExecErrorKind::PermissionDenied => {
                debugger_bindings::helios::system::programs::ExecErrorKind::PermissionDenied
            }
            ProgramExecErrorKind::InvalidHint => {
                debugger_bindings::helios::system::programs::ExecErrorKind::InvalidHint
            }
            ProgramExecErrorKind::OutOfMemory => {
                debugger_bindings::helios::system::programs::ExecErrorKind::OutOfMemory
            }
            ProgramExecErrorKind::Unavailable => {
                debugger_bindings::helios::system::programs::ExecErrorKind::Unavailable
            }
            ProgramExecErrorKind::Internal => {
                debugger_bindings::helios::system::programs::ExecErrorKind::Internal
            }
        },
        detail: error.detail.as_str().to_owned(),
    }
}

fn convert_launch_result(
    result: crate::ExecResult,
) -> debugger_bindings::helios::system::programs::ExecResult {
    debugger_bindings::helios::system::programs::ExecResult {
        instance_id: result.instance_id.raw(),
        exit_code: result.exit_code,
        output: debugger_bindings::helios::system::programs::ExecOutput {
            stdout: result.output.stdout,
            stderr: result.output.stderr,
        },
    }
}

fn convert_program_launch_error(
    error: crate::ProgramExecError,
) -> program_bindings::helios::system::programs::ExecError {
    program_bindings::helios::system::programs::ExecError {
        kind: match error.kind {
            ProgramExecErrorKind::InvalidBinary => {
                program_bindings::helios::system::programs::ExecErrorKind::InvalidBinary
            }
            ProgramExecErrorKind::MissingEntry => {
                program_bindings::helios::system::programs::ExecErrorKind::MissingEntry
            }
            ProgramExecErrorKind::UnsupportedImport => {
                program_bindings::helios::system::programs::ExecErrorKind::UnsupportedImport
            }
            ProgramExecErrorKind::InvalidSignature => {
                program_bindings::helios::system::programs::ExecErrorKind::InvalidSignature
            }
            ProgramExecErrorKind::InvalidPath => {
                program_bindings::helios::system::programs::ExecErrorKind::InvalidPath
            }
            ProgramExecErrorKind::PermissionDenied => {
                program_bindings::helios::system::programs::ExecErrorKind::PermissionDenied
            }
            ProgramExecErrorKind::InvalidHint => {
                program_bindings::helios::system::programs::ExecErrorKind::InvalidHint
            }
            ProgramExecErrorKind::OutOfMemory => {
                program_bindings::helios::system::programs::ExecErrorKind::OutOfMemory
            }
            ProgramExecErrorKind::Unavailable => {
                program_bindings::helios::system::programs::ExecErrorKind::Unavailable
            }
            ProgramExecErrorKind::Internal => {
                program_bindings::helios::system::programs::ExecErrorKind::Internal
            }
        },
        detail: error.detail.as_str().to_owned(),
    }
}

fn convert_program_launch_result(
    result: crate::ExecResult,
) -> program_bindings::helios::system::programs::ExecResult {
    program_bindings::helios::system::programs::ExecResult {
        instance_id: result.instance_id.raw(),
        exit_code: result.exit_code,
        output: program_bindings::helios::system::programs::ExecOutput {
            stdout: result.output.stdout,
            stderr: result.output.stderr,
        },
    }
}

fn convert_ip_address(
    address: crate::NetworkIpAddress,
) -> debugger_bindings::helios::system::net::IpAddress {
    match address {
        crate::NetworkIpAddress::Ipv4(address) => {
            let octets = address.octets();
            debugger_bindings::helios::system::net::IpAddress::Ipv4((
                octets[0], octets[1], octets[2], octets[3],
            ))
        }
        crate::NetworkIpAddress::Ipv6(address) => {
            debugger_bindings::helios::system::net::IpAddress::Ipv6(ipv6_address_groups(address))
        }
    }
}

fn convert_program_ip_address(
    address: crate::NetworkIpAddress,
) -> program_bindings::helios::system::net::IpAddress {
    match address {
        crate::NetworkIpAddress::Ipv4(address) => {
            let octets = address.octets();
            program_bindings::helios::system::net::IpAddress::Ipv4((
                octets[0], octets[1], octets[2], octets[3],
            ))
        }
        crate::NetworkIpAddress::Ipv6(address) => {
            program_bindings::helios::system::net::IpAddress::Ipv6(ipv6_address_groups(address))
        }
    }
}

fn convert_ping_reply(
    reply: crate::PingReply,
) -> debugger_bindings::helios::system::net::PingReply {
    debugger_bindings::helios::system::net::PingReply {
        address: convert_ip_address(reply.address),
        round_trip: reply.round_trip_nanos,
        payload_bytes: reply.payload_bytes,
    }
}

fn convert_program_ping_reply(
    reply: crate::PingReply,
) -> program_bindings::helios::system::net::PingReply {
    program_bindings::helios::system::net::PingReply {
        address: convert_program_ip_address(reply.address),
        round_trip: reply.round_trip_nanos,
        payload_bytes: reply.payload_bytes,
    }
}

fn convert_ping_error(
    error: crate::PingError,
) -> debugger_bindings::helios::system::net::PingError {
    debugger_bindings::helios::system::net::PingError {
        kind: match error.kind {
            crate::PingErrorKind::UnresolvedHost => {
                debugger_bindings::helios::system::net::PingErrorKind::UnresolvedHost
            }
            crate::PingErrorKind::Timeout => {
                debugger_bindings::helios::system::net::PingErrorKind::Timeout
            }
            crate::PingErrorKind::Unavailable => {
                debugger_bindings::helios::system::net::PingErrorKind::Unavailable
            }
            crate::PingErrorKind::Internal => {
                debugger_bindings::helios::system::net::PingErrorKind::Internal
            }
        },
        detail: error.detail.as_str().to_owned(),
    }
}

fn convert_program_ping_error(
    error: crate::PingError,
) -> program_bindings::helios::system::net::PingError {
    program_bindings::helios::system::net::PingError {
        kind: match error.kind {
            crate::PingErrorKind::UnresolvedHost => {
                program_bindings::helios::system::net::PingErrorKind::UnresolvedHost
            }
            crate::PingErrorKind::Timeout => {
                program_bindings::helios::system::net::PingErrorKind::Timeout
            }
            crate::PingErrorKind::Unavailable => {
                program_bindings::helios::system::net::PingErrorKind::Unavailable
            }
            crate::PingErrorKind::Internal => {
                program_bindings::helios::system::net::PingErrorKind::Internal
            }
        },
        detail: error.detail.as_str().to_owned(),
    }
}

fn unavailable_tcp_error() -> debugger_bindings::helios::system::net::TcpError {
    debugger_bindings::helios::system::net::TcpError {
        kind: debugger_bindings::helios::system::net::TcpErrorKind::Unavailable,
        detail: "network service is unavailable on this machine".to_owned(),
    }
}

fn unavailable_tcp_authority_error() -> debugger_bindings::helios::system::net::TcpError {
    debugger_bindings::helios::system::net::TcpError {
        kind: debugger_bindings::helios::system::net::TcpErrorKind::Unavailable,
        detail: "network authority is missing TCP or DNS rights".to_owned(),
    }
}

fn unavailable_program_tcp_error() -> program_bindings::helios::system::net::TcpError {
    program_bindings::helios::system::net::TcpError {
        kind: program_bindings::helios::system::net::TcpErrorKind::Unavailable,
        detail: "network service is unavailable on this machine".to_owned(),
    }
}

fn unavailable_program_tcp_authority_error() -> program_bindings::helios::system::net::TcpError {
    program_bindings::helios::system::net::TcpError {
        kind: program_bindings::helios::system::net::TcpErrorKind::Unavailable,
        detail: "network authority is missing TCP or DNS rights".to_owned(),
    }
}

fn unavailable_udp_error() -> debugger_bindings::helios::system::net::UdpError {
    debugger_bindings::helios::system::net::UdpError {
        kind: debugger_bindings::helios::system::net::UdpErrorKind::Unavailable,
        detail: "network service is unavailable on this machine".to_owned(),
    }
}

fn unavailable_udp_authority_error() -> debugger_bindings::helios::system::net::UdpError {
    debugger_bindings::helios::system::net::UdpError {
        kind: debugger_bindings::helios::system::net::UdpErrorKind::Unavailable,
        detail: "network authority is missing UDP or privileged-bind rights".to_owned(),
    }
}

fn unavailable_program_udp_error() -> program_bindings::helios::system::net::UdpError {
    program_bindings::helios::system::net::UdpError {
        kind: program_bindings::helios::system::net::UdpErrorKind::Unavailable,
        detail: "network service is unavailable on this machine".to_owned(),
    }
}

fn unavailable_program_udp_authority_error() -> program_bindings::helios::system::net::UdpError {
    program_bindings::helios::system::net::UdpError {
        kind: program_bindings::helios::system::net::UdpErrorKind::Unavailable,
        detail: "network authority is missing UDP or privileged-bind rights".to_owned(),
    }
}

fn convert_tcp_error(error: crate::TcpError) -> debugger_bindings::helios::system::net::TcpError {
    debugger_bindings::helios::system::net::TcpError {
        kind: match error.kind {
            crate::TcpErrorKind::UnresolvedHost => {
                debugger_bindings::helios::system::net::TcpErrorKind::UnresolvedHost
            }
            crate::TcpErrorKind::Timeout => {
                debugger_bindings::helios::system::net::TcpErrorKind::Timeout
            }
            crate::TcpErrorKind::ConnectionReset => {
                debugger_bindings::helios::system::net::TcpErrorKind::ConnectionReset
            }
            crate::TcpErrorKind::ConnectionAborted => {
                debugger_bindings::helios::system::net::TcpErrorKind::ConnectionAborted
            }
            crate::TcpErrorKind::PermissionDenied => {
                debugger_bindings::helios::system::net::TcpErrorKind::Unavailable
            }
            crate::TcpErrorKind::Unavailable => {
                debugger_bindings::helios::system::net::TcpErrorKind::Unavailable
            }
            crate::TcpErrorKind::Internal => {
                debugger_bindings::helios::system::net::TcpErrorKind::Internal
            }
        },
        detail: error.detail.as_str().to_owned(),
    }
}

fn convert_program_tcp_error(
    error: crate::TcpError,
) -> program_bindings::helios::system::net::TcpError {
    program_bindings::helios::system::net::TcpError {
        kind: match error.kind {
            crate::TcpErrorKind::UnresolvedHost => {
                program_bindings::helios::system::net::TcpErrorKind::UnresolvedHost
            }
            crate::TcpErrorKind::Timeout => {
                program_bindings::helios::system::net::TcpErrorKind::Timeout
            }
            crate::TcpErrorKind::ConnectionReset => {
                program_bindings::helios::system::net::TcpErrorKind::ConnectionReset
            }
            crate::TcpErrorKind::ConnectionAborted => {
                program_bindings::helios::system::net::TcpErrorKind::ConnectionAborted
            }
            crate::TcpErrorKind::PermissionDenied => {
                program_bindings::helios::system::net::TcpErrorKind::Unavailable
            }
            crate::TcpErrorKind::Unavailable => {
                program_bindings::helios::system::net::TcpErrorKind::Unavailable
            }
            crate::TcpErrorKind::Internal => {
                program_bindings::helios::system::net::TcpErrorKind::Internal
            }
        },
        detail: error.detail.as_str().to_owned(),
    }
}

fn convert_udp_datagram(
    datagram: crate::UdpDatagram,
) -> debugger_bindings::helios::system::net::UdpDatagram {
    debugger_bindings::helios::system::net::UdpDatagram {
        address: convert_ip_address(datagram.address),
        port: datagram.port,
        bytes: lower_bytes_to_vec(datagram.bytes),
    }
}

fn convert_program_udp_datagram(
    datagram: crate::UdpDatagram,
) -> program_bindings::helios::system::net::UdpDatagram {
    program_bindings::helios::system::net::UdpDatagram {
        address: convert_program_ip_address(datagram.address),
        port: datagram.port,
        bytes: lower_bytes_to_vec(datagram.bytes),
    }
}

fn convert_udp_error(error: crate::UdpError) -> debugger_bindings::helios::system::net::UdpError {
    debugger_bindings::helios::system::net::UdpError {
        kind: match error.kind {
            crate::UdpErrorKind::UnresolvedHost => {
                debugger_bindings::helios::system::net::UdpErrorKind::UnresolvedHost
            }
            crate::UdpErrorKind::Unsupported => {
                debugger_bindings::helios::system::net::UdpErrorKind::Unavailable
            }
            crate::UdpErrorKind::Timeout => {
                debugger_bindings::helios::system::net::UdpErrorKind::Timeout
            }
            crate::UdpErrorKind::PermissionDenied => {
                debugger_bindings::helios::system::net::UdpErrorKind::PermissionDenied
            }
            crate::UdpErrorKind::Unavailable => {
                debugger_bindings::helios::system::net::UdpErrorKind::Unavailable
            }
            crate::UdpErrorKind::Internal => {
                debugger_bindings::helios::system::net::UdpErrorKind::Internal
            }
        },
        detail: error.detail.as_str().to_owned(),
    }
}

fn convert_program_udp_error(
    error: crate::UdpError,
) -> program_bindings::helios::system::net::UdpError {
    program_bindings::helios::system::net::UdpError {
        kind: match error.kind {
            crate::UdpErrorKind::UnresolvedHost => {
                program_bindings::helios::system::net::UdpErrorKind::UnresolvedHost
            }
            crate::UdpErrorKind::Unsupported => {
                program_bindings::helios::system::net::UdpErrorKind::Unavailable
            }
            crate::UdpErrorKind::Timeout => {
                program_bindings::helios::system::net::UdpErrorKind::Timeout
            }
            crate::UdpErrorKind::PermissionDenied => {
                program_bindings::helios::system::net::UdpErrorKind::PermissionDenied
            }
            crate::UdpErrorKind::Unavailable => {
                program_bindings::helios::system::net::UdpErrorKind::Unavailable
            }
            crate::UdpErrorKind::Internal => {
                program_bindings::helios::system::net::UdpErrorKind::Internal
            }
        },
        detail: error.detail.as_str().to_owned(),
    }
}

fn add_instances_to_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let mut instance = linker.instance(INSTANCES_INSTANCE)?;
    instance.func_wrap("snapshot", |caller, (): ()| {
        Ok((caller
            .data()
            .instance_registry
            .snapshot(caller.data().now_nanos())
            .into_iter()
            .map(convert_instance)
            .collect::<Vec<_>>(),))
    })?;
    Ok(())
}

impl<CpuImpl, HostFs> debugger_bindings::helios::system::serial::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs, U> debugger_bindings::helios::system::serial::HostWithStore<U>
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn debug_port(
        accessor: &Accessor<U, Self>,
    ) -> wasmtime::Result<Option<Resource<SbiSerialPort>>> {
        accessor.with(|mut access| match access.get().debug_port() {
            Some(()) => Ok(Some(access.get().table.push(SbiSerialPort {
                _resource: SerialPortResource,
            })?)),
            None => Ok(None),
        })
    }
}

impl<CpuImpl, HostFs> debugger_bindings::helios::system::serial::HostSerialPort
    for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs, U> debugger_bindings::helios::system::serial::HostSerialPortWithStore<U>
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn rights(
        accessor: &Accessor<U, Self>,
        resource: Resource<SbiSerialPort>,
    ) -> wasmtime::Result<debugger_bindings::helios::system::serial::SerialRights> {
        accessor.with(|mut access| {
            let _ = access.get().table.get(&resource)?;
            Ok::<_, wasmtime::Error>(
                debugger_bindings::helios::system::serial::SerialRights::READ
                    | debugger_bindings::helios::system::serial::SerialRights::WRITE
                    | debugger_bindings::helios::system::serial::SerialRights::FLUSH,
            )
        })
    }

    async fn drop(
        accessor: &Accessor<U, Self>,
        resource: Resource<SbiSerialPort>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            let _ = access.get().table.delete(resource)?;
            Ok::<_, wasmtime::Error>(())
        })
    }

    async fn read(
        accessor: &Accessor<U, Self>,
        resource: Resource<SbiSerialPort>,
        max_bytes: u32,
    ) -> wasmtime::Result<Vec<u8>> {
        accessor.with(|mut access| {
            let _ = access.get().table.get(&resource)?;
            Ok::<_, wasmtime::Error>(())
        })?;
        // Poll the non-blocking serial reader and yield to the kernel
        // executor between polls so host-fs transport and other tasks keep
        // making progress while we wait for input.
        loop {
            let bytes = accessor.with(|mut access| access.get().try_read_serial_port(max_bytes));
            if !bytes.is_empty() {
                return Ok(bytes);
            }
            crate::yield_now().await;
        }
    }

    async fn write(
        accessor: &Accessor<U, Self>,
        resource: Resource<SbiSerialPort>,
        bytes: Vec<u8>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            let _ = access.get().table.get(&resource)?;
            access.get().write_serial(&bytes);
            Ok::<_, wasmtime::Error>(())
        })?;
        Ok(())
    }

    async fn flush(
        accessor: &Accessor<U, Self>,
        resource: Resource<SbiSerialPort>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            let _ = access.get().table.get(&resource)?;
            Ok::<_, wasmtime::Error>(())
        })?;
        Ok(())
    }
}

impl<CpuImpl, HostFs> debugger_bindings::helios::system::sync::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs> debugger_bindings::helios::system::sync::HostRawMutex
    for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs, U> debugger_bindings::helios::system::sync::HostRawMutexWithStore<U>
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn new(
        mut access: Access<'_, U, Self>,
    ) -> impl core::future::Future<Output = wasmtime::Result<Resource<SbiRawMutex>>> + Send {
        // `Access` is not `Send`; do the table work synchronously and hand
        // back a ready future that only captures the `Send` result.
        let pushed = access.get().table.push(SbiRawMutex {
            resource: RawMutexResource {
                inner: Arc::new(RawMutex::new()),
            },
        });
        async move { Ok(pushed?) }
    }

    async fn drop(
        accessor: &Accessor<U, Self>,
        resource: Resource<SbiRawMutex>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            let _ = access.get().table.delete(resource)?;
            Ok::<_, wasmtime::Error>(())
        })
    }

    async fn lock(
        accessor: &Accessor<U, Self>,
        resource: Resource<SbiRawMutex>,
    ) -> wasmtime::Result<Resource<SbiRawMutexGuard>> {
        let mutex = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table.get(&resource)?.resource.inner.clone())
        })?;
        let lease = mutex.lock_owned().await;
        accessor.with(|mut access| {
            Ok(access.get().table.push(SbiRawMutexGuard {
                _resource: RawMutexGuardResource { _lease: lease },
            })?)
        })
    }
}

impl<CpuImpl, HostFs> debugger_bindings::helios::system::sync::HostRawMutexGuard
    for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs, U> debugger_bindings::helios::system::sync::HostRawMutexGuardWithStore<U>
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn drop(
        accessor: &Accessor<U, Self>,
        resource: Resource<SbiRawMutexGuard>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            let _ = access.get().table.delete(resource)?;
            Ok::<_, wasmtime::Error>(())
        })
    }
}

impl<CpuImpl, HostFs> debugger_bindings::helios::system::sync::HostRawRwLock
    for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs, U> debugger_bindings::helios::system::sync::HostRawRwLockWithStore<U>
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn new(
        mut access: Access<'_, U, Self>,
    ) -> impl core::future::Future<Output = wasmtime::Result<Resource<SbiRawRwLock>>> + Send {
        let pushed = access.get().table.push(SbiRawRwLock {
            resource: RawRwLockResource {
                inner: Arc::new(RawRwLock::new()),
            },
        });
        async move { Ok(pushed?) }
    }

    async fn drop(
        accessor: &Accessor<U, Self>,
        resource: Resource<SbiRawRwLock>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            let _ = access.get().table.delete(resource)?;
            Ok::<_, wasmtime::Error>(())
        })
    }

    async fn read(
        accessor: &Accessor<U, Self>,
        resource: Resource<SbiRawRwLock>,
    ) -> wasmtime::Result<Resource<SbiRawRwLockReadGuard>> {
        let rwlock = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table.get(&resource)?.resource.inner.clone())
        })?;
        let lease = rwlock.read_owned().await;
        accessor.with(|mut access| {
            Ok(access.get().table.push(SbiRawRwLockReadGuard {
                _resource: RawRwLockReadGuardResource { _lease: lease },
            })?)
        })
    }

    async fn write(
        accessor: &Accessor<U, Self>,
        resource: Resource<SbiRawRwLock>,
    ) -> wasmtime::Result<Resource<SbiRawRwLockWriteGuard>> {
        let rwlock = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table.get(&resource)?.resource.inner.clone())
        })?;
        let lease = rwlock.write_owned().await;
        accessor.with(|mut access| {
            Ok(access.get().table.push(SbiRawRwLockWriteGuard {
                _resource: RawRwLockWriteGuardResource { _lease: lease },
            })?)
        })
    }
}

impl<CpuImpl, HostFs> debugger_bindings::helios::system::sync::HostRawRwLockReadGuard
    for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs, U> debugger_bindings::helios::system::sync::HostRawRwLockReadGuardWithStore<U>
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn drop(
        accessor: &Accessor<U, Self>,
        resource: Resource<SbiRawRwLockReadGuard>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            let _ = access.get().table.delete(resource)?;
            Ok::<_, wasmtime::Error>(())
        })
    }
}

impl<CpuImpl, HostFs> debugger_bindings::helios::system::sync::HostRawRwLockWriteGuard
    for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs, U>
    debugger_bindings::helios::system::sync::HostRawRwLockWriteGuardWithStore<U>
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn drop(
        accessor: &Accessor<U, Self>,
        resource: Resource<SbiRawRwLockWriteGuard>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            let _ = access.get().table.delete(resource)?;
            Ok::<_, wasmtime::Error>(())
        })
    }
}

struct StatsStreamProducer {
    period_nanos: u64,
    next_due: Option<u64>,
}

impl StatsStreamProducer {
    fn new(period_nanos: u64) -> Self {
        assert!(period_nanos != 0, "stats subscribe period must be non-zero");
        Self {
            period_nanos,
            next_due: None,
        }
    }
}

impl<CpuImpl, HostFs> StreamProducer<StoreData<CpuImpl, HostFs>> for StatsStreamProducer
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    type Item = debugger_bindings::helios::system::stats::Sample;
    type Buffer = Option<Self::Item>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        store: StoreContextMut<'_, StoreData<CpuImpl, HostFs>>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }

        let now = store.data().now_nanos();
        let period_nanos = self.period_nanos;
        let due = self
            .next_due
            .get_or_insert_with(|| now.saturating_add(period_nanos));
        if now < *due {
            return Poll::Pending;
        }

        *due = now.saturating_add(period_nanos);
        destination.set_buffer(Some(snapshot_sample(store.data())));
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

struct ProgramStatsStreamProducer {
    period_nanos: u64,
    next_due: Option<u64>,
}

impl ProgramStatsStreamProducer {
    fn new(period_nanos: u64) -> Self {
        assert!(period_nanos != 0, "stats subscribe period must be non-zero");
        Self {
            period_nanos,
            next_due: None,
        }
    }
}

impl<CpuImpl, HostFs> StreamProducer<StoreData<CpuImpl, HostFs>> for ProgramStatsStreamProducer
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    type Item = program_bindings::helios::system::stats::Sample;
    type Buffer = Option<Self::Item>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        store: StoreContextMut<'_, StoreData<CpuImpl, HostFs>>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }

        let now = store.data().now_nanos();
        let period_nanos = self.period_nanos;
        let due = self
            .next_due
            .get_or_insert_with(|| now.saturating_add(period_nanos));
        if now < *due {
            return Poll::Pending;
        }

        *due = now.saturating_add(period_nanos);
        destination.set_buffer(Some(snapshot_program_sample(store.data())));
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

struct TracingStreamProducer {
    filter: TraceFilter,
    cursor: u64,
}

impl TracingStreamProducer {
    fn new(filter: TraceFilter) -> Self {
        Self { filter, cursor: 0 }
    }
}

impl<CpuImpl, HostFs> StreamProducer<StoreData<CpuImpl, HostFs>> for TracingStreamProducer
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    type Item = debugger_bindings::helios::system::tracing::Event;
    type Buffer = Option<Self::Item>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        store: StoreContextMut<'_, StoreData<CpuImpl, HostFs>>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }

        match store
            .data()
            .runtime_state
            .next_after(self.cursor, &self.filter)
        {
            Some((seq, event)) => {
                self.cursor = seq;
                destination.set_buffer(Some(convert_event(event)));
                Poll::Ready(Ok(StreamResult::Completed))
            }
            None => Poll::Pending,
        }
    }
}

struct ProgramTracingStreamProducer {
    filter: TraceFilter,
    cursor: u64,
}

impl ProgramTracingStreamProducer {
    fn new(filter: TraceFilter) -> Self {
        Self { filter, cursor: 0 }
    }
}

impl<CpuImpl, HostFs> StreamProducer<StoreData<CpuImpl, HostFs>> for ProgramTracingStreamProducer
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    type Item = program_bindings::helios::system::tracing::Event;
    type Buffer = Option<Self::Item>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        store: StoreContextMut<'_, StoreData<CpuImpl, HostFs>>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }

        match store
            .data()
            .runtime_state
            .next_after(self.cursor, &self.filter)
        {
            Some((seq, event)) => {
                self.cursor = seq;
                destination.set_buffer(Some(convert_program_event(event)));
                Poll::Ready(Ok(StreamResult::Completed))
            }
            None => Poll::Pending,
        }
    }
}

fn snapshot_sample<CpuImpl, HostFs>(
    store: &StoreData<CpuImpl, HostFs>,
) -> debugger_bindings::helios::system::stats::Sample
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    convert_sample(store.runtime_state.snapshot(store.cpu.now().ticks()))
}

fn snapshot_program_sample<CpuImpl, HostFs>(
    store: &StoreData<CpuImpl, HostFs>,
) -> program_bindings::helios::system::stats::Sample
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    convert_program_sample(store.runtime_state.snapshot(store.cpu.now().ticks()))
}

/// Maps the kernel's block-device snapshot onto one binding set's
/// `block-device` record.
///
/// The debugger world and the program world generate distinct Rust types
/// for the same WIT record, so the field mapping is written once here and
/// instantiated for each of them.
/// Maps the kernel's IOMMU snapshot onto one binding set's `iommu`
/// record.
///
/// The two generated worlds carry the same WIT record as distinct Rust
/// types, so the mapping is written once and instantiated for each.
macro_rules! convert_iommu_stats {
    ($bindings:path, $iommu:expr) => {
        $iommu.map(|iommu: crate::IommuStats| {
            use $bindings as stats_bindings;
            stats_bindings::Iommu {
                granule_bytes: iommu.granule_bytes,
                global_bypass: iommu.global_bypass,
                faults: iommu.faults,
                endpoints: iommu
                    .endpoints()
                    .iter()
                    .map(|endpoint| stats_bindings::IommuEndpoint {
                        endpoint: endpoint.endpoint,
                        domain: endpoint.domain,
                        mapped_bytes: endpoint.mapped_bytes,
                    })
                    .collect(),
            }
        })
    };
}

/// Maps the host share's cache counters onto one binding set's
/// `host-share-cache` record.
macro_rules! convert_host_share_stats {
    ($bindings:path, $host_share:expr) => {
        $host_share.map(|cache: crate::HostFsCacheStats| {
            use $bindings as stats_bindings;
            stats_bindings::HostShareCache {
                attribute_hits: cache.attribute_hits,
                attribute_misses: cache.attribute_misses,
                negative_hits: cache.negative_hits,
                directory_hits: cache.directory_hits,
                directory_misses: cache.directory_misses,
                fid_hits: cache.fid_hits,
                fid_misses: cache.fid_misses,
                evictions: cache.evictions,
                invalidations: cache.invalidations,
            }
        })
    };
}

macro_rules! convert_network_stats {
    ($bindings:path, $network:expr) => {
        $network.map(|network: crate::NetworkStats| {
            use $bindings as stats_bindings;
            stats_bindings::Network {
                queues: network
                    .queues
                    .into_iter()
                    .map(|queue| stats_bindings::NetworkQueue {
                        id: queue.id,
                        rx_frames: queue.rx_frames,
                        tx_frames: queue.tx_frames,
                        interrupts: queue.interrupts,
                    })
                    .collect(),
            }
        })
    };
}

macro_rules! convert_block_stats {
    ($bindings:path, $block:expr) => {
        $block.map(|block: crate::BlockStats| {
            use $bindings as stats_bindings;
            let capabilities =
                helios_hal::fs::BlockDeviceCapabilities::from_bits_truncate(block.capabilities);
            stats_bindings::BlockDevice {
                capacity_bytes: block.capacity_bytes,
                block_bytes: block.block_bytes,
                physical_block_bytes: block.physical_block_bytes,
                flush: capabilities.contains(helios_hal::fs::BlockDeviceCapabilities::FLUSH),
                discard: capabilities.contains(helios_hal::fs::BlockDeviceCapabilities::DISCARD),
                write_zeroes: capabilities
                    .contains(helios_hal::fs::BlockDeviceCapabilities::WRITE_ZEROES),
                queues: block.queues,
                queue_depth: block.queue_depth,
                reads: block.reads,
                writes: block.writes,
                flushes: block.flushes,
                discards: block.discards,
                write_zeroes_requests: block.write_zeroes,
            }
        })
    };
}

/// Maps the kernel's balloon snapshot onto one binding set's
/// `memory-balloon` record, for the same reason
/// [`convert_block_stats`] exists.
macro_rules! convert_balloon_stats {
    ($bindings:path, $balloon:expr) => {
        $balloon.map(|balloon: crate::BalloonStats| {
            use $bindings as stats_bindings;
            stats_bindings::MemoryBalloon {
                target_bytes: balloon.target_bytes,
                actual_bytes: balloon.actual_bytes,
                reported_bytes: balloon.reported_bytes,
            }
        })
    };
}

/// Maps the kernel's swap snapshot onto one binding set's `swap`
/// record, for the same reason [`convert_block_stats`] exists.
macro_rules! convert_swap_stats {
    ($bindings:path, $swap:expr) => {
        $swap.map(|swap: crate::SwapStats| {
            use $bindings as stats_bindings;
            stats_bindings::Swap {
                backend: alloc::string::String::from(swap.backend),
                capacity_bytes: swap.capacity_bytes,
                used_bytes: swap.used_bytes,
                pages_out: swap.pages_out,
                pages_in: swap.pages_in,
                faults_served: swap.faults_served,
                mean_fault_latency: swap.mean_fault_latency_nanos,
            }
        })
    };
}

fn convert_sample(sample: StatsSample) -> debugger_bindings::helios::system::stats::Sample {
    let heap = heap_stats();
    let total_bytes =
        u64::try_from(heap.total_bytes).expect("kernel heap total bytes do not fit into u64");
    let available_bytes = u64::try_from(heap.available_bytes())
        .expect("kernel heap available bytes do not fit into u64");
    debugger_bindings::helios::system::stats::Sample {
        timestamp: sample.timestamp,
        uptime: sample.uptime,
        wall_clock: sample.wall_clock,
        processors: debugger_bindings::helios::system::stats::Processors {
            configured: sample.configured_processors,
            online: sample.online_processors,
            utilization: (0..sample.configured_processors)
                .map(|id| debugger_bindings::helios::system::stats::Processor { id, busy: 0 })
                .collect(),
        },
        memory: debugger_bindings::helios::system::stats::Memory {
            total_bytes,
            available_bytes,
            pressure: convert_memory_pressure(total_bytes, available_bytes),
        },
        block: convert_block_stats!(debugger_bindings::helios::system::stats, sample.block),
        iommu: convert_iommu_stats!(debugger_bindings::helios::system::stats, sample.iommu),
        balloon: convert_balloon_stats!(debugger_bindings::helios::system::stats, sample.balloon),
        swap: convert_swap_stats!(debugger_bindings::helios::system::stats, sample.swap),
        host_share: convert_host_share_stats!(
            debugger_bindings::helios::system::stats,
            sample.host_share
        ),
        network: convert_network_stats!(debugger_bindings::helios::system::stats, sample.network),
    }
}

fn convert_program_sample(sample: StatsSample) -> program_bindings::helios::system::stats::Sample {
    let heap = heap_stats();
    let total_bytes =
        u64::try_from(heap.total_bytes).expect("kernel heap total bytes do not fit into u64");
    let available_bytes = u64::try_from(heap.available_bytes())
        .expect("kernel heap available bytes do not fit into u64");
    program_bindings::helios::system::stats::Sample {
        timestamp: sample.timestamp,
        uptime: sample.uptime,
        wall_clock: sample.wall_clock,
        processors: program_bindings::helios::system::stats::Processors {
            configured: sample.configured_processors,
            online: sample.online_processors,
            utilization: (0..sample.configured_processors)
                .map(|id| program_bindings::helios::system::stats::Processor { id, busy: 0 })
                .collect(),
        },
        memory: program_bindings::helios::system::stats::Memory {
            total_bytes,
            available_bytes,
            pressure: convert_program_memory_pressure(total_bytes, available_bytes),
        },
        block: convert_block_stats!(program_bindings::helios::system::stats, sample.block),
        iommu: convert_iommu_stats!(program_bindings::helios::system::stats, sample.iommu),
        balloon: convert_balloon_stats!(program_bindings::helios::system::stats, sample.balloon),
        swap: convert_swap_stats!(program_bindings::helios::system::stats, sample.swap),
        host_share: convert_host_share_stats!(
            program_bindings::helios::system::stats,
            sample.host_share
        ),
        network: convert_network_stats!(program_bindings::helios::system::stats, sample.network),
    }
}

fn convert_instance(
    instance: crate::InstanceSnapshot,
) -> debugger_bindings::helios::system::instances::Instance {
    debugger_bindings::helios::system::instances::Instance {
        id: instance.id.raw(),
        name: instance.name,
        started_at: instance.started_at,
        uptime: instance.uptime,
        memory_bytes: instance.memory_bytes,
        cpu_busy: instance.cpu_busy,
    }
}

fn convert_memory_pressure(
    total_bytes: u64,
    available_bytes: u64,
) -> debugger_bindings::helios::system::stats::MemoryPressure {
    if total_bytes == 0 {
        return debugger_bindings::helios::system::stats::MemoryPressure::Nominal;
    }

    let used_permille =
        ((total_bytes.saturating_sub(available_bytes.min(total_bytes))) * 1_000) / total_bytes;

    match used_permille {
        0..=699 => debugger_bindings::helios::system::stats::MemoryPressure::Nominal,
        700..=849 => debugger_bindings::helios::system::stats::MemoryPressure::Elevated,
        850..=949 => debugger_bindings::helios::system::stats::MemoryPressure::High,
        _ => debugger_bindings::helios::system::stats::MemoryPressure::Critical,
    }
}

fn convert_program_memory_pressure(
    total_bytes: u64,
    available_bytes: u64,
) -> program_bindings::helios::system::stats::MemoryPressure {
    if total_bytes == 0 {
        return program_bindings::helios::system::stats::MemoryPressure::Nominal;
    }

    let used_permille =
        ((total_bytes.saturating_sub(available_bytes.min(total_bytes))) * 1_000) / total_bytes;

    match used_permille {
        0..=699 => program_bindings::helios::system::stats::MemoryPressure::Nominal,
        700..=849 => program_bindings::helios::system::stats::MemoryPressure::Elevated,
        850..=949 => program_bindings::helios::system::stats::MemoryPressure::High,
        _ => program_bindings::helios::system::stats::MemoryPressure::Critical,
    }
}

fn convert_filter(filter: debugger_bindings::helios::system::tracing::Filter) -> TraceFilter {
    TraceFilter {
        min_level: filter.min_level.map(convert_level_to_local),
        target_prefixes: filter.target_prefixes,
    }
}

fn convert_program_filter(
    filter: program_bindings::helios::system::tracing::Filter,
) -> TraceFilter {
    TraceFilter {
        min_level: filter.min_level.map(convert_program_level_to_local),
        target_prefixes: filter.target_prefixes,
    }
}

fn convert_profile_filter(
    filter: debugger_bindings::helios::system::profiling::Filter,
) -> ProfileFilter {
    ProfileFilter {
        scope: filter.scope.map(convert_profile_scope_to_local),
        stack_prefixes: filter.stack_prefixes,
    }
}

fn convert_profile_sample(
    sample: crate::FoldedProfileSample,
) -> debugger_bindings::helios::system::profiling::FoldedSample {
    debugger_bindings::helios::system::profiling::FoldedSample {
        scope: convert_profile_scope_from_local(sample.scope),
        stack: sample.stack,
        weight: sample.weight,
    }
}

fn convert_perf_metric_filter(
    filter: debugger_bindings::helios::system::profiling::MetricFilter,
) -> PerfMetricFilter {
    PerfMetricFilter {
        name_prefixes: filter.name_prefixes,
    }
}

fn convert_perf_metric_sample(
    sample: crate::PerfMetricSample,
) -> debugger_bindings::helios::system::profiling::MetricSample {
    debugger_bindings::helios::system::profiling::MetricSample {
        scope: convert_profile_scope_from_local(sample.scope),
        name: sample.name,
        count: sample.count,
        total_events: sample.total_events,
        total_nanos: sample.total_nanos,
        min_nanos: sample.min_nanos,
        max_nanos: sample.max_nanos,
        total_bytes: sample.total_bytes,
        total_reference_cycles: sample.total_reference_cycles,
        total_cpu_cycles: sample.total_cpu_cycles,
        total_instructions_retired: sample.total_instructions_retired,
    }
}

fn convert_event(event: TraceEvent) -> debugger_bindings::helios::system::tracing::Event {
    debugger_bindings::helios::system::tracing::Event {
        timestamp: event.timestamp,
        level: convert_level_from_local(event.level),
        target: event.target,
        fields: event.fields.into_iter().map(convert_field).collect(),
    }
}

fn convert_program_event(event: TraceEvent) -> program_bindings::helios::system::tracing::Event {
    program_bindings::helios::system::tracing::Event {
        timestamp: event.timestamp,
        level: convert_program_level_from_local(event.level),
        target: event.target,
        fields: event
            .fields
            .into_iter()
            .map(convert_program_field)
            .collect(),
    }
}

fn convert_field(field: TraceField) -> debugger_bindings::helios::system::tracing::Field {
    debugger_bindings::helios::system::tracing::Field {
        key: field.key,
        value: convert_value(field.value),
    }
}

fn convert_program_field(field: TraceField) -> program_bindings::helios::system::tracing::Field {
    program_bindings::helios::system::tracing::Field {
        key: field.key,
        value: convert_program_value(field.value),
    }
}

fn convert_value(value: TraceValue) -> debugger_bindings::helios::system::tracing::Value {
    match value {
        TraceValue::Boolean(value) => {
            debugger_bindings::helios::system::tracing::Value::Boolean(value)
        }
        TraceValue::Signed64(value) => {
            debugger_bindings::helios::system::tracing::Value::Signed64(value)
        }
        TraceValue::Unsigned64(value) => {
            debugger_bindings::helios::system::tracing::Value::Unsigned64(value)
        }
        TraceValue::Float64(value) => {
            debugger_bindings::helios::system::tracing::Value::Float64(value)
        }
        TraceValue::Text(value) => debugger_bindings::helios::system::tracing::Value::Text(value),
        TraceValue::Blob(value) => debugger_bindings::helios::system::tracing::Value::Blob(value),
    }
}

fn convert_program_value(value: TraceValue) -> program_bindings::helios::system::tracing::Value {
    match value {
        TraceValue::Boolean(value) => {
            program_bindings::helios::system::tracing::Value::Boolean(value)
        }
        TraceValue::Signed64(value) => {
            program_bindings::helios::system::tracing::Value::Signed64(value)
        }
        TraceValue::Unsigned64(value) => {
            program_bindings::helios::system::tracing::Value::Unsigned64(value)
        }
        TraceValue::Float64(value) => {
            program_bindings::helios::system::tracing::Value::Float64(value)
        }
        TraceValue::Text(value) => program_bindings::helios::system::tracing::Value::Text(value),
        TraceValue::Blob(value) => program_bindings::helios::system::tracing::Value::Blob(value),
    }
}

fn convert_profile_scope_from_local(
    scope: ProfileScope,
) -> debugger_bindings::helios::system::profiling::Scope {
    match scope {
        ProfileScope::Kernel => debugger_bindings::helios::system::profiling::Scope::Kernel,
        ProfileScope::User => debugger_bindings::helios::system::profiling::Scope::User,
    }
}

fn convert_profile_scope_to_local(
    scope: debugger_bindings::helios::system::profiling::Scope,
) -> ProfileScope {
    match scope {
        debugger_bindings::helios::system::profiling::Scope::Kernel => ProfileScope::Kernel,
        debugger_bindings::helios::system::profiling::Scope::User => ProfileScope::User,
    }
}

fn convert_level_from_local(
    level: TraceLevel,
) -> debugger_bindings::helios::system::tracing::Level {
    match level {
        TraceLevel::Error => debugger_bindings::helios::system::tracing::Level::Error,
        TraceLevel::Warn => debugger_bindings::helios::system::tracing::Level::Warn,
        TraceLevel::Info => debugger_bindings::helios::system::tracing::Level::Info,
        TraceLevel::Debug => debugger_bindings::helios::system::tracing::Level::Debug,
        TraceLevel::Trace => debugger_bindings::helios::system::tracing::Level::Trace,
    }
}

fn convert_level_to_local(level: debugger_bindings::helios::system::tracing::Level) -> TraceLevel {
    match level {
        debugger_bindings::helios::system::tracing::Level::Error => TraceLevel::Error,
        debugger_bindings::helios::system::tracing::Level::Warn => TraceLevel::Warn,
        debugger_bindings::helios::system::tracing::Level::Info => TraceLevel::Info,
        debugger_bindings::helios::system::tracing::Level::Debug => TraceLevel::Debug,
        debugger_bindings::helios::system::tracing::Level::Trace => TraceLevel::Trace,
    }
}

fn convert_program_level_from_local(
    level: TraceLevel,
) -> program_bindings::helios::system::tracing::Level {
    match level {
        TraceLevel::Error => program_bindings::helios::system::tracing::Level::Error,
        TraceLevel::Warn => program_bindings::helios::system::tracing::Level::Warn,
        TraceLevel::Info => program_bindings::helios::system::tracing::Level::Info,
        TraceLevel::Debug => program_bindings::helios::system::tracing::Level::Debug,
        TraceLevel::Trace => program_bindings::helios::system::tracing::Level::Trace,
    }
}

fn convert_program_level_to_local(
    level: program_bindings::helios::system::tracing::Level,
) -> TraceLevel {
    match level {
        program_bindings::helios::system::tracing::Level::Error => TraceLevel::Error,
        program_bindings::helios::system::tracing::Level::Warn => TraceLevel::Warn,
        program_bindings::helios::system::tracing::Level::Info => TraceLevel::Info,
        program_bindings::helios::system::tracing::Level::Debug => TraceLevel::Debug,
        program_bindings::helios::system::tracing::Level::Trace => TraceLevel::Trace,
    }
}

fn emit_stage_marker(write_serial: fn(&[u8]), stage: &str) {
    emit_serial_stage_marker(&MarkerSerial(write_serial), stage);
}

fn emit_program_stage_marker(write_serial: fn(&[u8]), stage: &str) {
    if cfg!(debug_assertions) {
        emit_stage_marker(write_serial, stage);
    }
}

struct MarkerSerial(fn(&[u8]));

impl ByteSerial for MarkerSerial {
    fn try_read_byte(&self) -> Option<u8> {
        None
    }

    fn write_bytes(&self, bytes: &[u8]) {
        (self.0)(bytes);
    }
}

#[derive(Debug, Error)]
enum DebuggerError {
    #[error("failed to initialize Wasmtime engine: {0}")]
    CreateEngine(wasmtime::Error),
    #[error("failed to validate embedded debugger artifact: {0}")]
    TrustComponent(ArtifactTrustError),
    #[error("failed to load embedded debugger component: {0}")]
    LoadComponent(wasmtime::Error),
    #[error("failed to instantiate debugger component: {0}")]
    InstantiateComponent(wasmtime::Error),
    #[error("debugger component trapped: {0}")]
    RunComponent(wasmtime::Error),
    #[error("debugger component returned a non-zero result")]
    GuestFailed,
    #[error("the executor has no task capacity left for the debugger component: {0}")]
    TaskCapacity(crate::TaskCapacityError),
}
