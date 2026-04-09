use wasmtime::Config;

pub fn build_target_engine_config(target: &str) -> Config {
    let mut config = Config::new();
    config
        .target(target)
        .expect("Helios build target must be accepted by Wasmtime");
    config
}


pub fn build_component_engine_config(target: &str) -> Config {
    let mut config = build_target_engine_config(target);
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    config
}


pub fn build_component_engine(target: &str) -> wasmtime::Result<wasmtime::Engine> {
    let config = build_component_engine_config(target);
    wasmtime::Engine::new(&config)
}
