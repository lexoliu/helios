use wasmtime::Config;

pub const COMPONENT_ASYNC_STACK_SIZE: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AotCompileHint {
    Fast,
    Balanced,
    Performance,
}

pub fn build_target_engine_config(target: &str) -> Config {
    let mut config = Config::new();
    config
        .target(target)
        .expect("Helios build target must be accepted by Wasmtime");
    #[cfg(all(
        target_arch = "x86_64",
        target_os = "none",
        not(target_feature = "soft-float")
    ))]
    unsafe {
        // Helios' custom x86 bare-metal target uses the hardware floating-point
        // ABI, so Wasmtime's x86_64-unknown-none soft-float guard does not
        // apply to this kernel build.
        config.x86_float_abi_ok(true);
    }
    config
}

pub fn build_component_engine_config(target: &str) -> Config {
    let mut config = build_target_engine_config(target);
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_builtins(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_component_model_threading(true);
    config.wasm_simd(true);
    config.wasm_relaxed_simd(true);
    config.relaxed_simd_deterministic(false);
    config.wasm_multi_memory(true);
    config.wasm_memory64(true);
    config.wasm_tail_call(true);
    config.wasm_threads(true);
    config.shared_memory(true);
    config.gc_support(true);
    config.wasm_reference_types(true);
    config.wasm_function_references(true);
    config.epoch_interruption(true);
    // CPython's class construction recurses deeply; the default
    // 512 KB stack is not enough. Give component instances an 8 MB
    // stack (both for the guest wasm call stack and for the host
    // Rust async frames that drive it).
    config.max_wasm_stack(COMPONENT_ASYNC_STACK_SIZE);
    config.async_stack_size(COMPONENT_ASYNC_STACK_SIZE);
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_encoder::{
        CodeSection, ExportKind, ExportSection, Function, FunctionSection, Instruction, Module,
        TypeSection, ValType,
    };

    #[test]
    fn component_engine_config_keeps_simd_and_relaxed_simd_enabled() {
        let config = build_component_engine_config(env!("HELIOS_BUILD_TARGET"));
        wasmtime::Engine::new(&config).expect("component engine config should build");

        let operators = relaxed_simd_module_operator_set();
        assert!(operators.simd, "SIMD operator must be present in probe wasm");
        assert!(
            operators.relaxed_simd,
            "relaxed SIMD operator must be present in probe wasm"
        );
    }

    fn relaxed_simd_module() -> alloc::vec::Vec<u8> {
        let mut module = Module::new();

        let mut types = TypeSection::new();
        types.ty().function([], [ValType::I32]);
        module.section(&types);

        let mut functions = FunctionSection::new();
        functions.function(0);
        module.section(&functions);

        let mut exports = ExportSection::new();
        exports.export("relaxed-simd", ExportKind::Func, 0);
        module.section(&exports);

        let mut body = Function::new(core::iter::empty::<(u32, ValType)>());
        body.instruction(&Instruction::V128Const(0));
        body.instruction(&Instruction::V128Const(0));
        body.instruction(&Instruction::I8x16RelaxedSwizzle);
        body.instruction(&Instruction::I32x4ExtractLane(0));
        body.instruction(&Instruction::End);

        let mut code = CodeSection::new();
        code.function(&body);
        module.section(&code);

        module.finish()
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct SimdOperatorSet {
        simd: bool,
        relaxed_simd: bool,
    }

    fn relaxed_simd_module_operator_set() -> SimdOperatorSet {
        let wasm = relaxed_simd_module();
        SimdOperatorSet {
            simd: contains_subslice(&wasm, &[0xfd, 0x0c])
                && contains_subslice(&wasm, &[0xfd, 0x1b, 0x00]),
            relaxed_simd: contains_subslice(&wasm, &[0xfd, 0x80, 0x02]),
        }
    }

    fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }
}
