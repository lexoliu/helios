use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use walkdir::WalkDir;
use wasmparser::Parser;
use wit_component::ComponentEncoder;

fn main() {
    println!("cargo:rerun-if-env-changed=HELIOS_BUILD_TARGET");
    println!("cargo:rerun-if-env-changed=HELIOS_INIT_WASM");
    println!("cargo:rerun-if-env-changed=HELIOS_INIT_MANIFEST");
    println!("cargo:rerun-if-env-changed=HELIOS_INIT_ARGV0");
    println!("cargo:rerun-if-env-changed=HELIOS_BOOTFS_ROOT");
    println!("cargo:rerun-if-env-changed=HELIOS_DEBUGGER_WASM");
    println!("cargo:rerun-if-env-changed=HELIOS_DEBUGGER_MANIFEST");
    rerun_if_changed_recursive(Path::new("../programs"));
    rerun_if_changed_recursive(Path::new("../wit"));

    let target = env::var("HELIOS_BUILD_TARGET")
        .or_else(|_| env::var("TARGET"))
        .unwrap_or_else(|error| panic!("failed to determine Helios build target triple: {error}"));
    println!("cargo:rustc-env=HELIOS_BUILD_TARGET={target}");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is missing"));
    let init_destination = out_dir.join("embedded_init.rs");
    let init_source = generate_embedded_init(&out_dir);
    fs::write(init_destination, init_source).expect("failed to write embedded init description");

    let debugger_destination = out_dir.join("embedded_debugger.rs");
    let debugger_source = generate_embedded_debugger(&out_dir);
    fs::write(debugger_destination, debugger_source)
        .expect("failed to write embedded debugger description");
}

fn generate_embedded_init(out_dir: &Path) -> String {
    let component_path = resolve_init_component(out_dir);
    let argv0 = env::var("HELIOS_INIT_ARGV0").unwrap_or_else(|_| "/init.wasm".to_owned());
    let bootfs_root = resolve_bootfs_root(&component_path);
    assert!(
        bootfs_root.is_dir(),
        "HELIOS_BOOTFS_ROOT={} is not a directory",
        bootfs_root.display()
    );
    println!("cargo:rerun-if-changed={}", bootfs_root.display());

    let name = component_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_else(|| panic!("{} has no valid UTF-8 file name", component_path.display()));
    let bootfs = render_embedded_bootfs(&bootfs_root);

    format!(
        "pub const EMBEDDED_INIT: Option<EmbeddedInitDescriptor> = Some(EmbeddedInitDescriptor {{\n    component: EmbeddedComponent::new(\n        r#\"{name}\"#,\n        include_bytes!(r#\"{component}\"#),\n    ),\n    argv0: r#\"{argv0}\"#,\n    bootfs: {bootfs},\n}});\n",
        name = name,
        component = component_path.display(),
        argv0 = argv0,
        bootfs = bootfs,
    )
}

fn generate_embedded_debugger(out_dir: &Path) -> String {
    let component_path = resolve_debugger_component(out_dir);
    let name = component_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_else(|| panic!("{} has no valid UTF-8 file name", component_path.display()));

    format!(
        "pub const EMBEDDED_DEBUGGER: Option<EmbeddedDebuggerDescriptor> = Some(EmbeddedDebuggerDescriptor {{\n    component: EmbeddedComponent::new(\n        r#\"{name}\"#,\n        include_bytes!(r#\"{component}\"#),\n    ),\n}});\n",
        name = name,
        component = component_path.display(),
    )
}

fn resolve_init_component(out_dir: &Path) -> PathBuf {
    match env::var_os("HELIOS_INIT_WASM") {
        Some(path) => canonicalize_file(path.as_ref(), "HELIOS_INIT_WASM"),
        None => build_default_init_component(out_dir),
    }
}

fn resolve_debugger_component(out_dir: &Path) -> PathBuf {
    match env::var_os("HELIOS_DEBUGGER_WASM") {
        Some(path) => canonicalize_file(path.as_ref(), "HELIOS_DEBUGGER_WASM"),
        None => build_default_debugger_component(out_dir),
    }
}

fn resolve_bootfs_root(wasm_path: &Path) -> PathBuf {
    match env::var_os("HELIOS_BOOTFS_ROOT") {
        Some(path) => fs::canonicalize(path)
            .unwrap_or_else(|error| panic!("failed to resolve HELIOS_BOOTFS_ROOT: {error}")),
        None => {
            let default_root = Path::new("../programs/init/bootfs");
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
    let manifest_path = env::var_os("HELIOS_INIT_MANIFEST")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../programs/init/Cargo.toml"));
    assert!(
        manifest_path.is_file(),
        "default init crate manifest {} is missing",
        manifest_path.display()
    );

    let core_module_path = build_wasip2_program(
        out_dir,
        &manifest_path,
        "helios-init-target",
        "helios_init.wasm",
    );
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

fn build_default_debugger_component(out_dir: &Path) -> PathBuf {
    let manifest_path = env::var_os("HELIOS_DEBUGGER_MANIFEST")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../programs/debugger/Cargo.toml"));
    assert!(
        manifest_path.is_file(),
        "default debugger crate manifest {} is missing",
        manifest_path.display()
    );

    let core_module_path = build_wasip2_program(
        out_dir,
        &manifest_path,
        "helios-debugger-target",
        "helios_debugger.wasm",
    );
    let core_module_path = canonicalize_file(&core_module_path, "generated debugger core module");
    let component_path = out_dir.join("helios_debugger_component.wasm");
    let component = encode_component(&core_module_path);
    fs::write(&component_path, component).unwrap_or_else(|error| {
        panic!(
            "failed to write generated debugger component {}: {error}",
            component_path.display()
        )
    });
    canonicalize_file(&component_path, "generated debugger component")
}

fn build_wasip2_program(
    out_dir: &Path,
    manifest_path: &Path,
    target_dir_name: &str,
    artifact_name: &str,
) -> PathBuf {
    let cargo = env::var_os("CARGO").expect("CARGO is missing");
    let target_dir = out_dir.join(target_dir_name);
    let mut command = Command::new(cargo);
    command
        .arg("build")
        .arg("--manifest-path")
        .arg(manifest_path)
        .arg("--target")
        .arg("wasm32-wasip2")
        .arg("--target-dir")
        .arg(&target_dir);
    command.env("CARGO_PROFILE_DEV_OPT_LEVEL", "z");
    command.env("CARGO_PROFILE_DEV_DEBUG", "0");
    command.env("CARGO_PROFILE_DEV_CODEGEN_UNITS", "1");
    command.env("CARGO_PROFILE_DEV_PANIC", "abort");
    // `wasm32-wasip2` artifacts must not inherit the outer bare-metal linker
    // scripts from the kernel build. They also need their own wasm-specific
    // strip flags to keep embedded components small enough for guest-side JIT.
    command.env_remove("CARGO_ENCODED_RUSTFLAGS");
    command.env("RUSTFLAGS", "-C debuginfo=0 -C strip=debuginfo");
    let status = command.status().unwrap_or_else(|error| {
        panic!(
            "failed to invoke cargo for {} build: {error}",
            manifest_path.display()
        )
    });
    assert!(
        status.success(),
        "component build for {} failed with status {status}",
        manifest_path.display()
    );

    let profile = env::var("PROFILE").unwrap_or_else(|error| panic!("PROFILE is missing: {error}"));
    let profile_dir = match profile.as_str() {
        "debug" => "debug",
        "release" => "release",
        other => other,
    };
    target_dir
        .join("wasm32-wasip2")
        .join(profile_dir)
        .join(artifact_name)
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

fn encode_component(path: &Path) -> Vec<u8> {
    let wasm =
        fs::read(path).unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    if Parser::is_component(&wasm) {
        return wasm;
    }
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
