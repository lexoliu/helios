extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::pin::Pin;
use core::task::{Context, Poll};

use crate::WasiRights;
use crate::{
    ComponentCache, ComponentNetworkService, ComponentOutputMode, ComponentOutputStreamKind,
    ComponentStoreData, DeadlinePollable, EmbeddedComponent, ExecResult, ProgramExecError,
    ProgramExecErrorKind, RawMutex, RawMutexGuardResource, RawMutexResource, RawRwLock,
    RawRwLockReadGuardResource, RawRwLockResource, RawRwLockWriteGuardResource, SerialPortResource,
    elapsed_millis, emit_serial_stage_marker, heap_stats, monotonic_nanos,
};
use helios_hal::cpu::Cpu;
use helios_hal::serial::ByteSerial;
use spin::Mutex;
use thiserror::Error;
use wasmtime::component::{
    Access, Accessor, Component, Destination, FutureReader, HasSelf, Linker, Resource,
    ResourceType, StreamProducer, StreamReader, StreamResult,
};
use wasmtime::{self, Engine, Store, StoreContextMut};
use wasmtime_wasi_io;

use crate::wasmtime_adapter::bindings::debugger::bindings as debugger_bindings;
use crate::wasmtime_adapter::bindings::program::bindings as program_bindings;
use crate::wasmtime_adapter::config::AotCompileHint;
use crate::wasmtime_adapter::wasi::bindings::filesystem::types::ErrorCode as FsErrorCode;
use crate::{StatsSample, TraceEvent, TraceField, TraceFilter, TraceLevel, TraceValue};

const SYNC_INSTANCE: &str = "helios:system/sync@0.1.0";
const STATS_INSTANCE: &str = "helios:system/stats@0.1.0";
const NET_INSTANCE: &str = "helios:system/net@0.1.0";
const TRACING_INSTANCE: &str = "helios:system/tracing@0.1.0";
const INSTANCES_INSTANCE: &str = "helios:system/instances@0.1.0";
const WORKER_STACK_SIZE: usize = 256 * 1024;
const COMPONENT_CACHE_FRACTION: usize = 8;

pub mod service;
mod topology;

pub use service::{
    ChildExit, ChildHandle, ProgramServiceConfig, UserProgramService,
    install_component_host_program_service, install_program_service,
    install_program_service_with_config, run_component_host_processor_forever,
    run_embedded_component_forever, run_program_workers_forever,
};
pub(crate) use service::{ProgramExecContext, ProgramSource};
pub use topology::{
    ComponentHostProcessorRole, component_host_processor_role, component_host_processors_to_start,
    component_host_system_processor, component_host_worker_count, system_component_should_run_on,
};

pub type SbiSerialPort = crate::ComponentSerialPort;

pub type NetworkTcpBackend = crate::ComponentTcpBackend<crate::DynamicNetworkService>;
pub type NetworkUdpBackend = crate::ComponentUdpBackend<crate::DynamicNetworkService>;
pub type SbiTcpStream = crate::ComponentTcpStream<NetworkTcpBackend>;
pub type SbiUdpSocket = crate::ComponentUdpSocket<NetworkUdpBackend>;
pub type HostRuntimeState<CpuImpl, HostFs> =
    crate::RuntimeState<UserProgramService<CpuImpl, HostFs>, crate::DynamicNetworkService, HostFs>;
pub type StoreData<CpuImpl, HostFs> = ComponentStoreData<
    CpuImpl,
    HostRuntimeState<CpuImpl, HostFs>,
    crate::wasmtime_adapter::wasi::DebugFileSystem<HostRuntimeState<CpuImpl, HostFs>, HostFs>,
>;
pub type OutputMode = ComponentOutputMode;
pub type OutputStreamKind = ComponentOutputStreamKind;
pub type RuntimeDeadlinePollable<CpuImpl, HostFs> =
    DeadlinePollable<CpuImpl, HostRuntimeState<CpuImpl, HostFs>>;

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
    ($bindings:ident, $convert_result:ident, $convert_error:ident) => {
        impl<CpuImpl, HostFs> $bindings::helios::system::programs::Host
            for StoreData<CpuImpl, HostFs>
        where
            CpuImpl: Cpu + crate::CodegenPlatform + Clone,
            HostFs: crate::HostFileSystem,
        {
        }

        impl<CpuImpl, HostFs> $bindings::helios::system::programs::HostChild
            for StoreData<CpuImpl, HostFs>
        where
            CpuImpl: Cpu + crate::CodegenPlatform + Clone,
            HostFs: crate::HostFileSystem,
        {
        }

        impl<CpuImpl, HostFs> $bindings::helios::system::programs::HostChildWithStore
            for HasSelf<StoreData<CpuImpl, HostFs>>
        where
            CpuImpl: Cpu + crate::CodegenPlatform + Clone,
            HostFs: crate::HostFileSystem,
        {
            async fn drop<T>(
                accessor: &Accessor<T, Self>,
                child: wasmtime::component::Resource<ChildHandle>,
            ) -> wasmtime::Result<()> {
                accessor.with(|mut access| {
                    let _ = access.get().table.delete(child)?;
                    Ok::<_, wasmtime::Error>(())
                })?;
                Ok(())
            }

            async fn wait<T: Send>(
                accessor: &Accessor<T, Self>,
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
            fn stdin<T>(
                mut access: Access<'_, T, Self>,
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

            fn stdout<T>(
                mut access: Access<'_, T, Self>,
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
                            crate::wasmtime_adapter::wasi::ChannelStreamProducer::new_with_completion(reader, tx),
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

            fn stderr<T>(
                mut access: Access<'_, T, Self>,
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
                            crate::wasmtime_adapter::wasi::ChannelStreamProducer::new_with_completion(reader, tx),
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

        impl<CpuImpl, HostFs> $bindings::helios::system::programs::HostWithStore
            for HasSelf<StoreData<CpuImpl, HostFs>>
        where
            CpuImpl: Cpu + crate::CodegenPlatform + Clone,
            HostFs: crate::HostFileSystem,
        {
            fn spawn<T: Send>(
                accessor: &Accessor<T, Self>,
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
                    ))
                });
                async move {
                    let (service, context) = snapshot?;
                    let Some(service) = service else {
                        return Ok(Err($bindings::helios::system::programs::SpawnError {
                            kind: $bindings::helios::system::programs::SpawnErrorKind::Unavailable,
                            detail: "program spawn is unavailable on this machine".to_owned(),
                        }));
                    };
                    let source = match read_program_source(accessor, &request.path).await? {
                        Ok(source) => source,
                        Err(error) => return Ok(Err($convert_error(error))),
                    };
                    match service
                        .spawn(
                            context,
                            request.name,
                            request.args,
                            request.env,
                            source,
                            None,
                            WasiRights::empty(),
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

            fn exec<T: Send>(
                accessor: &Accessor<T, Self>,
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
                    ))
                });
                async move {
                    let (service, context) = snapshot?;
                    let Some(service) = service else {
                        return Ok(Err($bindings::helios::system::programs::ExecError {
                            kind: $bindings::helios::system::programs::ExecErrorKind::Unavailable,
                            detail: "program exec is unavailable on this machine".to_owned(),
                        }));
                    };
                    let source = match read_program_source(accessor, &request.path).await? {
                        Ok(source) => source,
                        Err(error) => return Ok(Err($convert_error(error))),
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
                            request.name,
                            request.args,
                            request.env,
                            source,
                            hint,
                            request.stdin,
                            WasiRights::empty(),
                        )
                        .await
                        .map($convert_result)
                        .map_err($convert_error))
                }
            }

            fn aot<T: Send>(
                accessor: &Accessor<T, Self>,
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
                    ))
                });
                async move {
                    let (service, _context) = snapshot?;
                    let Some(service) = service else {
                        return Ok(Err($bindings::helios::system::programs::ExecError {
                            kind: $bindings::helios::system::programs::ExecErrorKind::Unavailable,
                            detail: "program AOT is unavailable on this machine".to_owned(),
                        }));
                    };
                    let source = match read_program_source(accessor, &request.source_path).await? {
                        Ok(source) => source,
                        Err(error) => return Ok(Err($convert_error(error))),
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
                    let artifact = match service.aot(&wasm, hint) {
                        Ok(artifact) => artifact,
                        Err(error) => return Ok(Err($convert_error(error))),
                    };
                    if let Err(error) =
                        write_program_artifact(accessor, &request.destination_path, &artifact).await?
                    {
                        return Ok(Err($convert_error(error)));
                    }
                    Ok(Ok($bindings::helios::system::programs::AotResult {
                        destination_path: request.destination_path,
                    }))
                }
            }
        }
    };
}

impl_program_bindings!(
    debugger_bindings,
    convert_launch_result,
    convert_launch_error
);
impl_program_bindings!(
    program_bindings,
    convert_program_launch_result,
    convert_program_launch_error
);

async fn read_program_source<T, CpuImpl, HostFs>(
    accessor: &Accessor<T, HasSelf<StoreData<CpuImpl, HostFs>>>,
    path: &str,
) -> wasmtime::Result<Result<service::ProgramSource, crate::ProgramExecError>>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    let absolute =
        crate::resolve_child_path("/", path).map_err(map_component_fs_path_error_to_wasmtime)?;
    let host_path = crate::guest_host_share_path(&absolute).map(str::to_owned);
    if let Some(host_path) = host_path {
        let host_service = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().runtime_state.host_filesystem_service())
        })?;
        let Some(host_service) = host_service else {
            return Ok(Err(crate::ProgramExecError {
                kind: crate::ProgramExecErrorKind::Unavailable,
                detail: "host filesystem service is unavailable".into(),
            }));
        };
        return Ok(host_service
            .read_file(&host_path)
            .await
            .map(|bytes| classify_program_source(bytes, false))
            .map_err(|error| crate::ProgramExecError {
                kind: crate::ProgramExecErrorKind::InvalidPath,
                detail: format!("failed to read {}: {error}", absolute),
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
            .read_program_file(&absolute)
            .map_err(map_fs_error_to_program_exec)?;
        Ok::<_, wasmtime::Error>(Ok(classify_program_source(bytes, readonly)))
    })
}

async fn write_program_artifact<T, CpuImpl, HostFs>(
    accessor: &Accessor<T, HasSelf<StoreData<CpuImpl, HostFs>>>,
    path: &str,
    bytes: &[u8],
) -> wasmtime::Result<Result<(), crate::ProgramExecError>>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    let absolute =
        crate::resolve_child_path("/", path).map_err(map_component_fs_path_error_to_wasmtime)?;
    let host_path = crate::guest_host_share_path(&absolute).map(str::to_owned);
    if let Some(host_path) = host_path {
        let host_service = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().runtime_state.host_filesystem_service())
        })?;
        let Some(host_service) = host_service else {
            return Ok(Err(crate::ProgramExecError {
                kind: crate::ProgramExecErrorKind::Unavailable,
                detail: "host filesystem service is unavailable".into(),
            }));
        };
        if let Err(truncate_error) = host_service.truncate_file(&host_path).await {
            host_service
                .create_file(&host_path)
                .await
                .map_err(|create_error| crate::ProgramExecError {
                    kind: crate::ProgramExecErrorKind::InvalidPath,
                    detail: format!(
                        "failed to prepare {}: truncate={truncate_error}, create={create_error}",
                        absolute
                    ),
                })?;
        }
        host_service
            .write_file(&host_path, 0, bytes)
            .await
            .map_err(|error| crate::ProgramExecError {
                kind: crate::ProgramExecErrorKind::InvalidPath,
                detail: format!("failed to write {}: {error}", absolute),
            })?;
        return Ok(Ok(()));
    }

    accessor.with(|mut access| {
        let now_nanos = access.get().now_nanos();
        access
            .get_mut()
            .filesystem_mut()
            .write_program_file(&absolute, bytes, now_nanos)
            .map_err(map_fs_error_to_program_exec)?;
        Ok::<_, wasmtime::Error>(Ok(()))
    })
}

fn classify_program_source(bytes: Vec<u8>, readonly: bool) -> ProgramSource {
    if crate::is_wasmc(&bytes) {
        if readonly {
            return ProgramSource::BootfsArtifact(bytes);
        }
        return ProgramSource::SignedArtifact(bytes);
    }
    ProgramSource::RawWasm(bytes)
}

fn map_fs_error_to_program_exec(error: FsErrorCode) -> crate::ProgramExecError {
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
        detail: error.to_string(),
    }
}

fn map_component_fs_path_error_to_wasmtime(error: crate::ComponentFsPathError) -> wasmtime::Error {
    wasmtime::Error::msg(error.to_string())
}

async fn run_system_component<CpuImpl, HostFs>(
    component: EmbeddedComponent,
    world: ComponentBindingSet,
    cpu: CpuImpl,
    spawner: crate::Spawner<CpuImpl>,
    debug_state: HostRuntimeState<CpuImpl, HostFs>,
    read_serial: fn(u32) -> Vec<u8>,
    write_serial: fn(&[u8]),
) -> Result<(), DebuggerError>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    use crate::{
        ComponentExecContext, ComponentExecutor, ComponentExitStatus, ComponentRuntimeFactory,
        ComponentWorld,
    };

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
    let trusted = crate::trust_bootfs_artifact(crate::UntrustedWasmc::new(component.bytes()))
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
    let instance =
        instance_registry.register(component_name, debug_state.uptime_nanos(cpu.now().ticks()));

    let component_world = match world {
        ComponentBindingSet::System => ComponentWorld::System,
        ComponentBindingSet::Program => ComponentWorld::Program,
    };

    let context = ComponentExecContext::new(
        cpu,
        spawner,
        debug_state.clone(),
        instance_registry,
        instance,
        true,
        debug_state,
        Vec::new(),
        Vec::new(),
        OutputMode::Serial,
        read_serial,
        write_serial,
    );

    let executor = runtime
        .instantiate(&engine, &compiled, component_world, context)
        .await
        .map_err(DebuggerError::InstantiateComponent)?;
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
    component: Option<&Component>,
) -> wasmtime::Result<Linker<StoreData<CpuImpl, HostFs>>>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    let mut linker = Linker::<StoreData<CpuImpl, HostFs>>::new(engine);
    if let Some(component) = component {
        linker.allow_shadowing(true);
        linker.define_unknown_imports_as_traps(component)?;
    }
    wasmtime_wasi_io::add_to_linker_async(&mut linker)?;
    crate::wasmtime_adapter::wasi::p2::add_to_linker(&mut linker)?;
    crate::wasmtime_adapter::wasi::add_to_linker(&mut linker)?;
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    let mut store = Store::new(engine, state);
    store.limiter(|state| state);
    store.call_hook(
        |mut caller: StoreContextMut<'_, StoreData<CpuImpl, HostFs>>, hook| {
            let transition = crate::wasmtime_adapter::store::translate_call_hook(hook);
            caller.data_mut().record_transition(transition);
            Ok(())
        },
    );
    store.set_epoch_deadline(1);
    store.epoch_deadline_async_yield_and_update(1);
    store
}

fn add_system_to_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    add_programs_to_linker(linker)?;
    add_net_to_linker(linker)?;
    add_stats_to_linker(linker)?;
    add_instances_to_linker(linker)?;
    add_tracing_to_linker(linker)?;
    Ok(())
}

pub(crate) fn add_program_world_to_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    add_programs_to_program_linker(linker)?;
    add_net_to_program_linker(linker)?;
    add_stats_to_program_linker(linker)?;
    add_tracing_to_program_linker(linker)?;
    Ok(())
}

pub(crate) fn add_serial_to_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    program_bindings::helios::system::programs::add_to_linker::<
        _,
        HasSelf<StoreData<CpuImpl, HostFs>>,
    >(linker, |state| state)?;
    Ok(())
}

fn add_net_to_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
                let service = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(access.get().runtime_state.network_service())
                })?;
                let Some(service) = service else {
                    return Ok::<_, wasmtime::Error>((Err(unavailable_tcp_error()),));
                };
                let connected = service.tcp_connect(&host, port, timeout).await;
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
                let socket = accessor.with(|mut access| {
                    let socket = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((
                        socket.resource.backend.service.clone(),
                        socket.resource.backend.stream,
                    ))
                })?;
                let response = socket
                    .0
                    .tcp_read(socket.1, max_bytes, timeout)
                    .await
                    .map_err(convert_tcp_error);
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]tcp-stream.write",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource, bytes, timeout): (Resource<SbiTcpStream>, Vec<u8>, u64)| {
            Box::pin(async move {
                let socket = accessor.with(|mut access| {
                    let socket = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((
                        socket.resource.backend.service.clone(),
                        socket.resource.backend.stream,
                    ))
                })?;
                let response = socket
                    .0
                    .tcp_write_all(socket.1, &bytes, timeout)
                    .await
                    .map(|()| bytes.len() as u64)
                    .map_err(convert_tcp_error);
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
                let service = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(access.get().runtime_state.network_service())
                })?;
                let Some(service) = service else {
                    return Ok::<_, wasmtime::Error>((Err(unavailable_program_tcp_error()),));
                };
                let connected = service.tcp_connect(&host, port, timeout).await;
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
                let socket = accessor.with(|mut access| {
                    let socket = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((
                        socket.resource.backend.service.clone(),
                        socket.resource.backend.stream,
                    ))
                })?;
                let response = socket
                    .0
                    .tcp_read(socket.1, max_bytes, timeout)
                    .await
                    .map_err(convert_program_tcp_error);
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]tcp-stream.write",
        |accessor: &Accessor<StoreData<CpuImpl, HostFs>>,
         (resource, bytes, timeout): (Resource<SbiTcpStream>, Vec<u8>, u64)| {
            Box::pin(async move {
                let socket = accessor.with(|mut access| {
                    let socket = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((
                        socket.resource.backend.service.clone(),
                        socket.resource.backend.stream,
                    ))
                })?;
                let response = socket
                    .0
                    .tcp_write_all(socket.1, &bytes, timeout)
                    .await
                    .map(|()| bytes.len() as u64)
                    .map_err(convert_program_tcp_error);
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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

fn add_stats_to_program_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
            ProgramExecErrorKind::InvalidHint => {
                debugger_bindings::helios::system::programs::ExecErrorKind::InvalidHint
            }
            ProgramExecErrorKind::Unavailable => {
                debugger_bindings::helios::system::programs::ExecErrorKind::Unavailable
            }
            ProgramExecErrorKind::Internal => {
                debugger_bindings::helios::system::programs::ExecErrorKind::Internal
            }
        },
        detail: error.detail,
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
            ProgramExecErrorKind::InvalidHint => {
                program_bindings::helios::system::programs::ExecErrorKind::InvalidHint
            }
            ProgramExecErrorKind::Unavailable => {
                program_bindings::helios::system::programs::ExecErrorKind::Unavailable
            }
            ProgramExecErrorKind::Internal => {
                program_bindings::helios::system::programs::ExecErrorKind::Internal
            }
        },
        detail: error.detail,
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

fn convert_ping_reply(
    reply: crate::PingReply,
) -> debugger_bindings::helios::system::net::PingReply {
    let octets = reply.address.octets();
    debugger_bindings::helios::system::net::PingReply {
        address: debugger_bindings::helios::system::net::IpAddress::Ipv4((
            octets[0], octets[1], octets[2], octets[3],
        )),
        round_trip: reply.round_trip_nanos,
        payload_bytes: reply.payload_bytes,
    }
}

fn convert_program_ping_reply(
    reply: crate::PingReply,
) -> program_bindings::helios::system::net::PingReply {
    let octets = reply.address.octets();
    program_bindings::helios::system::net::PingReply {
        address: program_bindings::helios::system::net::IpAddress::Ipv4((
            octets[0], octets[1], octets[2], octets[3],
        )),
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
        detail: error.detail,
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
        detail: error.detail,
    }
}

fn unavailable_tcp_error() -> debugger_bindings::helios::system::net::TcpError {
    debugger_bindings::helios::system::net::TcpError {
        kind: debugger_bindings::helios::system::net::TcpErrorKind::Unavailable,
        detail: "network service is unavailable on this machine".to_owned(),
    }
}

fn unavailable_program_tcp_error() -> program_bindings::helios::system::net::TcpError {
    program_bindings::helios::system::net::TcpError {
        kind: program_bindings::helios::system::net::TcpErrorKind::Unavailable,
        detail: "network service is unavailable on this machine".to_owned(),
    }
}

fn unavailable_udp_error() -> debugger_bindings::helios::system::net::UdpError {
    debugger_bindings::helios::system::net::UdpError {
        kind: debugger_bindings::helios::system::net::UdpErrorKind::Unavailable,
        detail: "network service is unavailable on this machine".to_owned(),
    }
}

fn unavailable_program_udp_error() -> program_bindings::helios::system::net::UdpError {
    program_bindings::helios::system::net::UdpError {
        kind: program_bindings::helios::system::net::UdpErrorKind::Unavailable,
        detail: "network service is unavailable on this machine".to_owned(),
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
            crate::TcpErrorKind::Unavailable => {
                debugger_bindings::helios::system::net::TcpErrorKind::Unavailable
            }
            crate::TcpErrorKind::Internal => {
                debugger_bindings::helios::system::net::TcpErrorKind::Internal
            }
        },
        detail: error.detail,
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
            crate::TcpErrorKind::Unavailable => {
                program_bindings::helios::system::net::TcpErrorKind::Unavailable
            }
            crate::TcpErrorKind::Internal => {
                program_bindings::helios::system::net::TcpErrorKind::Internal
            }
        },
        detail: error.detail,
    }
}

fn convert_udp_datagram(
    datagram: crate::UdpDatagram,
) -> debugger_bindings::helios::system::net::UdpDatagram {
    let octets = datagram.address.octets();
    debugger_bindings::helios::system::net::UdpDatagram {
        address: debugger_bindings::helios::system::net::IpAddress::Ipv4((
            octets[0], octets[1], octets[2], octets[3],
        )),
        port: datagram.port,
        bytes: datagram.bytes,
    }
}

fn convert_program_udp_datagram(
    datagram: crate::UdpDatagram,
) -> program_bindings::helios::system::net::UdpDatagram {
    let octets = datagram.address.octets();
    program_bindings::helios::system::net::UdpDatagram {
        address: program_bindings::helios::system::net::IpAddress::Ipv4((
            octets[0], octets[1], octets[2], octets[3],
        )),
        port: datagram.port,
        bytes: datagram.bytes,
    }
}

fn convert_udp_error(error: crate::UdpError) -> debugger_bindings::helios::system::net::UdpError {
    debugger_bindings::helios::system::net::UdpError {
        kind: match error.kind {
            crate::UdpErrorKind::UnresolvedHost => {
                debugger_bindings::helios::system::net::UdpErrorKind::UnresolvedHost
            }
            crate::UdpErrorKind::Timeout => {
                debugger_bindings::helios::system::net::UdpErrorKind::Timeout
            }
            crate::UdpErrorKind::Unavailable => {
                debugger_bindings::helios::system::net::UdpErrorKind::Unavailable
            }
            crate::UdpErrorKind::Internal => {
                debugger_bindings::helios::system::net::UdpErrorKind::Internal
            }
        },
        detail: error.detail,
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
            crate::UdpErrorKind::Timeout => {
                program_bindings::helios::system::net::UdpErrorKind::Timeout
            }
            crate::UdpErrorKind::Unavailable => {
                program_bindings::helios::system::net::UdpErrorKind::Unavailable
            }
            crate::UdpErrorKind::Internal => {
                program_bindings::helios::system::net::UdpErrorKind::Internal
            }
        },
        detail: error.detail,
    }
}

fn add_instances_to_linker<CpuImpl, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs> debugger_bindings::helios::system::serial::HostWithStore
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn debug_port<T: Send>(
        accessor: &Accessor<T, Self>,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs> debugger_bindings::helios::system::serial::HostSerialPortWithStore
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn rights<T: Send>(
        accessor: &Accessor<T, Self>,
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

    async fn drop<T>(
        accessor: &Accessor<T, Self>,
        resource: Resource<SbiSerialPort>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            let _ = access.get().table.delete(resource)?;
            Ok::<_, wasmtime::Error>(())
        })
    }

    async fn read<T: 'static + Send>(
        accessor: &Accessor<T, Self>,
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

    async fn write<T: 'static + Send>(
        accessor: &Accessor<T, Self>,
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

    async fn flush<T: 'static + Send>(
        accessor: &Accessor<T, Self>,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs> debugger_bindings::helios::system::sync::HostRawMutex
    for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs> debugger_bindings::helios::system::sync::HostRawMutexWithStore
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn new<T: Send>(accessor: &Accessor<T, Self>) -> wasmtime::Result<Resource<SbiRawMutex>> {
        accessor.with(|mut access| {
            Ok(access.get().table.push(SbiRawMutex {
                resource: RawMutexResource {
                    inner: Arc::new(RawMutex::new()),
                },
            })?)
        })
    }

    async fn drop<T>(
        accessor: &Accessor<T, Self>,
        resource: Resource<SbiRawMutex>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            let _ = access.get().table.delete(resource)?;
            Ok::<_, wasmtime::Error>(())
        })
    }

    async fn lock<T: 'static>(
        accessor: &Accessor<T, Self>,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs> debugger_bindings::helios::system::sync::HostRawMutexGuardWithStore
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn drop<T>(
        accessor: &Accessor<T, Self>,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs> debugger_bindings::helios::system::sync::HostRawRwLockWithStore
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn new<T: Send>(
        accessor: &Accessor<T, Self>,
    ) -> wasmtime::Result<Resource<SbiRawRwLock>> {
        accessor.with(|mut access| {
            Ok(access.get().table.push(SbiRawRwLock {
                resource: RawRwLockResource {
                    inner: Arc::new(RawRwLock::new()),
                },
            })?)
        })
    }

    async fn drop<T>(
        accessor: &Accessor<T, Self>,
        resource: Resource<SbiRawRwLock>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            let _ = access.get().table.delete(resource)?;
            Ok::<_, wasmtime::Error>(())
        })
    }

    async fn read<T: 'static>(
        accessor: &Accessor<T, Self>,
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

    async fn write<T: 'static>(
        accessor: &Accessor<T, Self>,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs> debugger_bindings::helios::system::sync::HostRawRwLockReadGuardWithStore
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn drop<T>(
        accessor: &Accessor<T, Self>,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs> debugger_bindings::helios::system::sync::HostRawRwLockWriteGuardWithStore
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn drop<T>(
        accessor: &Accessor<T, Self>,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    convert_sample(store.runtime_state.snapshot(store.cpu.now().ticks()))
}

fn snapshot_program_sample<CpuImpl, HostFs>(
    store: &StoreData<CpuImpl, HostFs>,
) -> program_bindings::helios::system::stats::Sample
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    convert_program_sample(store.runtime_state.snapshot(store.cpu.now().ticks()))
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
    TrustComponent(crate::ArtifactTrustError),
    #[error("failed to load embedded debugger component: {0}")]
    LoadComponent(wasmtime::Error),
    #[error("failed to instantiate debugger component: {0}")]
    InstantiateComponent(wasmtime::Error),
    #[error("debugger component trapped: {0}")]
    RunComponent(wasmtime::Error),
    #[error("debugger component returned a non-zero result")]
    GuestFailed,
}
