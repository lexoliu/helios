use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail, ensure};
use askama::Template;
use clap::{Parser, Subcommand, ValueEnum};
use ed25519_dalek::{SigningKey, VerifyingKey};
use fatfs::{FatType, FileSystem, FormatVolumeOptions, FsOptions};
use helios_artifact::{
    CWASM_NO_VMEM_MEMORY_GUARD_SIZE, CWASM_NO_VMEM_MEMORY_RESERVATION,
    CWASM_NO_VMEM_MEMORY_RESERVATION_FOR_GROWTH, sign_payload_with_key,
};
use mbrman::{BOOT_ACTIVE, CHS, MBR, MBRPartitionEntry};
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
const LIMINE_IMAGE_BYTES: u64 = 768 * 1024 * 1024;
const LIMINE_PARTITION_START_LBA: u32 = 2048;
const LIMINE_SECTOR_BYTES: u64 = 512;
const LIMINE_EFI_SYSTEM_PARTITION_TYPE: u8 = 0xef;
const LIMINE_DISK_SIGNATURE: [u8; 4] = *b"HELO";

#[derive(Template)]
#[template(path = "limine.conf", escape = "none")]
struct LimineConfigTemplate {
    baud: u32,
}

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
    LimineUefiImage(LimineUefiImageCommand),
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

#[derive(Parser)]
struct LimineUefiImageCommand {
    #[arg(long)]
    kernel: PathBuf,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    baud: u32,
    #[arg(long, value_enum)]
    efi_arch: LimineEfiArch,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LimineEfiArch {
    X86_64,
    Aarch64,
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
        Commands::LimineUefiImage(command) => run_limine_uefi_image(command),
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
        init_component: fs::canonicalize(&init_cwasm)
            .with_context(|| format!("failed to resolve {}", init_cwasm.display()))?,
        init_argv0: command.init_argv0,
        bootfs_root: fs::canonicalize(&bootfs_root)
            .with_context(|| format!("failed to resolve {}", bootfs_root.display()))?,
        root_public_key: fs::canonicalize(&root_public_path)
            .with_context(|| format!("failed to resolve {}", root_public_path.display()))?,
        root_secret_key: fs::canonicalize(&root_secret_path)
            .with_context(|| format!("failed to resolve {}", root_secret_path.display()))?,
        bootfs_assets,
    };
    let manifest_path = command.out_dir.join(PREBUILD_MANIFEST_FILE);
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)
        .with_context(|| format!("failed to write {}", manifest_path.display()))?;
    Ok(())
}

fn run_limine_uefi_image(command: LimineUefiImageCommand) -> Result<()> {
    let kernel = fs::canonicalize(&command.kernel)
        .with_context(|| format!("failed to canonicalize kernel {}", command.kernel.display()))?;
    let limine = LimineToolchain::discover(command.efi_arch)?;
    build_limine_uefi_image(&limine, &kernel, &command.output, command.baud)
}

struct LimineToolchain {
    efi_bootloader: PathBuf,
    efi_bootloader_name: &'static str,
}

impl LimineToolchain {
    fn discover(efi_arch: LimineEfiArch) -> Result<Self> {
        let executable = std::env::var_os("HELIOS_LIMINE_BIN")
            .map(PathBuf::from)
            .or_else(|| find_executable_in_path("limine"))
            .ok_or_else(|| {
                anyhow!("failed to find Limine; install `limine` or set HELIOS_LIMINE_BIN")
            })?;
        let share_dir = std::env::var_os("HELIOS_LIMINE_SHARE")
            .map(PathBuf::from)
            .or_else(|| limine_datadir(&executable).ok())
            .or_else(|| infer_limine_share_dir(&executable, efi_arch))
            .ok_or_else(|| {
                anyhow!("failed to locate Limine shared files; set HELIOS_LIMINE_SHARE")
            })?;
        let efi_bootloader_name = match efi_arch {
            LimineEfiArch::X86_64 => "BOOTX64.EFI",
            LimineEfiArch::Aarch64 => "BOOTAA64.EFI",
        };
        let efi_bootloader = share_dir.join(efi_bootloader_name);
        ensure!(
            efi_bootloader.is_file(),
            "Limine EFI bootloader is missing: {}",
            efi_bootloader.display()
        );
        Ok(Self {
            efi_bootloader,
            efi_bootloader_name,
        })
    }
}

fn limine_datadir(executable: &Path) -> Result<PathBuf> {
    let output = Command::new(executable)
        .arg("--print-datadir")
        .output()
        .with_context(|| {
            format!(
                "failed to query Limine data directory via {}",
                executable.display()
            )
        })?;
    ensure!(
        output.status.success(),
        "{} --print-datadir exited with status {}",
        executable.display(),
        output.status
    );
    let path = String::from_utf8(output.stdout).context("Limine data directory was not UTF-8")?;
    Ok(PathBuf::from(path.trim()))
}

fn build_limine_uefi_image(
    limine: &LimineToolchain,
    kernel: &Path,
    image: &Path,
    baud: u32,
) -> Result<()> {
    let image_bytes = limine_image_bytes(kernel, &limine.efi_bootloader)?;
    if let Some(parent) = image.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create image directory {}", parent.display()))?;
    }
    let mut image_file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(image)
        .with_context(|| format!("failed to create Limine image {}", image.display()))?;
    image_file
        .set_len(image_bytes)
        .with_context(|| format!("failed to size Limine image {}", image.display()))?;
    write_limine_mbr(&mut image_file, image_bytes)?;
    write_limine_fat_volume(&mut image_file, image_bytes, kernel, limine, baud)
}

fn limine_image_bytes(kernel: &Path, efi_bootloader: &Path) -> Result<u64> {
    let payload_bytes = fs::metadata(kernel)
        .with_context(|| format!("failed to inspect kernel {}", kernel.display()))?
        .len()
        + fs::metadata(efi_bootloader)
            .with_context(|| format!("failed to inspect {}", efi_bootloader.display()))?
            .len();
    Ok(LIMINE_IMAGE_BYTES.max(payload_bytes + 128 * 1024 * 1024))
}

fn write_limine_mbr(image: &mut fs::File, image_bytes: u64) -> Result<()> {
    let mut mbr = MBR::new_from(image, LIMINE_SECTOR_BYTES as u32, LIMINE_DISK_SIGNATURE)
        .context("failed to create Limine image MBR")?;
    let total_sectors = u32::try_from(image_bytes / LIMINE_SECTOR_BYTES)
        .context("Limine UEFI image is too large for an MBR partition table")?;
    let partition_sectors = total_sectors
        .checked_sub(LIMINE_PARTITION_START_LBA)
        .ok_or_else(|| anyhow!("Limine UEFI image is too small for the FAT partition"))?;
    mbr[1] = MBRPartitionEntry {
        boot: BOOT_ACTIVE,
        first_chs: CHS::empty(),
        sys: LIMINE_EFI_SYSTEM_PARTITION_TYPE,
        last_chs: CHS::empty(),
        starting_lba: LIMINE_PARTITION_START_LBA,
        sectors: partition_sectors,
    };
    mbr.write_into(image)
        .context("failed to write Limine image MBR")
}

fn write_limine_fat_volume(
    image: &mut fs::File,
    image_bytes: u64,
    kernel: &Path,
    limine: &LimineToolchain,
    baud: u32,
) -> Result<()> {
    let partition_offset = u64::from(LIMINE_PARTITION_START_LBA) * LIMINE_SECTOR_BYTES;
    let partition_len = image_bytes
        .checked_sub(partition_offset)
        .ok_or_else(|| anyhow!("Limine partition offset exceeds image size"))?;
    let mut partition = FileSlice::new(image, partition_offset, partition_len);
    fatfs::format_volume(
        &mut partition,
        FormatVolumeOptions::new()
            .fat_type(FatType::Fat32)
            .volume_label(*b"HELIOS     "),
    )
    .context("failed to format Limine FAT32 partition")?;
    write_fat32_hidden_sectors(&mut partition, LIMINE_PARTITION_START_LBA)?;
    partition
        .seek(SeekFrom::Start(0))
        .context("failed to rewind Limine FAT32 partition")?;
    let fs = FileSystem::new(partition, FsOptions::new())
        .context("failed to open Limine FAT32 partition")?;
    let root = fs.root_dir();
    let boot = root.create_dir("boot").context("failed to create /boot")?;
    let efi = root.create_dir("EFI").context("failed to create /EFI")?;
    let efi_boot = efi
        .create_dir("BOOT")
        .context("failed to create /EFI/BOOT")?;
    let limine_dir = boot
        .create_dir("limine")
        .context("failed to create /boot/limine")?;
    write_file_to_fat(
        &efi_boot,
        limine.efi_bootloader_name,
        &limine.efi_bootloader,
    )?;
    let config = LimineConfigTemplate { baud }
        .render()
        .context("failed to render Limine configuration")?;
    let mut limine_conf = limine_dir
        .create_file("limine.conf")
        .context("failed to create /boot/limine/limine.conf")?;
    limine_conf
        .write_all(config.as_bytes())
        .context("failed to write /boot/limine/limine.conf")?;
    let mut efi_limine_conf = efi_boot
        .create_file("limine.conf")
        .context("failed to create /EFI/BOOT/limine.conf")?;
    efi_limine_conf
        .write_all(config.as_bytes())
        .context("failed to write /EFI/BOOT/limine.conf")?;
    write_file_to_fat(&boot, "helios", kernel)?;
    Ok(())
}

fn write_fat32_hidden_sectors(partition: &mut FileSlice<'_>, hidden_sectors: u32) -> Result<()> {
    const BPB_HIDDEN_SECTORS_OFFSET: u64 = 0x1c;
    const FAT32_BACKUP_BOOT_SECTOR: u64 = 6 * LIMINE_SECTOR_BYTES;
    for boot_sector in [0, FAT32_BACKUP_BOOT_SECTOR] {
        partition
            .seek(SeekFrom::Start(boot_sector + BPB_HIDDEN_SECTORS_OFFSET))
            .context("failed to seek to FAT32 hidden-sectors field")?;
        partition
            .write_all(&hidden_sectors.to_le_bytes())
            .context("failed to write FAT32 hidden-sectors field")?;
    }
    Ok(())
}

fn write_file_to_fat<IO>(root: &fatfs::Dir<'_, IO>, name: &str, source: &Path) -> Result<()>
where
    IO: fatfs::ReadWriteSeek,
{
    let mut input = fs::File::open(source)
        .with_context(|| format!("failed to open source file {}", source.display()))?;
    let mut output = root
        .create_file(name)
        .with_context(|| format!("failed to create FAT file {name}"))?;
    std::io::copy(&mut input, &mut output)
        .with_context(|| format!("failed to copy {} into FAT file {name}", source.display()))?;
    Ok(())
}

fn find_executable_in_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|path| path.join(name))
        .find(|candidate| candidate.is_file())
}

fn infer_limine_share_dir(executable: &Path, efi_arch: LimineEfiArch) -> Option<PathBuf> {
    let bin_dir = executable.parent()?;
    let prefix_dir = bin_dir.parent()?;
    let efi_bootloader_name = match efi_arch {
        LimineEfiArch::X86_64 => "BOOTX64.EFI",
        LimineEfiArch::Aarch64 => "BOOTAA64.EFI",
    };
    [
        prefix_dir.join("share").join("limine"),
        prefix_dir.join("share"),
    ]
    .into_iter()
    .find(|path| path.join(efi_bootloader_name).is_file())
}

struct FileSlice<'a> {
    file: &'a mut fs::File,
    offset: u64,
    len: u64,
    position: u64,
}

impl<'a> FileSlice<'a> {
    fn new(file: &'a mut fs::File, offset: u64, len: u64) -> Self {
        Self {
            file,
            offset,
            len,
            position: 0,
        }
    }
}

impl Read for FileSlice<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.position >= self.len {
            return Ok(0);
        }
        let remaining = self.len - self.position;
        let count = remaining.min(buf.len() as u64) as usize;
        self.file.seek(SeekFrom::Start(
            self.offset
                .checked_add(self.position)
                .ok_or_else(|| std::io::Error::other("file slice position overflow"))?,
        ))?;
        let read = self.file.read(&mut buf[..count])?;
        self.position += read as u64;
        Ok(read)
    }
}

impl Write for FileSlice<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.position >= self.len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "file slice write exceeds partition bounds",
            ));
        }
        let remaining = self.len - self.position;
        let count = remaining.min(buf.len() as u64) as usize;
        self.file.seek(SeekFrom::Start(
            self.offset
                .checked_add(self.position)
                .ok_or_else(|| std::io::Error::other("file slice position overflow"))?,
        ))?;
        let written = self.file.write(&buf[..count])?;
        self.position += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl Seek for FileSlice<'_> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let next = match pos {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::End(delta) => i128::from(self.len) + i128::from(delta),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
        };
        if !(0..=i128::from(self.len)).contains(&next) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "file slice seek is outside partition bounds",
            ));
        }
        self.position = next as u64;
        Ok(self.position)
    }
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
    configure_target_isa_flags(&mut config, target);
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
    config.wasm_simd(true);
    config.wasm_relaxed_simd(true);
    config.relaxed_simd_deterministic(false);
    config.wasm_multi_memory(true);
    config.wasm_memory64(true);
    config.wasm_tail_call(true);
    config.gc_support(true);
    config.wasm_reference_types(true);
    config.wasm_function_references(true);
    config.concurrency_support(true);
    config.epoch_interruption(true);
    config.max_wasm_stack(8 * 1024 * 1024);
    if target_requires_no_vmem_cwasm(target) {
        config.signals_based_traps(true);
        config.memory_guard_size(CWASM_NO_VMEM_MEMORY_GUARD_SIZE);
        config.memory_reservation(CWASM_NO_VMEM_MEMORY_RESERVATION);
        config.memory_reservation_for_growth(CWASM_NO_VMEM_MEMORY_RESERVATION_FOR_GROWTH);
        config.memory_init_cow(false);
    }
    Ok(config)
}

fn target_requires_no_vmem_cwasm(target: &str) -> bool {
    target.contains("-unknown-none")
}

fn configure_target_isa_flags(config: &mut Config, target: &str) {
    if !target.starts_with("x86_64-") {
        return;
    }

    // TODO(x86-avx): add AVX/FMA/AVX512 once the x86 kernel enables OSXSAVE,
    // configures XCR0, and preserves the full XSAVE state across execution.
    for flag in [
        "has_cmpxchg16b",
        "has_sse3",
        "has_ssse3",
        "has_sse41",
        "has_sse42",
        "has_popcnt",
        "has_bmi1",
        "has_bmi2",
        "has_lzcnt",
    ] {
        unsafe {
            config.cranelift_flag_enable(flag);
        }
    }
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
