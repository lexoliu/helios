fn main() {
    let target = std::env::var("TARGET").expect("Cargo did not provide TARGET");
    println!("cargo:rustc-env=HELIOS_BUILD_TARGET={target}");
}
