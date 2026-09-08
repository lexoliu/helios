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
/// Elements one pooled table slot holds.
///
/// Set here rather than inherited, because the table pool's
/// address-space cost is this count times a pointer per slot and the
/// kernel is what has to fit that in its window. The value is what
/// Wasmtime has always defaulted to, and every component this tree
/// builds declares far smaller tables.
const POOLING_TABLE_ELEMENTS: usize = 20_000;

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

/// The pool sizes that follow from the kernel's instance budget.
///
/// Wasmtime's `InstanceLimits` default is one number — 1000 on a 64-bit
/// target — assigned to every total it has, so leaving them alone made
/// the kernel's concurrency a value it never chose, and bound it on
/// *core* instances rather than on programs: a component instantiates
/// three or four core modules, so the ceiling arrived at roughly a third
/// of the programs it appeared to promise (#284).
///
/// Each field is the budget times what one component may draw, which is
/// what a pool has to hold for the budget to mean anything under load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PoolingBudget {
    component_instances: u32,
    core_instances: u32,
    memories: u32,
    tables: u32,
    stacks: u32,
    gc_heaps: u32,
}

impl PoolingBudget {
    /// The pools the kernel's own limits imply.
    ///
    /// Address space is what the memory and table pools cost: a memory
    /// slot reserves `CWASM_MEMORY_RESERVATION + CWASM_MEMORY_GUARD_SIZE`
    /// whether or not anything runs in it, so `memories` is the field to
    /// read before widening anything. Every bare-metal backend hands the
    /// runtime a 32 TiB window (`USER_VA_BASE..USER_VA_END`), and these
    /// numbers reserve about 8.3 TiB of it.
    const fn of_kernel_policy() -> Self {
        Self {
            component_instances: MAX_CONCURRENT_INSTANCES,
            core_instances: MAX_CONCURRENT_INSTANCES * MAX_CORE_INSTANCES_PER_COMPONENT,
            memories: MAX_CONCURRENT_INSTANCES * MAX_MEMORIES_PER_COMPONENT,
            tables: MAX_CONCURRENT_INSTANCES * MAX_TABLES_PER_COMPONENT,
            // One fiber stack and one GC heap per live instance: a guest
            // thread takes a second store over the same instance, and the
            // budget counts instances.
            stacks: MAX_CONCURRENT_INSTANCES,
            gc_heaps: MAX_CONCURRENT_INSTANCES,
        }
    }
}

impl PoolingBudget {
    /// Address space the memory and table pools reserve up front.
    ///
    /// Only these two pools pre-reserve: a memory slot takes the whole
    /// `CWASM_MEMORY_RESERVATION + CWASM_MEMORY_GUARD_SIZE` so that
    /// Cranelift can drop the bounds check behind it, and a table slot
    /// takes its element array. Instance metadata and fiber stacks are
    /// taken as they are used.
    const fn reserved_address_space(&self) -> u64 {
        let per_memory =
            helios_artifact::CWASM_MEMORY_RESERVATION + helios_artifact::CWASM_MEMORY_GUARD_SIZE;
        let per_table = (POOLING_TABLE_ELEMENTS as u64) * (size_of::<usize>() as u64);
        (self.memories as u64) * per_memory + (self.tables as u64) * per_table
    }
}

fn apply_pooling_config(config: &mut wasmtime::Config) {
    use wasmtime::{InstanceAllocationStrategy, PoolingAllocationConfig};
    let budget = PoolingBudget::of_kernel_policy();
    // The pools are reserved out of the one window every backend hands
    // the runtime; a budget that does not fit it is a boot failure on
    // three targets, so it fails here where the number is written.
    assert!(
        budget.reserved_address_space() <= MAX_POOLED_ADDRESS_SPACE,
        "the instance pools reserve more address space than the kernel allows"
    );
    let mut pooling = PoolingAllocationConfig::default();
    pooling.max_unused_warm_slots(POOLING_MAX_UNUSED_WARM_SLOTS);
    pooling.async_stack_keep_resident(super::config::COMPONENT_ASYNC_STACK_SIZE);
    pooling.total_component_instances(budget.component_instances);
    pooling.total_core_instances(budget.core_instances);
    pooling.total_memories(budget.memories);
    pooling.total_tables(budget.tables);
    pooling.total_stacks(budget.stacks);
    pooling.total_gc_heaps(budget.gc_heaps);
    // What one component may draw. Left at `u32::MAX` by the default,
    // which means a single component could take the pool down and the
    // failure would arrive at some other instance.
    pooling.max_core_instances_per_component(MAX_CORE_INSTANCES_PER_COMPONENT);
    pooling.max_memories_per_component(MAX_MEMORIES_PER_COMPONENT);
    pooling.max_tables_per_component(MAX_TABLES_PER_COMPONENT);
    pooling.table_elements(POOLING_TABLE_ELEMENTS);
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

use super::config::{
    MAX_CONCURRENT_INSTANCES, MAX_CORE_INSTANCES_PER_COMPONENT, MAX_MEMORIES_PER_COMPONENT,
    MAX_POOLED_ADDRESS_SPACE, MAX_TABLES_PER_COMPONENT, build_component_engine_config,
};

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
    /// The pools follow the kernel's own limits, and the memory pool is
    /// what they cost.
    ///
    /// Until #284 every total was Wasmtime's `InstanceLimits` default —
    /// one number, 1000, for component instances, core instances,
    /// memories, tables, stacks and GC heaps alike. Bound on *core*
    /// instances, that ceiling arrives at roughly a third of the
    /// programs it reads as: `instance-startup-500` never completed on
    /// any bench run.
    #[test]
    fn the_pools_follow_the_kernel_instance_budget() {
        let budget = PoolingBudget::of_kernel_policy();

        assert_eq!(budget.component_instances, MAX_CONCURRENT_INSTANCES);
        assert_eq!(budget.stacks, MAX_CONCURRENT_INSTANCES);
        assert_eq!(budget.gc_heaps, MAX_CONCURRENT_INSTANCES);
        // A component instantiates three or four core modules, so the
        // core pool has to be a multiple of the instance budget or the
        // budget is a number the runtime will never honour.
        assert!(budget.core_instances >= budget.component_instances * 4);
        assert_eq!(
            budget.core_instances,
            MAX_CONCURRENT_INSTANCES * MAX_CORE_INSTANCES_PER_COMPONENT
        );
        assert_eq!(
            budget.memories,
            MAX_CONCURRENT_INSTANCES * MAX_MEMORIES_PER_COMPONENT
        );
        assert_eq!(
            budget.tables,
            MAX_CONCURRENT_INSTANCES * MAX_TABLES_PER_COMPONENT
        );
    }

    /// The pools are reserved out of the 32 TiB window every backend
    /// hands the runtime, so the budget has to fit before three targets
    /// try to boot with it.
    #[test]
    fn the_pools_fit_the_address_space_the_kernel_allows() {
        let reserved = PoolingBudget::of_kernel_policy().reserved_address_space();

        assert!(
            reserved <= MAX_POOLED_ADDRESS_SPACE,
            "pools reserve {reserved} bytes, budget is {MAX_POOLED_ADDRESS_SPACE}"
        );
        // Nearly all of it is the memory pool, which is the term to read
        // when either limit is widened: a slot reserves the whole 4 GiB a
        // wasm32 guest can address, plus its guard region, before
        // anything runs in it. The table pool is three orders of
        // magnitude smaller, and nothing else pre-reserves at all.
        let memory_pool = u64::from(PoolingBudget::of_kernel_policy().memories)
            * (helios_artifact::CWASM_MEMORY_RESERVATION
                + helios_artifact::CWASM_MEMORY_GUARD_SIZE);
        assert!(
            memory_pool * 1000 > reserved * 999,
            "{memory_pool} of {reserved}"
        );
        assert!(
            reserved > 8 << 40,
            "the budget is documented as about 8.1 TiB"
        );
    }

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
