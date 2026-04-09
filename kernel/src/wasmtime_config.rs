use wasmtime::Config;

pub fn build_target_engine_config(target: &str) -> Config {
    let mut config = Config::new();
    config
        .target(target)
        .expect("Helios build target must be accepted by Wasmtime");
    config
}
