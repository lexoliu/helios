extern crate alloc;

use alloc::vec::Vec;
use core::future::Future;

use crate::{
    ComponentOutputMode, ComponentRuntimeState, ExecOutput, HostFileSystem, InstanceId,
    InstanceRegistry, RegisteredInstance,
};
use helios_hal::cpu::Cpu;

/// Identifies which host-binding world to install when instantiating a
/// component.
#[derive(Clone, Copy, Debug)]
pub enum ComponentWorld {
    /// The system (debugger) world — includes programs, net, stats, tracing,
    /// instances, serial, sync.
    System,
    /// The launched-program world — includes programs, net, stats, tracing,
    /// serial, sync, but not instances.
    Program,
}

/// Component exit status reported by the runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentExitStatus {
    Ok,
    Failed,
}

/// Kernel-owned execution context passed to the runtime when launching a
/// component.  Contains no runtime-specific types.
pub struct ComponentExecContext<CpuImpl, RuntimeStateImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
    HostFs: HostFileSystem,
{
    pub cpu: CpuImpl,
    pub runtime_state: RuntimeStateImpl,
    pub instance_registry: InstanceRegistry,
    pub instance: RegisteredInstance,
    pub has_debug_port: bool,
    pub host_filesystem_state: RuntimeStateImpl,
    pub arguments: Vec<alloc::string::String>,
    pub environment: Vec<(alloc::string::String, alloc::string::String)>,
    pub output_mode: ComponentOutputMode,
    pub serial_reader: fn(u32) -> Vec<u8>,
    pub serial_writer: fn(&[u8]),
    _host_fs: core::marker::PhantomData<HostFs>,
}

impl<CpuImpl, RuntimeStateImpl, HostFs>
    ComponentExecContext<CpuImpl, RuntimeStateImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
    HostFs: HostFileSystem,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cpu: CpuImpl,
        runtime_state: RuntimeStateImpl,
        instance_registry: InstanceRegistry,
        instance: RegisteredInstance,
        has_debug_port: bool,
        host_filesystem_state: RuntimeStateImpl,
        arguments: Vec<alloc::string::String>,
        environment: Vec<(alloc::string::String, alloc::string::String)>,
        output_mode: ComponentOutputMode,
        serial_reader: fn(u32) -> Vec<u8>,
        serial_writer: fn(&[u8]),
    ) -> Self {
        Self {
            cpu,
            runtime_state,
            instance_registry,
            instance,
            has_debug_port,
            host_filesystem_state,
            arguments,
            environment,
            output_mode,
            serial_reader,
            serial_writer,
            _host_fs: core::marker::PhantomData,
        }
    }
}

/// An opaque compiled component artifact.
pub trait CompiledComponent: Send + Sync + 'static {}

/// A runtime engine that compiles component bytes into reusable artifacts.
pub trait ComponentRuntimeEngine: Clone + Send + Sync + 'static {
    type Compiled: CompiledComponent;
    type Error: core::fmt::Display + Send + 'static;

    fn compile(&self, bytes: &[u8]) -> Result<Self::Compiled, Self::Error>;
}

/// Result of running a component to completion.
pub struct ComponentRunResult {
    pub status: ComponentExitStatus,
    pub instance_id: InstanceId,
    pub output: ExecOutput,
}

/// A running component instance that can be driven to completion.
pub trait ComponentExecutor: Send + 'static {
    type Error: core::fmt::Display + Send + 'static;

    /// Run the component to completion asynchronously.
    fn run(self) -> impl Future<Output = Result<ComponentRunResult, Self::Error>> + Send;

    /// Run the component cooperatively, interleaving with kernel executor
    /// ticks. Returns when the component exits.
    fn run_cooperative(
        self,
        tick: impl FnMut() -> usize,
    ) -> Result<ComponentRunResult, Self::Error>;
}

/// Factory that builds engines and executors from kernel-owned state.
///
/// This is the top-level runtime abstraction: the kernel orchestration layer
/// calls methods on this trait without knowing whether the backing runtime is
/// Wasmtime, Wasmer, or anything else.
///
/// Each associated error type is runtime-specific and avoids heap-allocated
/// strings.
pub trait ComponentRuntimeFactory<CpuImpl, RuntimeStateImpl, HostFs>:
    Clone + Send + Sync + 'static
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
    HostFs: HostFileSystem,
{
    type Engine: ComponentRuntimeEngine;
    type Executor: ComponentExecutor;
    type CreateEngineError: core::fmt::Display + Send + 'static;
    type InstantiateError: core::fmt::Display + Send + 'static;

    /// Create a new runtime engine.
    fn create_engine(&self) -> Result<Self::Engine, Self::CreateEngineError>;

    /// Instantiate a compiled component with the given world and execution
    /// context, producing an executor ready to be driven.
    fn instantiate(
        &self,
        engine: &Self::Engine,
        compiled: &<Self::Engine as ComponentRuntimeEngine>::Compiled,
        world: ComponentWorld,
        context: ComponentExecContext<CpuImpl, RuntimeStateImpl, HostFs>,
    ) -> Result<Self::Executor, Self::InstantiateError>;
}
