fn main() {
    println!("cargo:rerun-if-env-changed=HELIOS_WATCHDOG_TIMEOUT_SECS");
    if let Ok(timeout) = std::env::var("HELIOS_WATCHDOG_TIMEOUT_SECS") {
        println!("cargo:rustc-env=HELIOS_WATCHDOG_TIMEOUT_SECS={timeout}");
    }
}
