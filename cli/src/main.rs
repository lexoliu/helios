use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use ed25519_dalek::{SigningKey, VerifyingKey};
use helios_artifact::sign_payload_with_key;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use toml::Value;
use wasmparser::Parser as WasmParser;
use wasmtime::{Config, Engine, OptLevel, Strategy};
use wit_component::ComponentEncoder;

const ROOT_SECRET_FILE: &str = "helios-root-secret.key";
const ROOT_PUBLIC_FILE: &str = "helios-root-public.key";
const PREBUILD_MANIFEST_FILE: &str = "kernel-prebuild.json";
const DEFAULT_INIT_ARGV0: &str = "/init.wasm";
const COMPILER_PLUGIN_BOOTFS_PATH: &str = "bin/compiler.cwasm";
const COMPILER_PLUGIN_ROOT_KEY_ENV: &str = "HELIOS_COMPILER_ROOT_KEY_HEX";

#[derive(Parser)]
#[command(name = "helios-cli")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Aot(AotCommand),
    CompilerPlugin(CompilerPluginCommand),
    KernelPrebuild(KernelPrebuildCommand),
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Hint {
    Fast,
    Balanced,
    Performance,
}

#[derive(Parser)]
struct AotCommand {
    #[arg(long)]
    input: PathBuf,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    target: String,
    #[arg(long, default_value = "balanced")]
    hint: Hint,
    #[arg(long)]
    root_key: PathBuf,
}

#[derive(Parser)]
struct CompilerPluginCommand {
    #[arg(long)]
    target: String,
    #[arg(long, default_value = "balanced")]
    hint: Hint,
}

#[derive(Parser)]
struct KernelPrebuildCommand {
    #[arg(long)]
    out_dir: PathBuf,
    #[arg(long)]
    target: String,
    #[arg(long)]
    profile: String,
    #[arg(long)]
    cargo: PathBuf,
    #[arg(long, default_value = "programs/init/Cargo.toml")]
    init_manifest: PathBuf,
    #[arg(long, default_value = "programs/init/bootfs")]
    bootfs_root: PathBuf,
    #[arg(long, default_value = DEFAULT_INIT_ARGV0)]
    init_argv0: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PrebuildManifest {
    target: String,
    init_component: PathBuf,
    init_argv0: String,
    bootfs_root: PathBuf,
    root_public_key: PathBuf,
    root_secret_key: PathBuf,
    bootfs_assets: Vec<BootAsset>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BootAsset {
    path: String,
    source: PathBuf,
}

#[derive(Clone, Debug)]
struct ProgramManifest {
    command: String,
    bootfs_name: String,
    manifest_path: PathBuf,
    artifact_name: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Aot(command) => run_aot(command),
        Commands::CompilerPlugin(command) => run_compiler_plugin(command),
        Commands::KernelPrebuild(command) => run_kernel_prebuild(command),
    }
}

fn run_aot(command: AotCommand) -> Result<()> {
    let root_signing_key = read_signing_key(&command.root_key)?;
    let wasm = fs::read(&command.input)
        .with_context(|| format!("failed to read {}", command.input.display()))?;
    let component = componentize(&wasm).context("failed to componentize AOT input")?;
    let payload = precompile_component(&component, &command.target, command.hint)?;
    let signed =
        sign_payload_with_key(&payload, &root_signing_key).context("failed to sign AOT payload")?;
    fs::write(&command.output, signed)
        .with_context(|| format!("failed to write {}", command.output.display()))?;
    Ok(())
}

fn run_compiler_plugin(command: CompilerPluginCommand) -> Result<()> {
    let root_key_hex = std::env::var(COMPILER_PLUGIN_ROOT_KEY_ENV)
        .with_context(|| format!("{COMPILER_PLUGIN_ROOT_KEY_ENV} is required"))?;
    let root_key_bytes: [u8; 32] = hex::decode(root_key_hex.trim())
        .context("failed to decode compiler plugin root key")?
        .try_into()
        .map_err(|bytes: Vec<u8>| anyhow!("root key must be 32 bytes, got {}", bytes.len()))?;
    let root_signing_key = SigningKey::from_bytes(&root_key_bytes);

    let mut wasm = Vec::new();
    io::stdin()
        .read_to_end(&mut wasm)
        .context("failed to read wasm from stdin")?;
    let component = componentize(&wasm).context("failed to componentize compiler input")?;
    let payload = precompile_component(&component, &command.target, command.hint)?;
    let signed = sign_payload_with_key(&payload, &root_signing_key)
        .context("failed to sign compiler plugin output")?;
    io::stdout()
        .write_all(&signed)
        .context("failed to write signed cwasm to stdout")?;
    Ok(())
}

fn run_kernel_prebuild(command: KernelPrebuildCommand) -> Result<()> {
    fs::create_dir_all(&command.out_dir)
        .with_context(|| format!("failed to create {}", command.out_dir.display()))?;

    let root_secret_path = command.out_dir.join(ROOT_SECRET_FILE);
    let root_public_path = command.out_dir.join(ROOT_PUBLIC_FILE);
    let root_signing_key = ensure_root_keypair(&root_secret_path, &root_public_path)?;

    let init_manifest = workspace_path(&command.init_manifest);
    let bootfs_root = workspace_path(&command.bootfs_root);

    let init_component = build_component_program(
        &command.cargo,
        &command.profile,
        &command.out_dir,
        &init_manifest,
        "init-target",
        "helios_init.wasm",
    )?;
    let init_cwasm = command.out_dir.join("init_component.cwasm");
    let init_component_bytes = encode_component(&init_component)?;
    let init_payload =
        precompile_component(&init_component_bytes, &command.target, Hint::Balanced)?;
    let init_signed = sign_payload_with_key(&init_payload, &root_signing_key)
        .context("failed to sign init AOT payload")?;
    fs::write(&init_cwasm, init_signed)
        .with_context(|| format!("failed to write {}", init_cwasm.display()))?;

    let selected_programs = selected_boot_programs()?;
    let mut bootfs_assets = Vec::new();
    bootfs_assets.push(build_compiler_plugin_asset(
        &command.cargo,
        &command.profile,
        &command.out_dir,
        &command.target,
        &root_signing_key,
    )?);
    bootfs_assets.extend(build_boot_program_assets(
        &command.cargo,
        &command.profile,
        &command.out_dir,
        &command.target,
        &root_signing_key,
        &selected_programs,
    )?);

    let manifest = PrebuildManifest {
        target: command.target,
        init_component: init_cwasm,
        init_argv0: command.init_argv0,
        bootfs_root: fs::canonicalize(&bootfs_root)
            .with_context(|| format!("failed to resolve {}", bootfs_root.display()))?,
        root_public_key: root_public_path,
        root_secret_key: root_secret_path,
        bootfs_assets,
    };
    let manifest_path = command.out_dir.join(PREBUILD_MANIFEST_FILE);
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)
        .with_context(|| format!("failed to write {}", manifest_path.display()))?;
    Ok(())
}

fn build_compiler_plugin_asset(
    cargo: &Path,
    profile: &str,
    out_dir: &Path,
    target: &str,
    root_signing_key: &SigningKey,
) -> Result<BootAsset> {
    let wasm_path = build_component_program(
        cargo,
        profile,
        out_dir,
        &workspace_path(Path::new("cli/Cargo.toml")),
        "compiler-plugin-target",
        "helios-cli.wasm",
    )?;
    let component_bytes = encode_component(&wasm_path)?;
    let payload = precompile_component(&component_bytes, target, Hint::Balanced)?;
    let signed = sign_payload_with_key(&payload, root_signing_key)
        .context("failed to sign compiler plugin AOT payload")?;
    let output_path = out_dir.join("compiler_plugin.cwasm");
    fs::write(&output_path, signed)
        .with_context(|| format!("failed to write {}", output_path.display()))?;

    Ok(BootAsset {
        path: COMPILER_PLUGIN_BOOTFS_PATH.to_owned(),
        source: fs::canonicalize(&output_path)
            .with_context(|| format!("failed to resolve {}", output_path.display()))?,
    })
}

fn selected_boot_programs() -> Result<Option<BTreeSet<String>>> {
    let Some(raw) = std::env::var_os("HELIOS_BOOT_PROGRAMS") else {
        return Ok(None);
    };
    let selected = raw
        .to_string_lossy()
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    ensure!(
        !selected.is_empty(),
        "HELIOS_BOOT_PROGRAMS must name at least one boot program"
    );
    Ok(Some(selected))
}

fn build_boot_program_assets(
    cargo: &Path,
    profile: &str,
    out_dir: &Path,
    target: &str,
    root_signing_key: &SigningKey,
    selected_programs: &Option<BTreeSet<String>>,
) -> Result<Vec<BootAsset>> {
    let programs_root = workspace_path(Path::new("programs"));
    let mut available_programs = BTreeSet::new();
    let mut manifests = Vec::new();

    for entry in fs::read_dir(&programs_root)
        .with_context(|| format!("failed to read {}", programs_root.display()))?
    {
        let entry = entry.with_context(|| "failed to read programs directory entry")?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(command) = path.file_name().and_then(|name| name.to_str()) else {
            bail!(
                "program directory {} has no valid UTF-8 name",
                path.display()
            );
        };
        if command == "init" {
            continue;
        }
        available_programs.insert(command.to_owned());
        if selected_programs
            .as_ref()
            .is_some_and(|selected| !selected.contains(command))
        {
            continue;
        }
        manifests.push(read_program_manifest(command, &path.join("Cargo.toml"))?);
    }

    if let Some(selected_programs) = selected_programs {
        let missing_programs = selected_programs
            .iter()
            .filter(|command| !available_programs.contains(*command))
            .cloned()
            .collect::<Vec<_>>();
        ensure!(
            missing_programs.is_empty(),
            "HELIOS_BOOT_PROGRAMS referenced unknown program(s): {}",
            missing_programs.join(", ")
        );
    }

    manifests.sort_by(|left, right| left.command.cmp(&right.command));
    manifests
        .into_iter()
        .map(|manifest| {
            build_boot_program_asset(cargo, profile, out_dir, target, root_signing_key, manifest)
        })
        .collect()
}

fn workspace_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or_else(|| panic!("cli manifest directory must have a workspace parent"))
        .join(path)
}

fn build_boot_program_asset(
    cargo: &Path,
    profile: &str,
    out_dir: &Path,
    target: &str,
    root_signing_key: &SigningKey,
    manifest: ProgramManifest,
) -> Result<BootAsset> {
    let wasm_path = build_component_program(
        cargo,
        profile,
        out_dir,
        &manifest.manifest_path,
        &format!("bootfs-{}-target", manifest.command),
        &manifest.artifact_name,
    )?;
    let component_bytes = encode_component(&wasm_path)?;
    let payload = precompile_component(&component_bytes, target, Hint::Balanced)?;
    let signed = sign_payload_with_key(&payload, root_signing_key)
        .context("failed to sign bootfs AOT payload")?;
    let output_path = out_dir.join(format!("{}_bootfs_component.cwasm", manifest.command));
    fs::write(&output_path, signed)
        .with_context(|| format!("failed to write {}", output_path.display()))?;

    Ok(BootAsset {
        path: format!("bin/{}", manifest.bootfs_name),
        source: fs::canonicalize(&output_path)
            .with_context(|| format!("failed to resolve {}", output_path.display()))?,
    })
}

fn read_program_manifest(command: &str, manifest_path: &Path) -> Result<ProgramManifest> {
    ensure!(
        manifest_path.is_file(),
        "default program crate manifest {} is missing",
        manifest_path.display()
    );
    let manifest = fs::read_to_string(manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest = manifest
        .parse::<Value>()
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
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
        .with_context(|| {
            format!(
                "{} is missing package.name or lib.name",
                manifest_path.display()
            )
        })?;
    Ok(ProgramManifest {
        command: command.to_owned(),
        bootfs_name: match command {
            "sh" => "dash".to_owned(),
            other => other.to_owned(),
        },
        manifest_path: manifest_path.to_path_buf(),
        artifact_name: format!("{artifact_stem}.wasm"),
    })
}

fn build_component_program(
    cargo: &Path,
    profile: &str,
    out_dir: &Path,
    manifest_path: &Path,
    target_dir_name: &str,
    artifact_name: &str,
) -> Result<PathBuf> {
    ensure!(
        manifest_path.is_file(),
        "crate manifest {} is missing",
        manifest_path.display()
    );
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
    command.env_remove("CARGO_ENCODED_RUSTFLAGS");
    command.env("RUSTFLAGS", "-C debuginfo=0 -C strip=debuginfo");
    let status = command
        .status()
        .with_context(|| format!("failed to invoke cargo for {}", manifest_path.display()))?;
    ensure!(
        status.success(),
        "component build for {} failed with status {}",
        manifest_path.display(),
        status
    );

    let profile_dir = if profile == "release" {
        "release"
    } else {
        "debug"
    };
    fs::canonicalize(
        target_dir
            .join("wasm32-wasip2")
            .join(profile_dir)
            .join(artifact_name),
    )
    .with_context(|| format!("failed to resolve generated artifact {}", artifact_name))
}

fn encode_component(path: &Path) -> Result<Vec<u8>> {
    let wasm = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    if WasmParser::is_component(&wasm) {
        return Ok(wasm);
    }
    ComponentEncoder::default()
        .module(&wasm)
        .with_context(|| format!("failed to load core module {}", path.display()))?
        .validate(true)
        .encode()
        .with_context(|| format!("failed to encode component {}", path.display()))
}

fn componentize(wasm: &[u8]) -> Result<Vec<u8>> {
    if WasmParser::is_component(wasm) {
        return Ok(wasm.to_vec());
    }
    ComponentEncoder::default()
        .module(wasm)
        .context("failed to load core module")?
        .validate(true)
        .encode()
        .context("failed to encode component")
}

fn precompile_component(bytes: &[u8], target: &str, hint: Hint) -> Result<Vec<u8>> {
    let engine = Engine::new(&build_engine_config(target, hint)?).map_err(|error| {
        anyhow!("failed to create Wasmtime engine for target {target}: {error:#}")
    })?;
    engine
        .precompile_component(bytes)
        .map_err(|error| anyhow!("failed to precompile component: {error:#}"))
}

fn build_engine_config(target: &str, hint: Hint) -> Result<Config> {
    let mut config = Config::new();
    config
        .target(target)
        .map_err(|error| anyhow!("Wasmtime rejected target {target}: {error:#}"))?;
    match hint {
        Hint::Fast => {
            config.strategy(Strategy::Winch);
        }
        Hint::Balanced => {
            config.strategy(Strategy::Cranelift);
            config.cranelift_opt_level(OptLevel::None);
        }
        Hint::Performance => {
            config.strategy(Strategy::Cranelift);
            config.cranelift_opt_level(OptLevel::Speed);
        }
    }
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    config.concurrency_support(true);
    Ok(config)
}

fn ensure_root_keypair(root_secret_path: &Path, root_public_path: &Path) -> Result<SigningKey> {
    if root_secret_path.is_file() {
        let signing_key = read_signing_key(root_secret_path)?;
        fs::write(
            root_public_path,
            VerifyingKey::from(&signing_key).to_bytes(),
        )
        .with_context(|| format!("failed to write {}", root_public_path.display()))?;
        return Ok(signing_key);
    }

    let signing_key = SigningKey::generate(&mut OsRng);
    fs::write(root_secret_path, signing_key.to_bytes())
        .with_context(|| format!("failed to write {}", root_secret_path.display()))?;
    fs::write(
        root_public_path,
        VerifyingKey::from(&signing_key).to_bytes(),
    )
    .with_context(|| format!("failed to write {}", root_public_path.display()))?;
    Ok(signing_key)
}

fn read_signing_key(path: &Path) -> Result<SigningKey> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let secret_bytes: [u8; 32] = bytes.as_slice().try_into().with_context(|| {
        format!(
            "{} does not contain a 32-byte Ed25519 secret key",
            path.display()
        )
    })?;
    Ok(SigningKey::from_bytes(&secret_bytes))
}
