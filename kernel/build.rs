use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use walkdir::WalkDir;
use wasmtime::{Config, Engine};
use wit_component::ComponentEncoder;

fn main() {
    println!("cargo:rerun-if-env-changed=HELIOS_BUILD_TARGET");
    println!("cargo:rerun-if-env-changed=HELIOS_INIT_WASM");
    println!("cargo:rerun-if-env-changed=HELIOS_INIT_ARGV0");
    println!("cargo:rerun-if-env-changed=HELIOS_BOOTFS_ROOT");
    rerun_if_changed_recursive(Path::new("../init"));
    rerun_if_changed_recursive(Path::new("../wit"));

    let target = env::var("HELIOS_BUILD_TARGET")
        .or_else(|_| env::var("TARGET"))
        .unwrap_or_else(|error| panic!("failed to determine Helios build target triple: {error}"));
    println!("cargo:rustc-env=HELIOS_BUILD_TARGET={target}");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is missing"));
    let destination = out_dir.join("embedded_init.rs");
    let source = generate_embedded_init(&out_dir, &target);
    fs::write(destination, source).expect("failed to write embedded init description");
}

fn generate_embedded_init(out_dir: &Path, target: &str) -> String {
    let component_path = resolve_init_component(out_dir);
    let argv0 = env::var("HELIOS_INIT_ARGV0").unwrap_or_else(|_| "/init.wasm".to_owned());
    let bootfs_root = resolve_bootfs_root(&component_path);
    assert!(
        bootfs_root.is_dir(),
        "HELIOS_BOOTFS_ROOT={} is not a directory",
        bootfs_root.display()
    );
    println!("cargo:rerun-if-changed={}", bootfs_root.display());

    let artifact = precompile_component(&component_path, target);
    let artifact_path = out_dir.join("embedded_init_component.cwasm");
    fs::write(&artifact_path, artifact).unwrap_or_else(|error| {
        panic!(
            "failed to write embedded init artifact {}: {error}",
            artifact_path.display()
        )
    });

    let name = component_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_else(|| panic!("{} has no valid UTF-8 file name", component_path.display()));
    let bootfs = render_embedded_bootfs(&bootfs_root);

    format!(
        "pub const EMBEDDED_INIT: Option<EmbeddedInitDescriptor> = Some(EmbeddedInitDescriptor {{\n    component: EmbeddedComponent::new(\n        r#\"{name}\"#,\n        r#\"{target}\"#,\n        include_bytes!(r#\"{artifact}\"#),\n    ),\n    argv0: r#\"{argv0}\"#,\n    bootfs: {bootfs},\n}});\n",
        name = name,
        target = target,
        artifact = artifact_path.display(),
        argv0 = argv0,
        bootfs = bootfs,
    )
}

fn resolve_init_component(out_dir: &Path) -> PathBuf {
    match env::var_os("HELIOS_INIT_WASM") {
        Some(path) => canonicalize_file(path.as_ref(), "HELIOS_INIT_WASM"),
        None => build_default_init_component(out_dir),
    }
}

fn resolve_bootfs_root(wasm_path: &Path) -> PathBuf {
    match env::var_os("HELIOS_BOOTFS_ROOT") {
        Some(path) => fs::canonicalize(path)
            .unwrap_or_else(|error| panic!("failed to resolve HELIOS_BOOTFS_ROOT: {error}")),
        None => {
            let default_root = Path::new("../init/bootfs");
            if default_root.is_dir() {
                return fs::canonicalize(default_root).unwrap_or_else(|error| {
                    panic!(
                        "failed to resolve default init bootfs {}: {error}",
                        default_root.display()
                    )
                });
            }

            wasm_path
                .parent()
                .unwrap_or_else(|| panic!("embedded init wasm must have a parent directory"))
                .to_path_buf()
        }
    }
}

fn build_default_init_component(out_dir: &Path) -> PathBuf {
    let manifest_path = Path::new("../init/Cargo.toml");
    assert!(
        manifest_path.is_file(),
        "default init crate manifest {} is missing",
        manifest_path.display()
    );

    let cargo = env::var_os("CARGO").expect("CARGO is missing");
    let target_dir = out_dir.join("helios-init-target");
    let mut command = Command::new(cargo);
    command
        .arg("build")
        .arg("--manifest-path")
        .arg(manifest_path)
        .arg("--target")
        .arg("wasm32-unknown-unknown")
        .arg("--target-dir")
        .arg(&target_dir);
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("failed to invoke cargo for init component build: {error}"));
    assert!(
        status.success(),
        "init component build failed with status {status}"
    );

    let profile = env::var("PROFILE").unwrap_or_else(|error| panic!("PROFILE is missing: {error}"));
    let profile_dir = match profile.as_str() {
        "debug" => "debug",
        "release" => "release",
        other => other,
    };
    let core_module_path = target_dir
        .join("wasm32-unknown-unknown")
        .join(profile_dir)
        .join("helios_init.wasm");
    let core_module_path = canonicalize_file(&core_module_path, "generated init core module");
    let component_path = out_dir.join("helios_init_component.wasm");
    let component = encode_component(&core_module_path);
    fs::write(&component_path, component).unwrap_or_else(|error| {
        panic!(
            "failed to write generated init component {}: {error}",
            component_path.display()
        )
    });
    canonicalize_file(&component_path, "generated init component")
}

fn canonicalize_file(path: &Path, label: &str) -> PathBuf {
    let wasm_path = fs::canonicalize(path)
        .unwrap_or_else(|error| panic!("failed to resolve {label} {}: {error}", path.display()));
    assert!(
        wasm_path.is_file(),
        "{label} {} is not a file",
        wasm_path.display()
    );
    println!("cargo:rerun-if-changed={}", wasm_path.display());
    wasm_path
}

fn precompile_component(path: &Path, target: &str) -> Vec<u8> {
    let mut config = Config::new();
    config
        .target(target)
        .unwrap_or_else(|error| panic!("invalid build target {target:?}: {error}"));
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)
        .unwrap_or_else(|error| panic!("failed to create wasmtime engine for {target}: {error}"));
    let wasm =
        fs::read(path).unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    engine.precompile_component(&wasm).unwrap_or_else(|error| {
        panic!("failed to precompile component {}: {error}", path.display())
    })
}

fn encode_component(path: &Path) -> Vec<u8> {
    let wasm =
        fs::read(path).unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    ComponentEncoder::default()
        .module(&wasm)
        .unwrap_or_else(|error| panic!("failed to load core module {}: {error}", path.display()))
        .validate(true)
        .encode()
        .unwrap_or_else(|error| panic!("failed to encode component {}: {error}", path.display()))
}

fn render_embedded_bootfs(root: &Path) -> String {
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
        entries.push(format!(
            "EmbeddedBootFile::new(r#\"{relative}\"#, include_bytes!(r#\"{path}\"#))",
            relative = relative,
            path = path.display(),
        ));
    }

    format!("EmbeddedBootFs::new(&[{}])", entries.join(", "))
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
