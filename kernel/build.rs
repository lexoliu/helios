use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use askama::Template;
use serde::Deserialize;
use walkdir::WalkDir;

#[derive(Template)]
#[template(path = "embedded_init.rs.askama", escape = "none")]
struct EmbeddedInitTemplate<'a> {
    name: &'a str,
    component: &'a str,
    argv0: &'a str,
    bootfs_entries: &'a [EmbeddedBootAsset],
}

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
    println!("cargo:rustc-check-cfg=cfg(helios_watchdog_self_test)");
    println!("cargo:rerun-if-env-changed=HELIOS_BUILD_TARGET");
    println!("cargo:rerun-if-env-changed=HELIOS_KERNEL_PREBUILD_MANIFEST");
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
    let init_destination = out_dir.join("embedded_init.rs");
    let init_source = generate_embedded_init(&out_dir, &target);
    fs::write(init_destination, init_source).expect("failed to write embedded init description");
}

fn generate_embedded_init(out_dir: &Path, target: &str) -> String {
    let prebuild = read_kernel_prebuild_manifest(target);
    write_trusted_root_file(
        out_dir,
        &prebuild.root_public_key,
        &prebuild.root_secret_key,
    );

    let component_path = prebuild.init_component;
    let name = component_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_else(|| panic!("{} has no valid UTF-8 file name", component_path.display()));
    let bootfs_entries = embedded_bootfs_entries(
        &prebuild.bootfs_root,
        &prebuild
            .bootfs_assets
            .into_iter()
            .map(|asset| EmbeddedBootAsset {
                path: asset.path,
                source: asset.source.display().to_string(),
                modified_nanos: file_modified_nanos(&asset.source),
            })
            .collect::<Vec<_>>(),
    );

    let component = component_path.display().to_string();
    EmbeddedInitTemplate {
        name,
        component: &component,
        argv0: &prebuild.init_argv0,
        bootfs_entries: &bootfs_entries,
    }
    .render()
    .unwrap_or_else(|error| panic!("failed to render embedded init template: {error}"))
}

#[derive(Deserialize)]
struct PrebuildManifest {
    target: String,
    init_component: PathBuf,
    init_argv0: String,
    bootfs_root: PathBuf,
    root_public_key: PathBuf,
    root_secret_key: PathBuf,
    bootfs_assets: Vec<BootAssetManifest>,
}

#[derive(Deserialize)]
struct BootAssetManifest {
    path: String,
    source: PathBuf,
}

fn read_kernel_prebuild_manifest(target: &str) -> PrebuildManifest {
    let manifest_path = env::var_os("HELIOS_KERNEL_PREBUILD_MANIFEST")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            panic!(
                "HELIOS_KERNEL_PREBUILD_MANIFEST must point to a cli-generated kernel-prebuild.json"
            )
        });
    println!("cargo:rerun-if-changed={}", manifest_path.display());
    let manifest = fs::read(&manifest_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", manifest_path.display()));
    let mut manifest: PrebuildManifest = serde_json::from_slice(&manifest)
        .unwrap_or_else(|error| panic!("failed to decode {}: {error}", manifest_path.display()));
    absolutize_prebuild_manifest_paths(&manifest_path, &mut manifest);
    assert!(
        manifest.target == target,
        "prebuild target {} does not match kernel target {}",
        manifest.target,
        target
    );
    rerun_if_changed_recursive(&manifest.bootfs_root);
    println!(
        "cargo:rerun-if-changed={}",
        manifest.init_component.display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest.root_public_key.display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest.root_secret_key.display()
    );
    for asset in &manifest.bootfs_assets {
        println!("cargo:rerun-if-changed={}", asset.source.display());
    }
    manifest
}

fn absolutize_prebuild_manifest_paths(manifest_path: &Path, manifest: &mut PrebuildManifest) {
    let manifest_dir = manifest_path
        .parent()
        .unwrap_or_else(|| panic!("{} has no parent directory", manifest_path.display()));
    manifest.init_component = manifest_relative_path(manifest_dir, &manifest.init_component);
    manifest.bootfs_root = manifest_relative_path(manifest_dir, &manifest.bootfs_root);
    manifest.root_public_key = manifest_relative_path(manifest_dir, &manifest.root_public_key);
    manifest.root_secret_key = manifest_relative_path(manifest_dir, &manifest.root_secret_key);
    for asset in &mut manifest.bootfs_assets {
        asset.source = manifest_relative_path(manifest_dir, &asset.source);
    }
}

fn manifest_relative_path(manifest_dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_owned();
    }
    manifest_dir.join(path)
}

fn write_trusted_root_file(out_dir: &Path, root_public_key: &Path, root_secret_key: &Path) {
    let bytes = fs::read(root_public_key)
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
    fs::write(&destination, source)
        .unwrap_or_else(|error| panic!("failed to write {}: {error}", destination.display()));

    let secret_bytes = fs::read(root_secret_key)
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
    fs::write(&signing_destination, signing_source).unwrap_or_else(|error| {
        panic!("failed to write {}: {error}", signing_destination.display())
    });
}

struct EmbeddedBootAsset {
    path: String,
    source: String,
    modified_nanos: u64,
}

fn embedded_bootfs_entries(
    root: &Path,
    extra_assets: &[EmbeddedBootAsset],
) -> Vec<EmbeddedBootAsset> {
    let mut entries = Vec::new();

    for entry in WalkDir::new(root).sort_by_file_name() {
        let entry = entry.unwrap_or_else(|error| {
            panic!(
                "failed to walk embedded bootfs root {}: {error}",
                root.display()
            )
        });
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.into_path();
        let relative = path
            .strip_prefix(root)
            .unwrap_or_else(|error| {
                panic!(
                    "failed to strip embedded bootfs root {} from {}: {error}",
                    root.display(),
                    path.display()
                )
            })
            .to_str()
            .unwrap_or_else(|| panic!("{} is not valid UTF-8", path.display()))
            .replace('\\', "/");
        entries.push(EmbeddedBootAsset {
            path: relative,
            source: path.display().to_string(),
            modified_nanos: file_modified_nanos(&path),
        });
    }

    for asset in extra_assets {
        entries.push(EmbeddedBootAsset {
            path: asset.path.clone(),
            source: asset.source.clone(),
            modified_nanos: asset.modified_nanos,
        });
    }

    entries.sort_by(|left, right| left.path.cmp(&right.path));
    entries
}

fn file_modified_nanos(path: &Path) -> u64 {
    let modified = fs::metadata(path)
        .unwrap_or_else(|error| panic!("failed to read metadata for {}: {error}", path.display()))
        .modified()
        .unwrap_or_else(|error| {
            panic!(
                "failed to read modification time for {}: {error}",
                path.display()
            )
        });
    let duration = modified.duration_since(UNIX_EPOCH).unwrap_or_else(|error| {
        panic!(
            "modification time for {} is before the Unix epoch: {error}",
            path.display()
        )
    });
    duration
        .as_secs()
        .checked_mul(1_000_000_000)
        .and_then(|seconds| seconds.checked_add(u64::from(duration.subsec_nanos())))
        .unwrap_or_else(|| panic!("modification time for {} overflowed u64", path.display()))
}

fn rerun_if_changed_recursive(root: &Path) {
    if !root.exists() {
        return;
    }

    for entry in WalkDir::new(root).sort_by_file_name() {
        let entry = entry.unwrap_or_else(|error| {
            panic!(
                "failed to walk {} for rerun tracking: {error}",
                root.display()
            )
        });
        println!("cargo:rerun-if-changed={}", entry.path().display());
    }
}
