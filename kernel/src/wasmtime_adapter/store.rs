//! Wasmtime trait implementations for `ComponentStoreData`.
//!
//! These trait impls bridge kernel-owned store state to Wasmtime's runtime
//! requirements. They live in the adapter module so that `component_runtime.rs`
//! stays free of Wasmtime imports.

extern crate alloc;
use alloc::boxed::Box;

use helios_hal::cpu::Cpu;
use wasmtime::component::ResourceTable;
use wasmtime::{CallHook, ResourceLimiter};
use wasmtime_wasi_io::IoView;
use wasmtime_wasi_io::bytes::Bytes;
use wasmtime_wasi_io::poll::Pollable;
use wasmtime_wasi_io::streams::{OutputStream, StreamError};

use crate::{
    ComponentOutputStream, ComponentOutputStreamKind, ComponentRuntimeState, ComponentStoreData,
    allow_instance_resource_growth,
};

impl<CpuImpl, RuntimeStateImpl, FileSystem> ResourceLimiter
    for ComponentStoreData<CpuImpl, RuntimeStateImpl, FileSystem>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
    FileSystem: Send,
{
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(allow_instance_resource_growth(
            self.instance(),
            desired,
            maximum,
        ))
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

impl<CpuImpl, RuntimeStateImpl, FileSystem> IoView
    for ComponentStoreData<CpuImpl, RuntimeStateImpl, FileSystem>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
{
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

#[wasmtime_wasi_io::async_trait]
impl<CpuImpl, RuntimeStateImpl> Pollable for ComponentOutputStream<CpuImpl, RuntimeStateImpl>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
{
    async fn ready(&mut self) {}
}

#[wasmtime_wasi_io::async_trait]
impl<CpuImpl, RuntimeStateImpl> OutputStream for ComponentOutputStream<CpuImpl, RuntimeStateImpl>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
{
    fn write(&mut self, bytes: Bytes) -> Result<(), StreamError> {
        self.write_output(bytes.as_ref());
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
impl<CpuImpl, RuntimeStateImpl> Pollable
    for crate::DeadlinePollable<CpuImpl, RuntimeStateImpl>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
{
    async fn ready(&mut self) {
        while self.uptime_nanos() < self.deadline_nanos() {
            crate::yield_now().await;
        }
    }
}

/// Hook adapter that translates `wasmtime::CallHook` into kernel instance
/// execution transitions.
pub(crate) fn translate_call_hook(hook: CallHook) -> crate::InstanceExecutionTransition {
    match hook {
        CallHook::CallingWasm | CallHook::ReturningFromHost => {
            crate::InstanceExecutionTransition::Resume
        }
        CallHook::ReturningFromWasm | CallHook::CallingHost => {
            crate::InstanceExecutionTransition::Pause
        }
    }
}
