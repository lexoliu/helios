//! Wasmtime-based implementation of the component runtime traits.
//!
//! This module is the **only** place in the kernel that should directly
//! depend on `wasmtime::*` types outside of the legacy `component_host`
//! internals. All other kernel code interacts with the runtime through
//! the traits defined in [`crate::component_runtime_backend`].

extern crate alloc;

use alloc::sync::Arc;

use helios_hal::cpu::Cpu;
use wasmtime::component::Component;
use wasmtime::Engine;

use crate::{
    CodegenPlatform, ComponentExecContext, ComponentExecutor, ComponentExitStatus,
    ComponentOutputMode, ComponentRuntimeEngine, ComponentRuntimeFactory, ComponentRuntimeState,
    ComponentWorld, CompiledComponent, ExecOutput, HostFileSystem, InstanceId,
};
use crate::component_host::{
    ComponentBindingSet, HostRuntimeState, StoreData, component_linker, store_with_state,
};

/// Wasmtime-backed compiled component artifact.
pub struct WasmtimeCompiledComponent {
    pub(crate) component: Component,
    pub(crate) engine: Engine,
}

impl CompiledComponent for WasmtimeCompiledComponent {}

/// Wasmtime component runtime engine.
#[derive(Clone)]
pub struct WasmtimeEngine {
    engine: Engine,
}

impl ComponentRuntimeEngine for WasmtimeEngine {
    type Compiled = WasmtimeCompiledComponent;
    type Error = wasmtime::Error;

    fn compile(&self, bytes: &[u8]) -> Result<Self::Compiled, Self::Error> {
        let component = Component::from_binary(&self.engine, bytes)?;
        Ok(WasmtimeCompiledComponent {
            component,
            engine: self.engine.clone(),
        })
    }
}

/// A running Wasmtime component instance.
pub struct WasmtimeExecutor<CpuImpl, HostFs>
where
    CpuImpl: Cpu + CodegenPlatform + Clone,
    HostFs: HostFileSystem,
{
    store: wasmtime::Store<StoreData<CpuImpl, HostFs>>,
    run_func: wasmtime::component::TypedFunc<(), (core::result::Result<(), ()>,)>,
}

impl<CpuImpl, HostFs> ComponentExecutor for WasmtimeExecutor<CpuImpl, HostFs>
where
    CpuImpl: Cpu + CodegenPlatform + Clone,
    HostFs: HostFileSystem,
{
    type Error = wasmtime::Error;

    fn run(
        mut self,
    ) -> impl core::future::Future<Output = Result<ComponentExitStatus, Self::Error>> + Send {
        async move {
            let run = self.run_func;
            let result = self
                .store
                .run_concurrent(async move |accessor| run.call_concurrent(accessor, ()).await)
                .await??;
            match result {
                (Ok(()),) => Ok(ComponentExitStatus::Ok),
                (Err(()),) => Ok(ComponentExitStatus::Failed),
            }
        }
    }

    fn run_cooperative(
        mut self,
        mut tick: impl FnMut() -> usize,
    ) -> Result<ComponentExitStatus, Self::Error> {
        let run = self.run_func;

        let parker = Arc::new(ComponentCallParker::new());
        let waker = core::task::Waker::from(parker.clone());
        let mut cx = core::task::Context::from_waker(&waker);
        let mut call = core::pin::pin!(self
            .store
            .run_concurrent(async move |accessor| { run.call_concurrent(accessor, ()).await }));

        loop {
            parker.clear();
            match call.as_mut().poll(&mut cx) {
                core::task::Poll::Ready(result) => {
                    let result = result??;
                    return match result {
                        (Ok(()),) => Ok(ComponentExitStatus::Ok),
                        (Err(()),) => Ok(ComponentExitStatus::Failed),
                    };
                }
                core::task::Poll::Pending => {
                    if tick() == 0 {
                        parker.wait();
                    }
                }
            }
        }
    }

    fn take_captured_output(&self) -> ExecOutput {
        self.store.data().take_captured_output()
    }

    fn instance_id(&self) -> InstanceId {
        self.store.data().instance().id()
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

impl<P: CodegenPlatform + Clone> WasmtimeComponentRuntime<P> {
    pub fn new(platform: P) -> Self {
        Self { platform }
    }
}

impl<CpuImpl, HostFs, P> ComponentRuntimeFactory<CpuImpl, HostRuntimeState<CpuImpl, HostFs>, HostFs>
    for WasmtimeComponentRuntime<P>
where
    CpuImpl: Cpu + CodegenPlatform + Clone,
    HostFs: HostFileSystem,
    P: CodegenPlatform + Clone,
{
    type Engine = WasmtimeEngine;
    type Executor = WasmtimeExecutor<CpuImpl, HostFs>;
    type CreateEngineError = wasmtime::Error;
    type InstantiateError = wasmtime::Error;

    fn create_engine(&self) -> Result<Self::Engine, Self::CreateEngineError> {
        let engine = crate::build_component_engine_for_platform(&self.platform)?;
        Ok(WasmtimeEngine { engine })
    }

    fn instantiate(
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

        let linker = component_linker(&engine.engine, binding_set, Some(&compiled.component))?;

        let filesystem =
            crate::component_wasi::DebugFileSystem::new(context.host_filesystem_state);
        let debug_port = if context.has_debug_port {
            Some(())
        } else {
            None
        };

        let mut store = store_with_state(
            &engine.engine,
            StoreData::<CpuImpl, HostFs>::new(
                context.cpu,
                context.runtime_state,
                context.instance_registry,
                context.instance,
                debug_port,
                filesystem,
                context.arguments,
                context.environment,
                context.output_mode,
                context.serial_reader,
                context.serial_writer,
            ),
        );

        let instance = crate::block_on(
            linker.instantiate_async(&mut store, &compiled.component),
        )?;

        let run_func = crate::resolve_wasi_cli_run(&compiled.component, &instance, &mut store)?;

        Ok(WasmtimeExecutor { store, run_func })
    }
}

/// Parker for cooperative component execution.
struct ComponentCallParker {
    notified: core::sync::atomic::AtomicBool,
}

impl ComponentCallParker {
    const fn new() -> Self {
        Self {
            notified: core::sync::atomic::AtomicBool::new(false),
        }
    }

    fn clear(&self) {
        self.notified
            .store(false, core::sync::atomic::Ordering::Release);
    }

    fn wait(&self) {
        while !self
            .notified
            .load(core::sync::atomic::Ordering::Acquire)
        {
            core::hint::spin_loop();
        }
    }
}

impl alloc::task::Wake for ComponentCallParker {
    fn wake(self: Arc<Self>) {
        self.notified
            .store(true, core::sync::atomic::Ordering::Release);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.notified
            .store(true, core::sync::atomic::Ordering::Release);
    }
}
