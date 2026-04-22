use wasmtime::Config;

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
    // CPython's class construction recurses deeply; the default
    // 512 KB stack is not enough. Give component instances an 8 MB
    // stack (both for the guest wasm call stack and for the host
    // Rust async frames that drive it).
    config.max_wasm_stack(8 * 1024 * 1024);
    config.async_stack_size(8 * 1024 * 1024);
    config
}
