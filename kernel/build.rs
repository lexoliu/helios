use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use toml::Value;
use walkdir::WalkDir;
use wasmparser::Parser;
use wit_component::ComponentEncoder;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(helios_watchdog_self_test)");
    println!("cargo:rerun-if-env-changed=HELIOS_BUILD_TARGET");
    println!("cargo:rerun-if-env-changed=HELIOS_INIT_WASM");
    println!("cargo:rerun-if-env-changed=HELIOS_INIT_MANIFEST");
    println!("cargo:rerun-if-env-changed=HELIOS_INIT_ARGV0");
    println!("cargo:rerun-if-env-changed=HELIOS_BOOTFS_ROOT");
    println!("cargo:rerun-if-env-changed=HELIOS_BOOT_PROGRAMS");
    println!("cargo:rerun-if-env-changed=HELIOS_WATCHDOG_SELF_TEST");
    println!("cargo:rerun-if-env-changed=HELIOS_WATCHDOG_SELF_TEST_DELAY_MS");
    rerun_if_changed_recursive(Path::new("../programs"));
    rerun_if_changed_recursive(Path::new("../api"));
    rerun_if_changed_recursive(Path::new("../api-macro"));
    rerun_if_changed_recursive(Path::new("../inspector-protocol"));
    rerun_if_changed_recursive(Path::new("../wit"));

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
    let init_source = generate_embedded_init(&out_dir);
    fs::write(init_destination, init_source).expect("failed to write embedded init description");
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
    let bootfs = render_embedded_bootfs(&bootfs_root, &build_boot_program_assets(out_dir));

    format!(
        "pub const EMBEDDED_INIT: Option<EmbeddedInitDescriptor> = Some(EmbeddedInitDescriptor {{\n    component: EmbeddedComponent::new(\n        r#\"{name}\"#,\n        include_bytes!(r#\"{component}\"#),\n    ),\n    argv0: r#\"{argv0}\"#,\n    bootfs: {bootfs},\n}});\n",
        name = name,
        component = component_path.display(),
        argv0 = argv0,
        bootfs = bootfs,
    )
}

struct EmbeddedBootAsset {
    path: String,
    source: PathBuf,
}

struct ProgramManifest {
    command: String,
    bootfs_name: String,
    manifest_path: PathBuf,
    artifact_name: String,
}

fn build_boot_program_assets(out_dir: &Path) -> Vec<EmbeddedBootAsset> {
    let programs_root = Path::new("../programs");
    let selected_programs = selected_boot_programs();
    let mut available_programs = BTreeSet::new();
    let mut manifests = fs::read_dir(programs_root)
        .unwrap_or_else(|error| {
            panic!(
                "failed to read programs directory {}: {error}",
                programs_root.display()
            )
        })
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter_map(|path| {
            let command = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_else(|| {
                    panic!(
                        "program directory {} has no valid UTF-8 name",
                        path.display()
                    )
                });
            if command == "init" {
                return None;
            }
            available_programs.insert(command.to_owned());
            if selected_programs
                .as_ref()
                .is_some_and(|selected| !selected.contains(command))
            {
                return None;
            }
            Some(read_program_manifest(command, &path.join("Cargo.toml")))
        })
        .collect::<Vec<_>>();
    if let Some(selected_programs) = &selected_programs {
        let missing_programs = selected_programs
            .iter()
            .filter(|command| !available_programs.contains(*command))
            .cloned()
            .collect::<Vec<_>>();
        assert!(
            missing_programs.is_empty(),
            "HELIOS_BOOT_PROGRAMS referenced unknown program(s): {}",
            missing_programs.join(", ")
        );
    }
    manifests.sort_by(|left, right| left.command.cmp(&right.command));

    manifests
        .into_iter()
        .map(|manifest| build_boot_program_asset(out_dir, manifest))
        .collect()
}

fn selected_boot_programs() -> Option<BTreeSet<String>> {
    let raw = env::var("HELIOS_BOOT_PROGRAMS").ok()?;
    let selected = raw
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    assert!(
        !selected.is_empty(),
        "HELIOS_BOOT_PROGRAMS must name at least one boot program"
    );
    Some(selected)
}

fn build_boot_program_asset(out_dir: &Path, manifest: ProgramManifest) -> EmbeddedBootAsset {
    let core_module_path = build_wasip2_program(
        out_dir,
        &manifest.manifest_path,
        &format!("helios-bootfs-{}-target", manifest.command),
        &manifest.artifact_name,
    );
    let core_module_path = canonicalize_file(&core_module_path, "generated bootfs core module");
    let component_path = out_dir.join(format!("{}_bootfs_component.wasm", manifest.command));
    let component = encode_component(&core_module_path);
    fs::write(&component_path, component).unwrap_or_else(|error| {
        panic!(
            "failed to write generated bootfs component {}: {error}",
            component_path.display()
        )
    });

    EmbeddedBootAsset {
        path: format!("bin/{}", manifest.bootfs_name),
        source: canonicalize_file(&component_path, "generated bootfs component"),
    }
}

fn read_program_manifest(command: &str, manifest_path: &Path) -> ProgramManifest {
    assert!(
        manifest_path.is_file(),
        "default program crate manifest {} is missing",
        manifest_path.display()
    );
    println!("cargo:rerun-if-changed={}", manifest_path.display());

    let manifest = fs::read_to_string(manifest_path).unwrap_or_else(|error| {
        panic!(
            "failed to read program manifest {}: {error}",
            manifest_path.display()
        )
    });
    let manifest = manifest.parse::<Value>().unwrap_or_else(|error| {
        panic!(
            "failed to parse program manifest {}: {error}",
            manifest_path.display()
        )
    });
    let artifact_stem = manifest
        .get("lib")
        .and_then(|lib| lib.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            manifest
                .get("package")
                .and_then(|package| package.get("name"))
                .and_then(Value::as_str)
                .map(|name| name.replace('-', "_"))
        })
        .unwrap_or_else(|| {
            panic!(
                "program manifest {} is missing package.name or lib.name",
                manifest_path.display()
            )
        });

    ProgramManifest {
        command: command.to_owned(),
        bootfs_name: match command {
            "sh" => "dash".to_owned(),
            other => other.to_owned(),
        },
        manifest_path: manifest_path.to_path_buf(),
        artifact_name: format!("{artifact_stem}.wasm"),
    }
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

fn build_wasip2_program(
    out_dir: &Path,
    manifest_path: &Path,
    target_dir_name: &str,
    artifact_name: &str,
) -> PathBuf {
    let cargo = env::var_os("CARGO").expect("CARGO is missing");
    let profile = env::var("PROFILE").unwrap_or_else(|error| panic!("PROFILE is missing: {error}"));
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
    if profile == "release" {
        command.arg("--release");
    } else {
        command.env("CARGO_PROFILE_DEV_OPT_LEVEL", "z");
        command.env("CARGO_PROFILE_DEV_DEBUG", "0");
        command.env("CARGO_PROFILE_DEV_CODEGEN_UNITS", "1");
        command.env("CARGO_PROFILE_DEV_PANIC", "abort");
    }
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

    let profile_dir = profile.as_str();
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

fn render_embedded_bootfs(root: &Path, extra_assets: &[EmbeddedBootAsset]) -> String {
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

    for asset in extra_assets {
        entries.push(format!(
            "EmbeddedBootFile::new(r#\"{path}\"#, include_bytes!(r#\"{source}\"#))",
            path = asset.path,
            source = asset.source.display(),
        ));
    }

    entries.sort();

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
