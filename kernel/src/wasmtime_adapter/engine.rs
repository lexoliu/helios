use alloc::borrow::ToOwned;
#[cfg(target_os = "none")]
use alloc::sync::Arc;

use helios_hal::cpu::Cpu;
#[cfg(target_os = "none")]
use helios_hal::pmm::PhysFrame;
use thiserror::Error;
#[cfg(target_os = "none")]
use wasmtime::CustomCodeMemory;
use wasmtime::component::{Component, Instance, TypedFunc};
use wasmtime::{AsContextMut, Engine};

const WASI_CLI_RUN_FUNC: &str = "run";
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
    let target = env!("HELIOS_BUILD_TARGET");
    let mut config = build_component_engine_config(target);
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
    // Every backend that gets this far serves wasmtime's virtual-memory ABI
    // from its own `hal::vmm::AddressSpace` (bare metal) or from the host
    // `mmap` (hosted). The pooling allocator's per-slot pre-reservations are
    // only affordable through such an address space, and Cranelift drops
    // linear-memory bounds checks only when the reservation and guard region
    // behind every slot are real. A backend without that capability has no
    // second memory stack to fall back to (AGENTS §3, §3.2), so it fails
    // here instead.
    assert!(
        platform.has_lazy_commit_virtual_memory(),
        "backend must provide lazy-commit virtual memory to host the Wasmtime pooling allocator"
    );
    // The page-fault trampoline blocks the faulting fiber through
    // `wasmtime::block_on_current_fiber`. A lazily committed address space is
    // exactly one that can take a page away underneath running guest code, so
    // the TLS slot that costs is asked for wherever that holds.
    config.block_on_current_fiber(true);
    apply_pooling_config(&mut config);
    config.memory_init_cow(true);
    config.memory_may_move(false);
    config.memory_reservation(helios_artifact::CWASM_MEMORY_RESERVATION);
    config.memory_guard_size(helios_artifact::CWASM_MEMORY_GUARD_SIZE);
    let engine = Engine::new(&config)?;
    tracing::info!(
        target,
        memory_reservation = engine.get_memory_reservation(),
        memory_guard_size = engine.get_memory_guard_size(),
        memory_init_cow = engine.get_memory_init_cow(),
        memory_may_move = engine.get_memory_may_move(),
        signals_based_traps = engine.get_signals_based_traps(),
        "component engine built with the pooling allocator on the lazy-commit memory profile"
    );
    Ok(engine)
}

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
        .map_err(|error| wasmtime::Error::new(WasiCliRunResolveError::FunctionTypeMismatch(error)))
}

use super::config::build_component_engine_config;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestCpu;

    /// The memory profile is a contract between this engine and the
    /// compiler plugin that produces its cwasm artifacts: the reservation
    /// and guard sizes a module was compiled against are the ones its
    /// elided bounds checks assume. Reading them back off the built engine
    /// is what proves the whole config pipeline — including wasmtime's own
    /// defaults and its pooling-allocator cross-checks — still resolves to
    /// the profile `helios-artifact` publishes.
    #[test]
    fn engine_resolves_the_lazy_commit_memory_profile() {
        let engine = build_component_engine_for_platform(&TestCpu::without_entropy())
            .expect("component engine should build on the lazy-commit profile");

        assert_eq!(
            engine.get_memory_reservation(),
            helios_artifact::CWASM_MEMORY_RESERVATION
        );
        assert_eq!(
            engine.get_memory_guard_size(),
            helios_artifact::CWASM_MEMORY_GUARD_SIZE
        );
        // A wasm32 guest cannot address past the reservation, so these three
        // together are what let Cranelift drop the bounds check: the
        // reservation never moves, the guard region catches a folded static
        // offset, and the fault becomes a trap rather than a signal the
        // runtime cannot see.
        assert!(!engine.get_memory_may_move());
        assert!(engine.get_signals_based_traps());
        assert!(engine.get_memory_init_cow());
    }

    /// The pooling allocator requires the GC-heap tunables to match the
    /// linear-memory ones and refuses to build an engine otherwise, so this
    /// is also what keeps the two sets from drifting apart as either side's
    /// defaults change.
    #[test]
    fn engine_gc_heap_profile_matches_linear_memory() {
        let engine = build_component_engine_for_platform(&TestCpu::without_entropy())
            .expect("component engine should build on the lazy-commit profile");

        assert_eq!(
            engine.get_gc_heap_reservation(),
            engine.get_memory_reservation()
        );
        assert_eq!(
            engine.get_gc_heap_guard_size(),
            engine.get_memory_guard_size()
        );
    }
}
