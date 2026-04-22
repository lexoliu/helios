use alloc::borrow::ToOwned;
use alloc::format;
use alloc::sync::Arc;

use crate::CodegenPlatform;
use wasmtime::component::{Component, Instance, TypedFunc};
use wasmtime::{AsContextMut, CustomCodeMemory, Engine, OptLevel, RegallocAlgorithm};

const COMPONENT_MEMORY_RESERVATION_FOR_GROWTH: u64 = 1 << 20;
const WASI_CLI_RUN_FUNC: &str = "run";

struct PlatformCodeMemory<P> {
    platform: P,
}

impl<P: CodegenPlatform + Clone> CustomCodeMemory for PlatformCodeMemory<P> {
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

fn build_engine_for_platform<P: CodegenPlatform + Clone>(
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
    config.cranelift_opt_level(OptLevel::None);
    config.cranelift_regalloc_algorithm(RegallocAlgorithm::SinglePass);
    config.cranelift_debug_verifier(false);
    config.with_custom_code_memory(Some(Arc::new(PlatformCodeMemory {
        platform: platform.clone(),
    })));
    config.signals_based_traps(false);
    config.memory_guard_size(0);
    config.memory_reservation(0);
    config.memory_reservation_for_growth(COMPONENT_MEMORY_RESERVATION_FOR_GROWTH);
    config.memory_init_cow(false);
    Engine::new(&config)
}

pub fn build_component_engine_for_platform<P: CodegenPlatform + Clone>(
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
