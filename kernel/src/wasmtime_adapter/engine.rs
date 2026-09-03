use alloc::borrow::ToOwned;
use alloc::sync::Arc;

use crate::wasmtime_adapter::user_memory::UserMemoryCreator;
use helios_hal::cpu::Cpu;
#[cfg(target_os = "none")]
use helios_hal::pmm::PhysFrame;
use thiserror::Error;
#[cfg(target_os = "none")]
use wasmtime::CustomCodeMemory;
use wasmtime::component::{Component, Instance, TypedFunc};
use wasmtime::{AsContextMut, Engine};

const WASI_CLI_RUN_FUNC: &str = "run";
#[cfg(any(
    not(target_os = "none"),
    all(target_os = "none", feature = "wasmtime-aarch64")
))]
const POOLING_MAX_UNUSED_WARM_SLOTS: u32 = 100;

#[derive(Debug, Error)]
enum WasiCliRunResolveError {
    #[error("component export interface starting with `wasi:cli/run` was not found")]
    InterfaceMissing,
    #[error("component run interface was not found on instance")]
    InterfaceExportMissing,
    #[error("component run interface does not expose `run`")]
    FunctionMissing,
    #[error("component run function has an invalid type")]
    FunctionTypeMismatch(#[source] wasmtime::Error),
}

#[cfg(target_os = "none")]
struct PlatformCodeMemory<P> {
    platform: P,
}

#[cfg(target_os = "none")]
impl<P: Cpu + Clone> CustomCodeMemory for PlatformCodeMemory<P> {
    fn required_alignment(&self) -> usize {
        PhysFrame::SIZE
    }

    fn publish_executable(&self, ptr: *const u8, len: usize) -> wasmtime::Result<()> {
        self.platform.publish_executable(ptr, len);
        Ok(())
    }

    fn unpublish_executable(&self, ptr: *const u8, len: usize) -> wasmtime::Result<()> {
        self.platform.unpublish_executable(ptr, len);
        Ok(())
    }
}

fn build_engine_for_platform<P: Cpu + Clone>(
    platform: &P,
    concurrency_support: bool,
) -> wasmtime::Result<Engine> {
    let mut config = build_component_engine_config(env!("HELIOS_BUILD_TARGET"));
    config.concurrency_support(concurrency_support);
    if let Some(probe) = platform.native_feature_probe() {
        unsafe {
            config.detect_host_feature(probe);
        }
    }
    #[cfg(target_os = "none")]
    config.with_custom_code_memory(Some(Arc::new(PlatformCodeMemory {
        platform: platform.clone(),
    })));
    config.signals_based_traps(true);
    let target_uses_lazy_commit =
        helios_artifact::cwasm_target_uses_lazy_commit_virtual_memory(env!("HELIOS_BUILD_TARGET"));
    assert!(
        platform.has_lazy_commit_virtual_memory() == target_uses_lazy_commit,
        "platform lazy-commit virtual-memory capability does not match cwasm target profile"
    );
    if configure_pooling(&mut config).applied {
        // Pooling path took ownership of the linear-memory
        // configuration. UserMemoryCreator must not be installed —
        // it uses the kernel buddy heap which cannot satisfy
        // wasmtime's per-slot pre-reservations.
        if platform.has_lazy_commit_virtual_memory() {
            config.memory_init_cow(true);
            config.memory_may_move(false);
        }
    } else if platform.has_lazy_commit_virtual_memory() {
        // Bare-metal custom-vm builds route Wasmtime's default memory
        // creator through the backend-installed `wasmtime_mmap_*`
        // hooks. Do not install `UserMemoryCreator` here: combining
        // `has_virtual_memory=true` with the buddy-heap host-memory
        // creator drives Wasmtime through a mismatched upper/lower
        // memory stack and is the #16 RPC-stall failure mode.
        config.memory_init_cow(true);
        config.memory_may_move(false);
    } else {
        // Backends without a real VM stack (or builds without
        // `pooling-allocator`) rely on the kernel-side buddy heap;
        // preserve the historical OnDemand path with explicit
        // "no reservation, allocate exactly the wasm module's
        // declared minimum" tuning so SharedMemory requests never
        // grow physical RAM beyond what the wasm guest actually
        // needs.
        config.with_host_memory(Arc::new(UserMemoryCreator::<P>::new(platform.clone())));
        config.memory_guard_size(helios_artifact::CWASM_NO_VMEM_MEMORY_GUARD_SIZE);
        config.memory_reservation(helios_artifact::CWASM_NO_VMEM_MEMORY_RESERVATION);
        config.memory_reservation_for_growth(
            helios_artifact::CWASM_NO_VMEM_MEMORY_RESERVATION_FOR_GROWTH,
        );
        config.memory_init_cow(false);
    }
    Engine::new(&config)
}

struct PoolingConfiguration {
    applied: bool,
}

/// Apply Wasmtime's pooling instance allocator to `config` when this
/// build links a wasmtime variant that ships the pooling-allocator
/// feature.
#[cfg(all(target_os = "none", feature = "wasmtime-aarch64"))]
fn configure_pooling(config: &mut wasmtime::Config) -> PoolingConfiguration {
    apply_pooling_config(config);
    PoolingConfiguration { applied: true }
}

#[cfg(all(target_os = "none", not(feature = "wasmtime-aarch64")))]
fn configure_pooling(config: &mut wasmtime::Config) -> PoolingConfiguration {
    config.async_stack_zeroing(false);
    PoolingConfiguration { applied: false }
}

#[cfg(not(target_os = "none"))]
fn configure_pooling(config: &mut wasmtime::Config) -> PoolingConfiguration {
    apply_pooling_config(config);
    PoolingConfiguration { applied: true }
}

#[cfg(any(
    not(target_os = "none"),
    all(target_os = "none", feature = "wasmtime-aarch64")
))]
fn apply_pooling_config(config: &mut wasmtime::Config) {
    use wasmtime::{InstanceAllocationStrategy, PoolingAllocationConfig};
    let mut pooling = PoolingAllocationConfig::default();
    pooling.max_unused_warm_slots(POOLING_MAX_UNUSED_WARM_SLOTS);
    pooling.async_stack_keep_resident(super::config::COMPONENT_ASYNC_STACK_SIZE);
    config.allocation_strategy(InstanceAllocationStrategy::Pooling(pooling));
    config.async_stack_zeroing(false);
}

pub fn build_component_engine_for_platform<P: Cpu + Clone>(
    platform: &P,
) -> wasmtime::Result<Engine> {
    build_engine_for_platform(platform, true)
}

pub fn resolve_wasi_cli_run<T: 'static>(
    component: &Component,
    instance: &Instance,
    mut store: impl AsContextMut<Data = T>,
) -> wasmtime::Result<TypedFunc<(), (core::result::Result<(), ()>,)>> {
    let run_interface_name = component
        .component_type()
        .exports(component.engine())
        .find_map(|(name, item)| {
            (name.starts_with("wasi:cli/run")
                && matches!(
                    item.ty,
                    wasmtime::component::types::ComponentItem::ComponentInstance(_)
                ))
            .then(|| name.to_owned())
        })
        .ok_or_else(|| wasmtime::Error::new(WasiCliRunResolveError::InterfaceMissing))?;
    let mut store = store.as_context_mut();
    let run_interface = instance
        .get_export_index(&mut store, None, &run_interface_name)
        .ok_or_else(|| wasmtime::Error::new(WasiCliRunResolveError::InterfaceExportMissing))?;
    let run = instance
        .get_export_index(&mut store, Some(&run_interface), WASI_CLI_RUN_FUNC)
        .ok_or_else(|| wasmtime::Error::new(WasiCliRunResolveError::FunctionMissing))?;
    instance
        .get_typed_func::<(), (core::result::Result<(), ()>,)>(&mut store, &run)
        .map_err(|error| {
            wasmtime::Error::new(WasiCliRunResolveError::FunctionTypeMismatch(error))
        })
}

use super::config::build_component_engine_config;
