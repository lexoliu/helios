//! The real compiler plugin, hosted under Wasmtime on this machine.
//!
//! `just test-units` builds the plugin with `helios-cli kernel-prebuild` and
//! names the module it wrote in `HELIOS_COMPILER_PLUGIN_WASM`.

use std::num::NonZeroU32;

use helios_artifact::cwasm_target_isa_levels;
use helios_compiler_abi::{CompileHint, OutputKind};
use helios_compiler_host::{CompilerHost, CompilerHostError, on_wasm_thread};
use wasmtime::{Engine, Precompiled};

const PLUGIN_ENV: &str = "HELIOS_COMPILER_PLUGIN_WASM";

#[test]
fn the_real_plugin_compiles_a_module_for_this_host() {
    let path = std::env::var_os(PLUGIN_ENV).unwrap_or_else(|| {
        panic!("{PLUGIN_ENV} must name kernel-prebuild's compiler_plugin.wasm; `just test-units` sets it")
    });
    let plugin = std::fs::read(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.to_string_lossy()));
    let input = wat::parse_str(r#"(module (func (export "add") (param i32 i32) (result i32) local.get 0 local.get 1 i32.add))"#)
        .expect("the input assembles");
    let target = target_lexicon::HOST.to_string();
    let workers = NonZeroU32::new(2).expect("two workers");
    let isa_level = cwasm_target_isa_levels(&target).floor().name();
    let compiled = on_wasm_thread(move || -> Result<_, CompilerHostError> {
        let mut host = CompilerHost::new(&plugin, workers)?;
        host.compile(&input, &target, isa_level, CompileHint::Performance)
    })
    .expect("the driver thread runs")
    .expect("the plugin compiles the module");
    assert_eq!(compiled.kind, OutputKind::CoreModule);
    assert_eq!(
        Engine::detect_precompiled(&compiled.output),
        Some(Precompiled::Module)
    );
}
