use std::env;

fn main() {
    let target = env::var("TARGET").unwrap_or_else(|error| {
        panic!("failed to read hosted build target triple from Cargo: {error}")
    });
    println!("cargo:rustc-env=HELIOS_HOSTED_TARGET={target}");
}
