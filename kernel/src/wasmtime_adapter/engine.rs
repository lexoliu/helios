use alloc::borrow::ToOwned;
use alloc::format;
use alloc::sync::Arc;

use crate::UserMemoryCreator;
use helios_hal::cpu::Cpu;
use wasmtime::component::{Component, Instance, TypedFunc};
use wasmtime::{AsContextMut, CustomCodeMemory, Engine};

const WASI_CLI_RUN_FUNC: &str = "run";

struct PlatformCodeMemory<P> {
    platform: P,
}

impl<P: Cpu + Clone> CustomCodeMemory for PlatformCodeMemory<P> {
    fn required_alignment(&self) -> usize {
        1
    }

    fn publish_executable(&self, ptr: *const u8, len: usize) -> wasmtime::Result<()> {
        self.platform.publish_executable(ptr, len);
        Ok(())
    }

    fn unpublish_executable(&self, _ptr: *const u8, _len: usize) -> wasmtime::Result<()> {
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
    config.with_custom_code_memory(Some(Arc::new(PlatformCodeMemory {
        platform: platform.clone(),
    })));
    config.signals_based_traps(true);
    if platform.has_lazy_commit_virtual_memory() && configure_pooling(&mut config) {
        // Pooling path took ownership of the linear-memory
        // configuration. UserMemoryCreator must not be installed —
        // it uses the kernel buddy heap which cannot satisfy
        // wasmtime's per-slot pre-reservations.
    } else {
        // Backends without a real VM stack (or builds without
        // `pooling-allocator`) rely on the kernel-side buddy heap;
        // preserve the historical OnDemand path with explicit
        // "no reservation, allocate exactly the wasm module's
        // declared minimum" tuning so SharedMemory requests never
        // grow physical RAM beyond what the wasm guest actually
        // needs.
        config.with_host_memory(Arc::new(UserMemoryCreator));
        config.memory_guard_size(helios_artifact::CWASM_NO_VMEM_MEMORY_GUARD_SIZE);
        config.memory_reservation(helios_artifact::CWASM_NO_VMEM_MEMORY_RESERVATION);
        config.memory_reservation_for_growth(
            helios_artifact::CWASM_NO_VMEM_MEMORY_RESERVATION_FOR_GROWTH,
        );
        config.memory_init_cow(false);
    }
    Engine::new(&config)
}

/// Apply Wasmtime's pooling instance allocator to `config` if this
/// build links a wasmtime variant that ships the pooling-allocator
/// feature (currently the hosted target — wasmtime upstream still
/// gates pooling on `std`, which is unavailable on bare-metal targets
/// that link the `custom-virtual-memory` ABI instead).
///
/// Returns `true` when the pooling path was actually applied so the
/// caller can skip the `OnDemand`-tuned host-memory wiring; returns
/// `false` on bare-metal where the function is a structural no-op.
#[cfg(target_os = "none")]
fn configure_pooling(_config: &mut wasmtime::Config) -> bool {
    false
}

#[cfg(not(target_os = "none"))]
fn configure_pooling(config: &mut wasmtime::Config) -> bool {
    use wasmtime::{InstanceAllocationStrategy, PoolingAllocationConfig};
    config.allocation_strategy(InstanceAllocationStrategy::Pooling(
        PoolingAllocationConfig::default(),
    ));
    config.async_stack_zeroing(false);
    true
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
                    item,
                    wasmtime::component::types::ComponentItem::ComponentInstance(_)
                ))
            .then(|| name.to_owned())
        })
        .ok_or_else(|| {
            wasmtime::Error::msg(format!(
                "component export interface starting with `wasi:cli/run` was not found"
            ))
        })?;
    let mut store = store.as_context_mut();
    let run_interface = instance
        .get_export_index(&mut store, None, &run_interface_name)
        .ok_or_else(|| {
            wasmtime::Error::msg(format!(
                "component export interface `{run_interface_name}` was not found on instance"
            ))
        })?;
    let run = instance
        .get_export_index(&mut store, Some(&run_interface), WASI_CLI_RUN_FUNC)
        .ok_or_else(|| {
            wasmtime::Error::msg(format!(
                "component export `{run_interface_name}` does not expose `{WASI_CLI_RUN_FUNC}`"
            ))
        })?;
    instance
        .get_typed_func::<(), (core::result::Result<(), ()>,)>(&mut store, &run)
        .map_err(|error| {
            wasmtime::Error::msg(format!(
                "failed to type-check `{run_interface_name}.{WASI_CLI_RUN_FUNC}`: {error:#}"
            ))
        })
}

use super::config::build_component_engine_config;
