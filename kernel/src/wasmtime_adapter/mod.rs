//! Wasmtime-based implementation of the component runtime traits.
//!
//! This module is the **only** place in the kernel that should directly
//! depend on `wasmtime::*` types. All other kernel code interacts with
//! the runtime through the traits defined in
//! [`crate::component::runtime_backend`].

pub(crate) mod artifact_profile;
pub mod bindings;
pub(crate) mod component_host;
pub mod config;
#[cfg(all(target_os = "none", feature = "wasmtime-bare-metal"))]
pub mod custom_vm;
pub(crate) mod cwasm;
pub mod engine;
pub mod store;
pub mod swap_fault;
#[cfg(all(target_os = "none", feature = "wasmtime-bare-metal"))]
mod sync;
pub mod tls;
pub mod wasi;
pub(crate) mod wasix;

use bytes::Bytes;
use helios_hal::cpu::Cpu;
use wasmtime::component::Component;
use wasmtime::{Engine, Module, Precompiled};

use self::component_host::{
    ComponentBindingSet, HostRuntimeState, StoreData, component_linker, store_with_state,
};
use crate::{
    CompiledComponent, ComponentExecContext, ComponentExecutor, ComponentExitStatus,
    ComponentRunResult, ComponentRuntimeEngine, ComponentRuntimeFactory, ComponentWorld,
    HostFileSystem,
};

const _: fn() -> bool = wasix::manifest_is_mapped;

/// Wasmtime-backed compiled component artifact.
pub struct WasmtimeCompiledComponent {
    pub(crate) component: Component,
}

impl CompiledComponent for WasmtimeCompiledComponent {}

/// Wasmtime-backed compiled Preview1 core-module artifact.
pub(crate) struct WasmtimeCompiledCoreModule {
    pub(crate) cache_key: Bytes,
    pub(crate) module: Module,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WasmtimePrecompiledKind {
    CoreModule,
    Component,
}

impl WasmtimePrecompiledKind {
    pub fn detect(bytes: &[u8]) -> Option<Self> {
        match Engine::detect_precompiled(bytes)? {
            Precompiled::Module => Some(Self::CoreModule),
            Precompiled::Component => Some(Self::Component),
        }
    }
}

/// Wasmtime component runtime engine.
#[derive(Clone)]
pub struct WasmtimeEngine {
    engine: Engine,
}

impl WasmtimeEngine {
    pub(crate) fn increment_epoch(&self) {
        self.engine.increment_epoch();
    }

    pub(crate) fn raw(&self) -> &Engine {
        &self.engine
    }
}

/// Stash of the program-service engine used by the OOM killer to wake
/// CPU-bound victims. `crate::instance::request_kill` calls
/// `bump_user_engine_epoch` so the next `epoch_deadline_async_yield`
/// boundary on any executing wasm fires; that yield forces `call_hook`
/// to observe the kill flag and trap, even when the victim is in pure
/// wasm without WASI calls.
///
/// Wasmtime `Engine` is internally an `Arc`, so storing a clone here
/// is cheap. The static is initialised once, in
/// `install_program_service_inner`.
static OOM_KICK_ENGINE: spin::Once<Engine> = spin::Once::new();

pub(crate) fn register_oom_kick_engine(engine: Engine) {
    OOM_KICK_ENGINE.call_once(|| engine);
}

pub(crate) fn bump_user_engine_epoch() {
    if let Some(engine) = OOM_KICK_ENGINE.get() {
        engine.increment_epoch();
    }
}

impl ComponentRuntimeEngine for WasmtimeEngine {
    type Compiled = WasmtimeCompiledComponent;
    type Error = wasmtime::Error;

    fn compile(&self, bytes: &[u8]) -> Result<Self::Compiled, Self::Error> {
        let component = unsafe { Component::deserialize(&self.engine, bytes)? };
        Ok(WasmtimeCompiledComponent { component })
    }
}

/// A running Wasmtime component instance.
pub struct WasmtimeExecutor<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: HostFileSystem,
{
    store: wasmtime::Store<StoreData<CpuImpl, HostFs>>,
    run_func: wasmtime::component::TypedFunc<(), (core::result::Result<(), ()>,)>,
}

impl<CpuImpl, HostFs> ComponentExecutor for WasmtimeExecutor<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: HostFileSystem,
{
    type Error = wasmtime::Error;

    fn run(
        mut self,
    ) -> impl core::future::Future<Output = Result<ComponentRunResult, Self::Error>> + Send {
        async move {
            let run = self.run_func;
            let raw = self
                .store
                .run_concurrent(async move |accessor| run.call_concurrent(accessor, ()).await)
                .await;
            let (status, exit_code) = interpret_run_result(raw, self.store.data_mut())?;
            let instance_id = self.store.data().instance().id();
            Ok(ComponentRunResult {
                status,
                exit_code,
                instance_id,
            })
        }
    }
}

/// Turn the raw `wasmtime::Result<wasmtime::Result<(Result<(), ()>,)>>` coming
/// out of `run_concurrent` into a clean `(status, exit_code)` pair.
///
/// Runtime traps that correspond to `wasi:cli/exit` requests are treated as
/// a clean exit with the code the guest supplied; other traps still bubble
/// up as `wasmtime::Error`.
fn interpret_run_result<CpuImpl, HostFs>(
    raw: wasmtime::Result<wasmtime::Result<(Result<(), ()>,)>>,
    store_data: &mut StoreData<CpuImpl, HostFs>,
) -> wasmtime::Result<(ComponentExitStatus, u32)>
where
    CpuImpl: Cpu + Clone,
    HostFs: HostFileSystem,
{
    match raw {
        Ok(Ok((Ok(()),))) => Ok((ComponentExitStatus::Ok, 0)),
        Ok(Ok((Err(()),))) => Ok((ComponentExitStatus::Failed, 1)),
        Ok(Err(trap)) | Err(trap) => match store_data.take_requested_exit() {
            Some(code) => {
                let status = if code == 0 {
                    ComponentExitStatus::Ok
                } else {
                    ComponentExitStatus::Failed
                };
                Ok((status, u32::from(code)))
            }
            None => Err(trap),
        },
    }
}

/// Wasmtime component runtime factory.
///
/// This is the concrete [`ComponentRuntimeFactory`] implementation that
/// the kernel wires in when using Wasmtime as the component runtime.
#[derive(Clone)]
pub struct WasmtimeComponentRuntime<P> {
    platform: P,
}

impl<P: Cpu + Clone> WasmtimeComponentRuntime<P> {
    pub fn new(platform: P) -> Self {
        Self { platform }
    }
}

impl<CpuImpl, HostFs, P> ComponentRuntimeFactory<CpuImpl, HostRuntimeState<CpuImpl, HostFs>, HostFs>
    for WasmtimeComponentRuntime<P>
where
    CpuImpl: Cpu + Clone,
    HostFs: HostFileSystem,
    P: Cpu + Clone,
{
    type Engine = WasmtimeEngine;
    type Executor = WasmtimeExecutor<CpuImpl, HostFs>;
    type CreateEngineError = wasmtime::Error;
    type InstantiateError = wasmtime::Error;

    fn create_engine(&self) -> Result<Self::Engine, Self::CreateEngineError> {
        let engine = engine::build_component_engine_for_platform(&self.platform)?;
        Ok(WasmtimeEngine { engine })
    }

    async fn instantiate(
        &self,
        engine: &Self::Engine,
        compiled: &WasmtimeCompiledComponent,
        world: ComponentWorld,
        context: ComponentExecContext<CpuImpl, HostRuntimeState<CpuImpl, HostFs>, HostFs>,
    ) -> Result<Self::Executor, Self::InstantiateError> {
        let binding_set = match world {
            ComponentWorld::System => ComponentBindingSet::System,
            ComponentWorld::Program => ComponentBindingSet::Program,
        };

        let linker = component_linker(&engine.engine, binding_set, &compiled.component)?;

        let filesystem =
            crate::wasmtime_adapter::wasi::DebugFileSystem::new(context.host_filesystem_state);
        let debug_port = if context.has_debug_port {
            Some(())
        } else {
            None
        };

        let mut store = store_with_state(
            &engine.engine,
            StoreData::<CpuImpl, HostFs>::new(
                wasmtime::component::ResourceTable::new(),
                context.cpu,
                context.timer,
                context.spawner,
                context.runtime_state,
                context.instance_registry,
                context.instance,
                debug_port,
                filesystem,
                context.arguments,
                context.environment,
                context.process_authority,
                context.output_mode,
                context.serial_reader,
                context.serial_writer,
            ),
        );

        let instance = linker
            .instantiate_async(&mut store, &compiled.component)
            .await?;

        let run_func = engine::resolve_wasi_cli_run(&compiled.component, &instance, &mut store)?;

        Ok(WasmtimeExecutor { store, run_func })
    }
}
