use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use askama::Template;

#[derive(Template)]
#[template(path = "trusted_roots.rs.askama", escape = "none")]
struct TrustedRootsTemplate<'a> {
    root_public_key: &'a [u8],
}

#[derive(Template)]
#[template(path = "trusted_signing_key.rs.askama", escape = "none")]
struct TrustedSigningKeyTemplate<'a> {
    root_secret_key: &'a [u8],
}

fn main() {
    // Set by the `profile-generate` build's rustflags, alongside
    // `-C profile-generate`, so the profile runtime and the instrumentation
    // it serves can never be compiled apart (docs/pgo.md).
    println!("cargo:rustc-check-cfg=cfg(helios_profile_generate)");
    println!("cargo:rustc-check-cfg=cfg(helios_watchdog_self_test)");
    println!("cargo:rerun-if-env-changed=HELIOS_BUILD_TARGET");
    println!("cargo:rerun-if-env-changed=HELIOS_KERNEL_ROOT_PUBLIC_KEY");
    println!("cargo:rerun-if-env-changed=HELIOS_KERNEL_ROOT_SECRET_KEY");
    println!("cargo:rerun-if-env-changed=HELIOS_WATCHDOG_SELF_TEST");
    println!("cargo:rerun-if-env-changed=HELIOS_WATCHDOG_SELF_TEST_DELAY_MS");

    let target = env::var("HELIOS_BUILD_TARGET")
        .or_else(|_| env::var("TARGET"))
        .unwrap_or_else(|error| panic!("failed to determine Helios build target triple: {error}"));
    println!("cargo:rustc-env=HELIOS_BUILD_TARGET={target}");
    if env::var_os("HELIOS_WATCHDOG_SELF_TEST").is_some() {
        println!("cargo:rustc-cfg=helios_watchdog_self_test");
        let delay_ms =
            env::var("HELIOS_WATCHDOG_SELF_TEST_DELAY_MS").unwrap_or_else(|_| "5000".to_owned());
        println!("cargo:rustc-env=HELIOS_WATCHDOG_SELF_TEST_DELAY_MS={delay_ms}");
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is missing"));
    write_trusted_root_file(&out_dir);
}

/// The kernel image carries the trusted root keys — verification of the
/// signed payload has to happen inside the signed artifact — and nothing
/// else the prebuild produces. The key files are the only inputs the
/// kernel build tracks: a `kernel-prebuild` rerun that changes the
/// user payload but leaves them byte-identical recompiles nothing here.
fn trusted_key_file(env_name: &str) -> PathBuf {
    let path = env::var_os(env_name)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("{env_name} must name the key file kernel-prebuild wrote"));
    println!("cargo:rerun-if-changed={}", path.display());
    path
}

fn write_trusted_root_file(out_dir: &Path) {
    let root_public_key = trusted_key_file("HELIOS_KERNEL_ROOT_PUBLIC_KEY");
    let root_secret_key = trusted_key_file("HELIOS_KERNEL_ROOT_SECRET_KEY");

    let bytes = fs::read(&root_public_key)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", root_public_key.display()));
    assert!(
        bytes.len() == 32,
        "trusted root public key {} must be 32 bytes, got {}",
        root_public_key.display(),
        bytes.len()
    );
    let destination = out_dir.join("trusted_roots.rs");
    let source = TrustedRootsTemplate {
        root_public_key: &bytes,
    }
    .render()
    .unwrap_or_else(|error| panic!("failed to render trusted roots template: {error}"));
    write_if_changed(&destination, source.as_bytes());

    let secret_bytes = fs::read(&root_secret_key)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", root_secret_key.display()));
    assert!(
        secret_bytes.len() == 32,
        "trusted root secret key {} must be 32 bytes, got {}",
        root_secret_key.display(),
        secret_bytes.len()
    );
    let signing_destination = out_dir.join("trusted_signing_key.rs");
    let signing_source = TrustedSigningKeyTemplate {
        root_secret_key: &secret_bytes,
    }
    .render()
    .unwrap_or_else(|error| panic!("failed to render trusted signing key template: {error}"));
    write_if_changed(&signing_destination, signing_source.as_bytes());
}

/// Writes `bytes` to `destination` only when the content differs. The
/// generated files feed `include!`, so a same-bytes rewrite would still
/// bump the mtime rustc's dep-info checks and recompile the kernel for
/// nothing — the whole point of the boot-module split is that a new
/// payload leaves the kernel binary alone.
fn write_if_changed(destination: &Path, bytes: &[u8]) {
    if fs::read(destination).ok().as_deref() == Some(bytes) {
        return;
    }
    fs::write(destination, bytes)
        .unwrap_or_else(|error| panic!("failed to write {}: {error}", destination.display()));
}
