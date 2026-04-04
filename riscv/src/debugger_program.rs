extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use helios_hal::cpu::Cpu;
use helios_kernel::{
    EmbeddedDebugger, InstanceExecutionTransition, InstanceRegistry, OwnedRawMutexLease,
    OwnedRawRwLockReadLease, OwnedRawRwLockWriteLease, RawMutex, RawRwLock, RegisteredInstance,
    embedded_debugger, heap_stats,
};
use thiserror::Error;
use wasmtime::component::{
    Accessor, Destination, HasSelf, Linker, Resource, ResourceTable, ResourceType, StreamProducer,
    StreamReader, StreamResult,
};
use wasmtime::{
    CallHook, Config, CustomCodeMemory, Engine, OptLevel, RegallocAlgorithm, ResourceLimiter,
    Store, StoreContextMut,
};
use wasmtime_wasi_io::IoView;
use wasmtime_wasi_io::bytes::Bytes;
use wasmtime_wasi_io::poll::Pollable;
use wasmtime_wasi_io::streams::{OutputStream, StreamError};

use crate::debug_state::{
    RuntimeState, StatsSample, TraceEvent, TraceField, TraceFilter, TraceLevel, TraceValue,
};
use crate::debugger_bindings::bindings;
use crate::{RiscvCpu, try_read_debug_serial_byte, write_debug_serial_bytes};

const WASMTIME_TARGET: &str = "riscv64gc-unknown-none-elf";

struct RiscvCodeMemory;

impl CustomCodeMemory for RiscvCodeMemory {
    fn required_alignment(&self) -> usize {
        1
    }

    fn publish_executable(&self, _ptr: *const u8, _len: usize) -> wasmtime::Result<()> {
        unsafe {
            core::arch::asm!("fence.i", options(nostack, preserves_flags));
        }
        Ok(())
    }

    fn unpublish_executable(&self, _ptr: *const u8, _len: usize) -> wasmtime::Result<()> {
        Ok(())
    }
}

pub struct SbiSerialPort;
pub(crate) struct DebugSerialOutputStream;
pub(crate) struct DeadlinePollable {
    pub(crate) cpu: RiscvCpu,
    pub(crate) deadline_nanos: u64,
}
pub struct SbiRawMutex {
    inner: Arc<RawMutex>,
}

pub struct SbiRawMutexGuard {
    _lease: OwnedRawMutexLease,
}

pub struct SbiRawRwLock {
    inner: Arc<RawRwLock>,
}

pub struct SbiRawRwLockReadGuard {
    _lease: OwnedRawRwLockReadLease,
}

pub struct SbiRawRwLockWriteGuard {
    _lease: OwnedRawRwLockWriteLease,
}

pub fn should_run_on(hart_id: u16, hart_count: usize, bootstrap_hart: u16) -> bool {
    assert!(
        hart_count > 1,
        "embedded debugger requires at least two processors so one can be dedicated to shell I/O"
    );
    hart_id != bootstrap_hart && hart_id == debug_processor(bootstrap_hart, hart_count)
}

pub fn run_forever(cpu: RiscvCpu) -> ! {
    let debugger = embedded_debugger()
        .unwrap_or_else(|| panic!("no embedded debugger program found; set HELIOS_DEBUGGER_WASM"));
    emit_stage_marker("boot");
    tracing::info!("debugger hart: launching embedded debugger component");
    run_debugger(debugger, cpu.clone())
        .unwrap_or_else(|error| panic!("failed to launch embedded debugger component:\n{error:#}"));
    emit_stage_marker("done");
    tracing::info!("debugger hart: embedded debugger component exited cleanly");
    cpu.shutdown()
}

fn debug_processor(bootstrap_hart: u16, hart_count: usize) -> u16 {
    assert!(
        usize::from(bootstrap_hart) < hart_count,
        "bootstrap hart {} is outside detected hart count {}",
        bootstrap_hart,
        hart_count
    );

    if bootstrap_hart == 0 {
        return 1;
    }

    0
}

fn run_debugger(debugger: EmbeddedDebugger, cpu: RiscvCpu) -> Result<(), DebuggerError> {
    emit_stage_marker("engine:new");
    tracing::info!("debugger hart: creating wasmtime engine");
    let engine = build_engine()?;
    emit_stage_marker("engine:ok");
    tracing::info!("debugger hart: compiling embedded debugger component");
    let component =
        wasmtime::component::Component::from_binary(&engine, debugger.component().bytes())
            .map_err(DebuggerError::CompileComponent)?;
    emit_stage_marker("component:ok");

    emit_stage_marker("linker:new");
    tracing::info!("debugger hart: preparing component linker");
    let mut linker = Linker::<StoreData>::new(&engine);
    wasmtime_wasi_io::add_to_linker_async(&mut linker).map_err(DebuggerError::LinkComponent)?;
    crate::debugger_wasi_p2::add_to_linker(&mut linker).map_err(DebuggerError::LinkComponent)?;
    crate::debugger_wasi::add_to_linker(&mut linker).map_err(DebuggerError::LinkComponent)?;
    add_serial_to_linker(&mut linker).map_err(DebuggerError::LinkComponent)?;
    add_sync_to_linker(&mut linker).map_err(DebuggerError::LinkComponent)?;
    add_system_to_linker(&mut linker).map_err(DebuggerError::LinkComponent)?;
    emit_stage_marker("linker:ok");

    emit_stage_marker("store:new");
    let debug_state = cpu.debug_state();
    let instance_registry = cpu.instance_registry();
    let instance =
        instance_registry.register("debugger", debug_state.uptime_nanos(cpu.now().ticks()));
    let mut store = Store::new(
        &engine,
        StoreData {
            table: ResourceTable::new(),
            debug_state,
            instance_registry,
            instance,
            cpu,
            debug_port: Some(()),
            filesystem: crate::debugger_wasi::DebugFileSystem::new(),
        },
    );
    store.limiter(|state| state);
    store.call_hook(|mut caller: StoreContextMut<'_, StoreData>, hook| {
        caller.data_mut().record_call_hook(hook);
        Ok(())
    });
    emit_stage_marker("store:ok");

    emit_stage_marker("pre:begin");
    tracing::info!("debugger hart: instantiating embedded debugger component");
    if let Err(error) = linker.instantiate_pre(&component) {
        emit_error_marker("pre:error", &format!("{error:#}"));
        log_wasmtime_error_chain("debugger hart: instantiate_pre failed", &error);
        return Err(DebuggerError::InstantiateComponent(error));
    }
    emit_stage_marker("pre:ok");
    emit_stage_marker("instantiate:begin");
    let instance = block_on(bindings::Debugger::instantiate_async(
        &mut store, &component, &linker,
    ))
    .map_err(|error| {
        emit_error_marker("instantiate:error", &format!("{error:#}"));
        DebuggerError::InstantiateComponent(error)
    })?;
    emit_stage_marker("instantiate:ok");
    tracing::info!("debugger hart: entering wasi:cli/run");
    emit_stage_marker("run:begin");
    let result =
        block_on(store.run_concurrent(async move |accessor| {
            instance.wasi_cli_run().call_run(accessor).await
        }))
        .map_err(|error| {
            emit_error_marker("run:error", &format!("{error:#}"));
            DebuggerError::RunComponent(error)
        })?;
    emit_stage_marker("run:ok");
    tracing::info!("debugger hart: wasi:cli/run returned");
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(())) => Err(DebuggerError::GuestFailed),
        Err(error) => Err(DebuggerError::RunComponent(error)),
    }
}

fn build_engine() -> Result<Engine, DebuggerError> {
    let mut config = Config::new();
    config
        .target(WASMTIME_TARGET)
        .expect("Helios build target must be accepted by Wasmtime");
    config.cranelift_opt_level(OptLevel::None);
    config.cranelift_regalloc_algorithm(RegallocAlgorithm::SinglePass);
    config.cranelift_debug_verifier(false);
    config.with_custom_code_memory(Some(Arc::new(RiscvCodeMemory)));
    // The RISC-V backend has not wired Wasmtime's custom virtual-memory and
    // native-signal hooks yet, so the engine configuration must match the
    // currently available execution model exactly.
    config.signals_based_traps(false);
    config.memory_guard_size(0);
    config.memory_reservation(0);
    config.memory_reservation_for_growth(1 << 20);
    config.memory_init_cow(false);
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    Engine::new(&config).map_err(DebuggerError::CreateEngine)
}

fn add_system_to_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    add_stats_to_linker(linker)?;
    add_instances_to_linker(linker)?;
    add_tracing_to_linker(linker)?;
    Ok(())
}

fn add_serial_to_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    let mut instance = linker.instance("helios:system/serial@0.1.0")?;
    instance.resource_concurrent(
        "serial-port",
        ResourceType::host::<SbiSerialPort>(),
        |accessor, rep| {
            Box::pin(async move {
                accessor.with(|mut access| {
                    let resource = Resource::<SbiSerialPort>::new_own(rep);
                    let _ = access.get().table.delete(resource)?;
                    Ok::<_, wasmtime::Error>(())
                })
            })
        },
    )?;
    instance.func_wrap(
        "debug-port",
        |mut caller: StoreContextMut<'_, StoreData>, (): ()| {
            let resource = match caller.data().debug_port {
                Some(()) => Some(caller.data_mut().table.push(SbiSerialPort)?),
                None => None,
            };
            Ok((resource,))
        },
    )?;
    instance.func_wrap(
        "[method]serial-port.rights",
        |mut caller: StoreContextMut<'_, StoreData>, (resource,): (Resource<SbiSerialPort>,)| {
            let _ = caller.data_mut().table.get(&resource)?;
            Ok((bindings::helios::system::serial::SerialRights::READ
                | bindings::helios::system::serial::SerialRights::WRITE
                | bindings::helios::system::serial::SerialRights::FLUSH,))
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]serial-port.read",
        |accessor: &Accessor<StoreData>, (resource, max_bytes): (Resource<SbiSerialPort>, u32)| {
            Box::pin(async move {
                accessor.with(|mut access| {
                    let _ = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((read_serial(max_bytes),))
                })
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]serial-port.write",
        |accessor: &Accessor<StoreData>, (resource, bytes): (Resource<SbiSerialPort>, Vec<u8>)| {
            Box::pin(async move {
                accessor.with(|mut access| {
                    let _ = access.get().table.get(&resource)?;
                    write_serial(&bytes);
                    Ok::<_, wasmtime::Error>(())
                })
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]serial-port.flush",
        |accessor: &Accessor<StoreData>, (resource,): (Resource<SbiSerialPort>,)| {
            Box::pin(async move {
                accessor.with(|mut access| {
                    let _ = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>(())
                })
            })
        },
    )?;
    Ok(())
}

fn add_sync_to_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    let mut instance = linker.instance("helios:system/sync@0.1.0")?;
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
        |mut caller: StoreContextMut<'_, StoreData>, (): ()| {
            let resource = caller.data_mut().table.push(SbiRawMutex {
                inner: Arc::new(RawMutex::new()),
            })?;
            Ok((resource,))
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]raw-mutex.lock",
        |accessor: &Accessor<StoreData>, (resource,): (Resource<SbiRawMutex>,)| {
            Box::pin(async move {
                let mutex = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(access.get().table.get(&resource)?.inner.clone())
                })?;
                let lease = mutex.lock_owned().await;
                accessor.with(|mut access| {
                    let guard = access
                        .get()
                        .table
                        .push(SbiRawMutexGuard { _lease: lease })?;
                    Ok::<_, wasmtime::Error>((guard,))
                })
            })
        },
    )?;
    instance.func_wrap(
        "[constructor]raw-rw-lock",
        |mut caller: StoreContextMut<'_, StoreData>, (): ()| {
            let resource = caller.data_mut().table.push(SbiRawRwLock {
                inner: Arc::new(RawRwLock::new()),
            })?;
            Ok((resource,))
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]raw-rw-lock.read",
        |accessor: &Accessor<StoreData>, (resource,): (Resource<SbiRawRwLock>,)| {
            Box::pin(async move {
                let rwlock = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(access.get().table.get(&resource)?.inner.clone())
                })?;
                let lease = rwlock.read_owned().await;
                accessor.with(|mut access| {
                    let guard = access
                        .get()
                        .table
                        .push(SbiRawRwLockReadGuard { _lease: lease })?;
                    Ok::<_, wasmtime::Error>((guard,))
                })
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]raw-rw-lock.write",
        |accessor: &Accessor<StoreData>, (resource,): (Resource<SbiRawRwLock>,)| {
            Box::pin(async move {
                let rwlock = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(access.get().table.get(&resource)?.inner.clone())
                })?;
                let lease = rwlock.write_owned().await;
                accessor.with(|mut access| {
                    let guard = access
                        .get()
                        .table
                        .push(SbiRawRwLockWriteGuard { _lease: lease })?;
                    Ok::<_, wasmtime::Error>((guard,))
                })
            })
        },
    )?;
    Ok(())
}

fn add_stats_to_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    let mut instance = linker.instance("helios:system/stats@0.1.0")?;
    instance.func_wrap("snapshot", |caller, (): ()| {
        Ok((snapshot_sample(caller.data()),))
    })?;
    instance.func_wrap("subscribe", |mut caller, (period,): (u64,)| {
        let reader = StreamReader::new(&mut caller, StatsStreamProducer::new(period))?;
        Ok((reader,))
    })?;
    Ok(())
}

fn add_tracing_to_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    let mut instance = linker.instance("helios:system/tracing@0.1.0")?;
    instance.func_wrap(
        "recent",
        |caller, (filter, limit): (bindings::helios::system::tracing::Filter, u32)| {
            let filter = convert_filter(filter);
            let events = caller
                .data()
                .debug_state
                .recent(&filter, limit)
                .into_iter()
                .map(convert_event)
                .collect::<Vec<_>>();
            Ok((events,))
        },
    )?;
    instance.func_wrap(
        "subscribe",
        |mut caller, (filter,): (bindings::helios::system::tracing::Filter,)| {
            let reader = StreamReader::new(
                &mut caller,
                TracingStreamProducer::new(convert_filter(filter)),
            )?;
            Ok((reader,))
        },
    )?;
    Ok(())
}

fn add_instances_to_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    let mut instance = linker.instance("helios:system/instances@0.1.0")?;
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

pub(crate) struct StoreData {
    pub(crate) table: ResourceTable,
    pub(crate) cpu: RiscvCpu,
    pub(crate) debug_state: RuntimeState,
    pub(crate) instance_registry: InstanceRegistry,
    pub(crate) instance: RegisteredInstance,
    pub(crate) debug_port: Option<()>,
    pub(crate) filesystem: crate::debugger_wasi::DebugFileSystem,
}

impl StoreData {
    pub(crate) fn now_nanos(&self) -> u64 {
        self.debug_state.uptime_nanos(self.cpu.now().ticks())
    }

    pub(crate) fn record_call_hook(&mut self, hook: CallHook) {
        let transition = match hook {
            CallHook::CallingWasm | CallHook::ReturningFromHost => {
                InstanceExecutionTransition::Resume
            }
            CallHook::ReturningFromWasm | CallHook::CallingHost => {
                InstanceExecutionTransition::Pause
            }
        };
        self.instance.transition(transition, self.now_nanos());
    }
}

impl ResourceLimiter for StoreData {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let allow = maximum.is_none_or(|maximum| desired <= maximum);
        if allow {
            self.instance
                .set_memory_bytes(u64::try_from(desired).expect("desired memory exceeds u64"));
        }
        Ok(allow)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(maximum.is_none_or(|maximum| desired <= maximum))
    }
}

impl IoView for StoreData {
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

#[wasmtime_wasi_io::async_trait]
impl Pollable for DebugSerialOutputStream {
    async fn ready(&mut self) {}
}

#[wasmtime_wasi_io::async_trait]
impl OutputStream for DebugSerialOutputStream {
    fn write(&mut self, bytes: Bytes) -> Result<(), StreamError> {
        write_serial(bytes.as_ref());
        Ok(())
    }

    fn flush(&mut self) -> Result<(), StreamError> {
        Ok(())
    }

    fn check_write(&mut self) -> Result<usize, StreamError> {
        Ok(4096)
    }
}

#[wasmtime_wasi_io::async_trait]
impl Pollable for DeadlinePollable {
    async fn ready(&mut self) {
        while self
            .cpu
            .debug_state()
            .ticks_to_nanos(self.cpu.now().ticks())
            < self.deadline_nanos
        {
            core::hint::spin_loop();
        }
    }
}

impl bindings::helios::system::serial::Host for StoreData {}

impl bindings::helios::system::serial::HostWithStore for HasSelf<StoreData> {
    async fn debug_port<T: Send>(
        accessor: &Accessor<T, Self>,
    ) -> wasmtime::Result<Option<Resource<SbiSerialPort>>> {
        accessor.with(|mut access| match access.get().debug_port {
            Some(()) => Ok(Some(access.get().table.push(SbiSerialPort)?)),
            None => Ok(None),
        })
    }
}

impl bindings::helios::system::serial::HostSerialPort for StoreData {}

impl bindings::helios::system::serial::HostSerialPortWithStore for HasSelf<StoreData> {
    async fn rights<T: Send>(
        accessor: &Accessor<T, Self>,
        resource: Resource<SbiSerialPort>,
    ) -> wasmtime::Result<bindings::helios::system::serial::SerialRights> {
        accessor.with(|mut access| {
            let _ = access.get().table.get(&resource)?;
            Ok::<_, wasmtime::Error>(
                bindings::helios::system::serial::SerialRights::READ
                    | bindings::helios::system::serial::SerialRights::WRITE
                    | bindings::helios::system::serial::SerialRights::FLUSH,
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
        Ok(accessor.with(|mut access| {
            let _ = access.get().table.get(&resource)?;
            Ok::<_, wasmtime::Error>(read_serial(max_bytes))
        })?)
    }

    async fn write<T: 'static + Send>(
        accessor: &Accessor<T, Self>,
        resource: Resource<SbiSerialPort>,
        bytes: Vec<u8>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            let _ = access.get().table.get(&resource)?;
            write_serial(&bytes);
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

impl bindings::helios::system::sync::Host for StoreData {}

impl bindings::helios::system::sync::HostRawMutex for StoreData {}

impl bindings::helios::system::sync::HostRawMutexWithStore for HasSelf<StoreData> {
    async fn new<T: Send>(accessor: &Accessor<T, Self>) -> wasmtime::Result<Resource<SbiRawMutex>> {
        accessor.with(|mut access| {
            Ok(access.get().table.push(SbiRawMutex {
                inner: Arc::new(RawMutex::new()),
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
            Ok::<_, wasmtime::Error>(access.get().table.get(&resource)?.inner.clone())
        })?;
        let lease = mutex.lock_owned().await;
        accessor.with(|mut access| {
            Ok(access
                .get()
                .table
                .push(SbiRawMutexGuard { _lease: lease })?)
        })
    }
}

impl bindings::helios::system::sync::HostRawMutexGuard for StoreData {}

impl bindings::helios::system::sync::HostRawMutexGuardWithStore for HasSelf<StoreData> {
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

impl bindings::helios::system::sync::HostRawRwLock for StoreData {}

impl bindings::helios::system::sync::HostRawRwLockWithStore for HasSelf<StoreData> {
    async fn new<T: Send>(
        accessor: &Accessor<T, Self>,
    ) -> wasmtime::Result<Resource<SbiRawRwLock>> {
        accessor.with(|mut access| {
            Ok(access.get().table.push(SbiRawRwLock {
                inner: Arc::new(RawRwLock::new()),
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
            Ok::<_, wasmtime::Error>(access.get().table.get(&resource)?.inner.clone())
        })?;
        let lease = rwlock.read_owned().await;
        accessor.with(|mut access| {
            Ok(access
                .get()
                .table
                .push(SbiRawRwLockReadGuard { _lease: lease })?)
        })
    }

    async fn write<T: 'static>(
        accessor: &Accessor<T, Self>,
        resource: Resource<SbiRawRwLock>,
    ) -> wasmtime::Result<Resource<SbiRawRwLockWriteGuard>> {
        let rwlock = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table.get(&resource)?.inner.clone())
        })?;
        let lease = rwlock.write_owned().await;
        accessor.with(|mut access| {
            Ok(access
                .get()
                .table
                .push(SbiRawRwLockWriteGuard { _lease: lease })?)
        })
    }
}

impl bindings::helios::system::sync::HostRawRwLockReadGuard for StoreData {}

impl bindings::helios::system::sync::HostRawRwLockReadGuardWithStore for HasSelf<StoreData> {
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

impl bindings::helios::system::sync::HostRawRwLockWriteGuard for StoreData {}

impl bindings::helios::system::sync::HostRawRwLockWriteGuardWithStore for HasSelf<StoreData> {
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

impl StreamProducer<StoreData> for StatsStreamProducer {
    type Item = bindings::helios::system::stats::Sample;
    type Buffer = Option<Self::Item>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        store: StoreContextMut<'_, StoreData>,
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

struct TracingStreamProducer {
    filter: TraceFilter,
    cursor: u64,
}

impl TracingStreamProducer {
    fn new(filter: TraceFilter) -> Self {
        Self { filter, cursor: 0 }
    }
}

impl StreamProducer<StoreData> for TracingStreamProducer {
    type Item = bindings::helios::system::tracing::Event;
    type Buffer = Option<Self::Item>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        store: StoreContextMut<'_, StoreData>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }

        match store
            .data()
            .debug_state
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

fn snapshot_sample(store: &StoreData) -> bindings::helios::system::stats::Sample {
    convert_sample(store.debug_state.snapshot(store.cpu.now().ticks()))
}

fn convert_sample(sample: StatsSample) -> bindings::helios::system::stats::Sample {
    let heap = heap_stats();
    let total_bytes =
        u64::try_from(heap.total_bytes).expect("kernel heap total bytes do not fit into u64");
    let available_bytes = u64::try_from(heap.available_bytes())
        .expect("kernel heap available bytes do not fit into u64");
    bindings::helios::system::stats::Sample {
        timestamp: sample.timestamp,
        uptime: sample.uptime,
        processors: bindings::helios::system::stats::Processors {
            configured: sample.configured_processors,
            online: sample.online_processors,
            utilization: (0..sample.configured_processors)
                .map(|id| bindings::helios::system::stats::Processor { id, busy: 0 })
                .collect(),
        },
        memory: bindings::helios::system::stats::Memory {
            total_bytes,
            available_bytes,
            pressure: convert_memory_pressure(total_bytes, available_bytes),
        },
    }
}

fn convert_instance(
    instance: helios_kernel::InstanceSnapshot,
) -> bindings::helios::system::instances::Instance {
    bindings::helios::system::instances::Instance {
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
) -> bindings::helios::system::stats::MemoryPressure {
    if total_bytes == 0 {
        return bindings::helios::system::stats::MemoryPressure::Nominal;
    }

    let used_permille =
        ((total_bytes.saturating_sub(available_bytes.min(total_bytes))) * 1_000) / total_bytes;

    match used_permille {
        0..=699 => bindings::helios::system::stats::MemoryPressure::Nominal,
        700..=849 => bindings::helios::system::stats::MemoryPressure::Elevated,
        850..=949 => bindings::helios::system::stats::MemoryPressure::High,
        _ => bindings::helios::system::stats::MemoryPressure::Critical,
    }
}

fn convert_filter(filter: bindings::helios::system::tracing::Filter) -> TraceFilter {
    TraceFilter {
        min_level: filter.min_level.map(convert_level_to_local),
        target_prefixes: filter.target_prefixes,
    }
}

fn convert_event(event: TraceEvent) -> bindings::helios::system::tracing::Event {
    bindings::helios::system::tracing::Event {
        timestamp: event.timestamp,
        level: convert_level_from_local(event.level),
        target: event.target,
        fields: event.fields.into_iter().map(convert_field).collect(),
    }
}

fn convert_field(field: TraceField) -> bindings::helios::system::tracing::Field {
    bindings::helios::system::tracing::Field {
        key: field.key,
        value: convert_value(field.value),
    }
}

fn convert_value(value: TraceValue) -> bindings::helios::system::tracing::Value {
    match value {
        TraceValue::Boolean(value) => bindings::helios::system::tracing::Value::Boolean(value),
        TraceValue::Signed64(value) => bindings::helios::system::tracing::Value::Signed64(value),
        TraceValue::Unsigned64(value) => {
            bindings::helios::system::tracing::Value::Unsigned64(value)
        }
        TraceValue::Float64(value) => bindings::helios::system::tracing::Value::Float64(value),
        TraceValue::Text(value) => bindings::helios::system::tracing::Value::Text(value),
        TraceValue::Blob(value) => bindings::helios::system::tracing::Value::Blob(value),
    }
}

fn convert_level_from_local(level: TraceLevel) -> bindings::helios::system::tracing::Level {
    match level {
        TraceLevel::Error => bindings::helios::system::tracing::Level::Error,
        TraceLevel::Warn => bindings::helios::system::tracing::Level::Warn,
        TraceLevel::Info => bindings::helios::system::tracing::Level::Info,
        TraceLevel::Debug => bindings::helios::system::tracing::Level::Debug,
        TraceLevel::Trace => bindings::helios::system::tracing::Level::Trace,
    }
}

fn convert_level_to_local(level: bindings::helios::system::tracing::Level) -> TraceLevel {
    match level {
        bindings::helios::system::tracing::Level::Error => TraceLevel::Error,
        bindings::helios::system::tracing::Level::Warn => TraceLevel::Warn,
        bindings::helios::system::tracing::Level::Info => TraceLevel::Info,
        bindings::helios::system::tracing::Level::Debug => TraceLevel::Debug,
        bindings::helios::system::tracing::Level::Trace => TraceLevel::Trace,
    }
}

fn read_serial(max_bytes: u32) -> Vec<u8> {
    let max_bytes = max_bytes as usize;
    let mut bytes = Vec::with_capacity(max_bytes);

    loop {
        if let Some(byte) = try_read_debug_serial_byte() {
            bytes.push(byte);
            break;
        }
        core::hint::spin_loop();
    }

    while bytes.len() < max_bytes {
        let Some(byte) = try_read_debug_serial_byte() else {
            break;
        };
        bytes.push(byte);
    }

    bytes
}

fn write_serial(bytes: &[u8]) {
    write_debug_serial_bytes(bytes);
}

fn emit_stage_marker(stage: &str) {
    write_debug_serial_bytes(b"\n[KDBG ");
    write_debug_serial_bytes(stage.as_bytes());
    write_debug_serial_bytes(b"]\n");
}

fn emit_error_marker(label: &str, message: &str) {
    write_debug_serial_bytes(b"\n[KDBG ");
    write_debug_serial_bytes(label.as_bytes());
    write_debug_serial_bytes(b": ");
    for byte in message.bytes() {
        match byte {
            b'\n' | b'\r' => write_debug_serial_bytes(b" "),
            b']' => write_debug_serial_bytes(b")"),
            other => write_debug_serial_bytes(&[other]),
        }
    }
    write_debug_serial_bytes(b"]\n");
}

fn log_wasmtime_error_chain(prefix: &str, error: &wasmtime::Error) {
    tracing::error!("{prefix}: {error:?}");
}

fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = Pin::from(Box::new(future));

    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => core::hint::spin_loop(),
        }
    }
}

#[derive(Debug, Error)]
enum DebuggerError {
    #[error("failed to initialize Wasmtime engine: {0}")]
    CreateEngine(wasmtime::Error),
    #[error("failed to JIT-compile embedded debugger component: {0}")]
    CompileComponent(wasmtime::Error),
    #[error("failed to add debugger host bindings: {0}")]
    LinkComponent(wasmtime::Error),
    #[error("failed to instantiate debugger component: {0}")]
    InstantiateComponent(wasmtime::Error),
    #[error("debugger component trapped: {0}")]
    RunComponent(wasmtime::Error),
    #[error("debugger component returned a non-zero result")]
    GuestFailed,
}
