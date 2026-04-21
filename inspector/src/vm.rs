use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use bootloader::BiosBoot;
use clap::{Args as ClapArgs, Subcommand, ValueEnum};
use console::style;
use directories::ProjectDirs;
use helios_hal::fs::HOST_SHARE_MOUNT_TAG;
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

use crate::{connect_client, run_connected, SessionCommand};

const DEFAULT_BAUD: u32 = 115_200;
const DEFAULT_RISCV_QEMU_BIN: &str = "qemu-system-riscv64";
const DEFAULT_X86_QEMU_BIN: &str = "qemu-system-x86_64";
const DEFAULT_MEMORY: &str = "512M";
const DEFAULT_RISCV_SMP: u16 = 4;
const DEFAULT_X86_SMP: u16 = 2;
const DEFAULT_SOCKET_WAIT: Duration = Duration::from_secs(10);
const DEFAULT_BLOCK_DEVICE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum VmArch {
    Riscv64,
    X86_64,
}

impl VmArch {
    fn profile(self) -> &'static VmProfile {
        match self {
            Self::Riscv64 => &RISCV64_VM_PROFILE,
            Self::X86_64 => &X86_64_VM_PROFILE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VmBootArtifactKind {
    KernelBinary,
    BiosDiskImage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VmConsoleProfile {
    SerialUnixSocket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VmNetworkProfile {
    VirtioMmioUser,
    VirtioPciUser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VmBlockProfile {
    VirtioMmioDataDisk,
    VirtioPciBootDisk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VmHostShareProfile {
    Virtio9pMmio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VmWatchdogProfile {
    I6300Esb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VmProfile {
    arch: VmArch,
    qemu_bin: &'static str,
    cargo_target: &'static str,
    machine: &'static str,
    kernel_artifact_name: &'static str,
    default_smp: u16,
    default_memory: &'static str,
    default_bios: Option<&'static str>,
    boot_artifact: VmBootArtifactKind,
    console: VmConsoleProfile,
    network: Option<VmNetworkProfile>,
    block: Option<VmBlockProfile>,
    host_share: Option<VmHostShareProfile>,
    watchdog: Option<VmWatchdogProfile>,
}

const RISCV64_VM_PROFILE: VmProfile = VmProfile {
    arch: VmArch::Riscv64,
    qemu_bin: DEFAULT_RISCV_QEMU_BIN,
    cargo_target: "riscv64gc-unknown-none-elf",
    machine: "virt",
    kernel_artifact_name: "helios",
    default_smp: DEFAULT_RISCV_SMP,
    default_memory: DEFAULT_MEMORY,
    default_bios: Some("default"),
    boot_artifact: VmBootArtifactKind::KernelBinary,
    console: VmConsoleProfile::SerialUnixSocket,
    network: Some(VmNetworkProfile::VirtioMmioUser),
    block: Some(VmBlockProfile::VirtioMmioDataDisk),
    host_share: Some(VmHostShareProfile::Virtio9pMmio),
    watchdog: None,
};

const X86_64_VM_PROFILE: VmProfile = VmProfile {
    arch: VmArch::X86_64,
    qemu_bin: DEFAULT_X86_QEMU_BIN,
    cargo_target: "x86_64-unknown-none",
    machine: "q35",
    kernel_artifact_name: "helios",
    default_smp: DEFAULT_X86_SMP,
    default_memory: DEFAULT_MEMORY,
    default_bios: None,
    boot_artifact: VmBootArtifactKind::BiosDiskImage,
    console: VmConsoleProfile::SerialUnixSocket,
    network: Some(VmNetworkProfile::VirtioPciUser),
    block: Some(VmBlockProfile::VirtioPciBootDisk),
    host_share: None,
    watchdog: Some(VmWatchdogProfile::I6300Esb),
};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct VmConfigFile {
    #[serde(default)]
    pub(crate) arch: Option<VmArch>,
    #[serde(default)]
    pub(crate) qemu_bin: Option<PathBuf>,
    #[serde(default)]
    pub(crate) kernel: Option<PathBuf>,
    #[serde(default)]
    pub(crate) smp: Option<u16>,
    #[serde(default)]
    pub(crate) memory: Option<String>,
    #[serde(default)]
    pub(crate) bios: Option<String>,
    #[serde(default)]
    pub(crate) baud: Option<u32>,
    #[serde(default)]
    pub(crate) shared_dir: Option<PathBuf>,
}

#[derive(Debug, ClapArgs)]
pub(crate) struct VmCommand {
    #[arg(long, value_enum, default_value_t = VmArch::Riscv64)]
    arch: VmArch,

    #[arg(long)]
    config: Option<PathBuf>,

    #[arg(long)]
    qemu_bin: Option<PathBuf>,

    #[arg(long)]
    kernel: Option<PathBuf>,

    #[arg(long)]
    socket: Option<PathBuf>,

    #[arg(long, default_value_t = false)]
    no_build: bool,

    #[arg(long)]
    smp: Option<u16>,

    #[arg(long, default_value = DEFAULT_MEMORY)]
    memory: String,

    #[arg(long)]
    bios: Option<String>,

    #[arg(long, default_value_t = DEFAULT_BAUD)]
    baud: u32,

    #[arg(long)]
    shared_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<VmSessionCommand>,
}

#[derive(Debug, Subcommand)]
enum VmSessionCommand {
    Dash(crate::DashCommand),
    Tracing(crate::TracingCommand),
    Stats,
    Repl,
}

#[derive(Debug)]
struct ResolvedVmCommand {
    profile: &'static VmProfile,
    qemu_bin: PathBuf,
    kernel: PathBuf,
    socket: Option<PathBuf>,
    no_build: bool,
    smp: u16,
    memory: String,
    bios: Option<String>,
    baud: u32,
    shared_dir: Option<PathBuf>,
    command: Option<SessionCommand>,
}

pub(crate) fn run(command: VmCommand) -> Result<()> {
    let command = resolve(command)?;
    ensure_qemu_command(&command)?;
    if !command.no_build {
        build_vm(&command)?;
    }
    let mut runtime = VmRuntime::spawn(&command)?;
    let result = connect_and_run(&command, runtime.socket_path());
    runtime.shutdown();
    result
}

fn resolve(command: VmCommand) -> Result<ResolvedVmCommand> {
    let file = load_config_file(command.config.as_deref())?;
    let arch = file.arch.unwrap_or(command.arch);
    let profile = arch.profile();
    let qemu_bin = command
        .qemu_bin
        .or(file.qemu_bin)
        .unwrap_or_else(|| PathBuf::from(profile.qemu_bin));
    let kernel = command
        .kernel
        .or(file.kernel)
        .unwrap_or_else(|| default_kernel_path(arch));
    let smp = command
        .smp
        .or(file.smp)
        .unwrap_or(profile.default_smp);
    let memory = if command.memory != DEFAULT_MEMORY {
        command.memory
    } else {
        file.memory
            .unwrap_or_else(|| profile.default_memory.to_owned())
    };
    let bios = command
        .bios
        .or(file.bios)
        .or_else(|| profile.default_bios.map(str::to_owned));
    let baud = if command.baud != DEFAULT_BAUD {
        command.baud
    } else {
        file.baud.unwrap_or(DEFAULT_BAUD)
    };
    let shared_dir = command.shared_dir.or(file.shared_dir);

    Ok(ResolvedVmCommand {
        profile,
        qemu_bin,
        kernel,
        socket: command.socket,
        no_build: command.no_build,
        smp,
        memory,
        bios,
        baud,
        shared_dir,
        command: command.command.map(Into::into),
    })
}

fn load_config_file(path: Option<&Path>) -> Result<VmConfigFile> {
    let path = path.map(Path::to_path_buf).or_else(default_config_path);
    let Some(path) = path else {
        return Ok(VmConfigFile::default());
    };
    if !path.is_file() {
        return Ok(VmConfigFile::default());
    }
    let bytes = fs::read(&path)
        .with_context(|| format!("failed to read inspector VM config {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to decode inspector VM config {}", path.display()))
}

fn default_config_path() -> Option<PathBuf> {
    ProjectDirs::from("cool", "lexo", "helios-inspector")
        .map(|dirs| dirs.config_dir().join("vm.json"))
}

fn ensure_qemu_command(command: &ResolvedVmCommand) -> Result<()> {
    if let Some(shared_dir) = &command.shared_dir {
        if !shared_dir.is_dir() {
            bail!("shared directory does not exist: {}", shared_dir.display());
        }
    }
    Ok(())
}

fn build_vm(command: &ResolvedVmCommand) -> Result<()> {
    let repo_root = repo_root();
    run_step(
        &format!("building {} kernel", arch_label(command.profile.arch)),
        Command::new("cargo")
            .current_dir(repo_root)
            .arg("build")
            .arg("--target")
            .arg(command.profile.cargo_target)
            .arg("--bin")
            .arg(command.profile.kernel_artifact_name),
    )?;
    run_step(
        "building inspector",
        Command::new("cargo")
            .current_dir(repo_root)
            .arg("build")
            .arg("-p")
            .arg("helios-inspector"),
    )?;
    Ok(())
}

fn connect_and_run(command: &ResolvedVmCommand, socket_path: &Path) -> Result<()> {
    let socket = socket_path.to_str().ok_or_else(|| {
        anyhow::anyhow!("socket path must be valid UTF-8: {}", socket_path.display())
    })?;
    let client = connect_client(socket, command.baud, true)?;
    run_connected(client, command.command.clone())
}

fn prepare_boot_artifact_in(
    command: &ResolvedVmCommand,
    runtime_dir: Option<&Path>,
) -> Result<PathBuf> {
    match command.profile.boot_artifact {
        VmBootArtifactKind::KernelBinary => Ok(command.kernel.clone()),
        VmBootArtifactKind::BiosDiskImage => prepare_x86_bios_image(command, runtime_dir),
    }
}

fn prepare_x86_bios_image(
    command: &ResolvedVmCommand,
    runtime_dir: Option<&Path>,
) -> Result<PathBuf> {
    let kernel = fs::canonicalize(&command.kernel)
        .with_context(|| format!("failed to canonicalize kernel {}", command.kernel.display()))?;
    let image = match runtime_dir {
        Some(dir) => dir.join("kernel.bios.img"),
        None => kernel.with_extension("bios.img"),
    };
    let spinner = spinner("building x86_64 BIOS disk image");
    let bios = BiosBoot::new(&kernel);
    bios.create_disk_image(&image)
        .with_context(|| format!("failed to create BIOS image {}", image.display()))?;
    spinner.finish_with_message(format!("{} {}", style("built").green(), image.display()));
    Ok(image)
}

fn arch_label(arch: VmArch) -> &'static str {
    match arch {
        VmArch::Riscv64 => "riscv64",
        VmArch::X86_64 => "x86_64",
    }
}

fn run_step(label: &str, command: &mut Command) -> Result<()> {
    let spinner = spinner(label);
    let status = command
        .status()
        .with_context(|| format!("failed to spawn {label}"))?;
    if status.success() {
        spinner.finish_with_message(format!("{} {}", style("built").green(), label));
        return Ok(());
    }
    spinner.finish_and_clear();
    bail!("{label} exited with status {status}")
}

struct VmRuntime {
    socket_path: PathBuf,
    _tempdir: Option<TempDir>,
    child: Child,
}

impl VmRuntime {
    fn spawn(command: &ResolvedVmCommand) -> Result<Self> {
        let (tempdir, socket_path) = match &command.socket {
            Some(path) => (None, path.clone()),
            None => {
                let dir = tempfile::Builder::new()
                    .prefix("helios-inspector-vm.")
                    .tempdir()
                    .context("failed to create temporary QEMU runtime directory")?;
                (Some(dir), PathBuf::from("debug.sock"))
            }
        };
        let (socket_path, qemu_log) = match &tempdir {
            Some(dir) => (dir.path().join(socket_path), dir.path().join("qemu.log")),
            None => {
                let log = socket_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join("qemu.log");
                (socket_path, log)
            }
        };

        let runtime_dir = qemu_log.parent().unwrap_or_else(|| Path::new("."));
        let artifact = prepare_boot_artifact_in(command, Some(runtime_dir))?;
        let block_image = prepare_block_image(command, runtime_dir)?;

        let spinner = spinner(&format!("starting QEMU for {}", arch_label(command.profile.arch)));
        let mut qemu = Command::new(&command.qemu_bin);
        qemu.arg("-display").arg("none").arg("-monitor").arg("none");
        qemu.arg("-machine").arg(command.profile.machine);
        qemu.arg("-m").arg(&command.memory);
        qemu.arg("-smp").arg(command.smp.to_string());
        if command.profile.console == VmConsoleProfile::SerialUnixSocket {
            qemu.arg("-serial")
                .arg(format!("unix:{},server=on,wait=on", socket_path.display()));
        }
        qemu.stdout(Stdio::from(fs::File::create(&qemu_log).with_context(
            || format!("failed to create {}", qemu_log.display()),
        )?));
        qemu.stderr(Stdio::from(
            fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&qemu_log)
                .with_context(|| format!("failed to open {} for append", qemu_log.display()))?,
        ));
        if command.profile.arch == VmArch::Riscv64 {
            qemu.arg("-global").arg("virtio-mmio.force-legacy=false");
        }
        if let Some(bios) = &command.bios {
            qemu.arg("-bios").arg(bios);
        }
        match command.profile.boot_artifact {
            VmBootArtifactKind::KernelBinary => {
                qemu.arg("-kernel").arg(&artifact);
            }
            VmBootArtifactKind::BiosDiskImage => {}
        }
        if let Some(network) = command.profile.network {
            configure_network_device(&mut qemu, network);
        }
        if let Some(block) = command.profile.block {
            configure_block_device(
                &mut qemu,
                block,
                &artifact,
                block_image.as_deref(),
            );
        }
        if let Some(host_share) = command.profile.host_share {
            if let Some(shared_dir) = &command.shared_dir {
                configure_host_share(&mut qemu, host_share, shared_dir);
            }
        }
        if let Some(watchdog) = command.profile.watchdog {
            configure_watchdog(&mut qemu, watchdog);
        }
        let mut child = qemu.spawn().with_context(|| {
            format!(
                "failed to start QEMU executable {}",
                command.qemu_bin.display()
            )
        })?;
        wait_for_socket(&socket_path, &qemu_log, &mut child)?;
        spinner.finish_with_message(format!(
            "{} {}",
            style("ready").green(),
            socket_path.display()
        ));
        Ok(Self {
            socket_path,
            _tempdir: tempdir,
            child,
        })
    }

    fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    fn shutdown(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl Drop for VmRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn wait_for_socket(socket_path: &Path, qemu_log: &Path, child: &mut Child) -> Result<()> {
    let started = std::time::Instant::now();
    while started.elapsed() < DEFAULT_SOCKET_WAIT {
        if socket_path.exists() {
            return Ok(());
        }
        if child
            .try_wait()
            .context("failed to poll QEMU process state")?
            .is_some()
        {
            let log = fs::read_to_string(qemu_log).unwrap_or_default();
            bail!(
                "QEMU exited before opening the debug serial socket {}
{}",
                socket_path.display(),
                log
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!(
        "timed out waiting for QEMU to create debug serial socket {}",
        socket_path.display()
    )
}

fn spinner(label: &str) -> ProgressBar {
    let bar = ProgressBar::new_spinner();
    bar.set_style(
        ProgressStyle::with_template("{spinner} {msg}")
            .expect("progress style template must stay valid"),
    );
    bar.enable_steady_tick(Duration::from_millis(80));
    bar.set_message(label.to_owned());
    bar
}

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("inspector crate must live under repo root")
}

fn default_kernel_path(arch: VmArch) -> PathBuf {
    let profile = arch.profile();
    repo_root()
        .join("target")
        .join(profile.cargo_target)
        .join("debug")
        .join(profile.kernel_artifact_name)
}

fn prepare_block_image(command: &ResolvedVmCommand, runtime_dir: &Path) -> Result<Option<PathBuf>> {
    let Some(block_profile) = command.profile.block else {
        return Ok(None);
    };
    if block_profile == VmBlockProfile::VirtioPciBootDisk {
        return Ok(None);
    }
    let image = runtime_dir.join("data.img");
    let file = fs::File::create(&image)
        .with_context(|| format!("failed to create block image {}", image.display()))?;
    file.set_len(DEFAULT_BLOCK_DEVICE_BYTES)
        .with_context(|| format!("failed to size block image {}", image.display()))?;
    Ok(Some(image))
}

fn configure_network_device(qemu: &mut Command, network: VmNetworkProfile) {
    qemu.arg("-netdev").arg("user,id=net0");
    match network {
        VmNetworkProfile::VirtioMmioUser => {
            qemu.arg("-device").arg("virtio-net-device,netdev=net0");
        }
        VmNetworkProfile::VirtioPciUser => {
            qemu.arg("-device").arg("virtio-net-pci,netdev=net0");
        }
    }
}

fn configure_block_device(
    qemu: &mut Command,
    block: VmBlockProfile,
    boot_artifact: &Path,
    data_image: Option<&Path>,
) {
    match block {
        VmBlockProfile::VirtioMmioDataDisk => {
            let image = data_image
                .unwrap_or_else(|| panic!("virtio-mmio block device requires a prepared data image"));
            qemu.arg("-drive").arg(format!(
                "if=none,format=raw,file={},id=rootfs",
                image.display()
            ));
            qemu.arg("-device").arg("virtio-blk-device,drive=rootfs");
        }
        VmBlockProfile::VirtioPciBootDisk => {
            qemu.arg("-drive").arg(format!(
                "if=none,format=raw,file={},id=bootdisk",
                boot_artifact.display()
            ));
            qemu.arg("-device")
                .arg("virtio-blk-pci,drive=bootdisk,bootindex=0");
        }
    }
}

fn configure_host_share(qemu: &mut Command, host_share: VmHostShareProfile, shared_dir: &Path) {
    match host_share {
        VmHostShareProfile::Virtio9pMmio => {
            qemu.arg("-fsdev").arg(format!(
                "local,id=hostfs,path={},security_model=none,multidevs=remap",
                shared_dir.display()
            ));
            qemu.arg("-device")
                .arg(format!("virtio-9p-device,fsdev=hostfs,mount_tag={HOST_SHARE_MOUNT_TAG}"));
        }
    }
}

fn configure_watchdog(qemu: &mut Command, watchdog: VmWatchdogProfile) {
    match watchdog {
        VmWatchdogProfile::I6300Esb => {
            qemu.arg("-watchdog").arg("i6300esb");
        }
    }
}

impl From<VmSessionCommand> for SessionCommand {
    fn from(value: VmSessionCommand) -> Self {
        match value {
            VmSessionCommand::Dash(command) => Self::Dash(command),
            VmSessionCommand::Tracing(command) => Self::Tracing(command),
            VmSessionCommand::Stats => Self::Stats,
            VmSessionCommand::Repl => Self::Repl,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn riscv64_profile_matches_qemu_first_baseline() {
        assert_eq!(RISCV64_VM_PROFILE.arch, VmArch::Riscv64);
        assert_eq!(RISCV64_VM_PROFILE.machine, "virt");
        assert_eq!(
            RISCV64_VM_PROFILE.boot_artifact,
            VmBootArtifactKind::KernelBinary
        );
        assert_eq!(
            RISCV64_VM_PROFILE.network,
            Some(VmNetworkProfile::VirtioMmioUser)
        );
        assert_eq!(
            RISCV64_VM_PROFILE.block,
            Some(VmBlockProfile::VirtioMmioDataDisk)
        );
        assert_eq!(
            RISCV64_VM_PROFILE.host_share,
            Some(VmHostShareProfile::Virtio9pMmio)
        );
        assert_eq!(RISCV64_VM_PROFILE.watchdog, None);
    }

    #[test]
    fn x86_64_profile_matches_qemu_first_baseline() {
        assert_eq!(X86_64_VM_PROFILE.arch, VmArch::X86_64);
        assert_eq!(X86_64_VM_PROFILE.machine, "q35");
        assert_eq!(
            X86_64_VM_PROFILE.boot_artifact,
            VmBootArtifactKind::BiosDiskImage
        );
        assert_eq!(
            X86_64_VM_PROFILE.network,
            Some(VmNetworkProfile::VirtioPciUser)
        );
        assert_eq!(
            X86_64_VM_PROFILE.block,
            Some(VmBlockProfile::VirtioPciBootDisk)
        );
        assert_eq!(X86_64_VM_PROFILE.host_share, None);
        assert_eq!(
            X86_64_VM_PROFILE.watchdog,
            Some(VmWatchdogProfile::I6300Esb)
        );
    }

    #[test]
    fn resolve_uses_profile_defaults_without_local_config() {
        let tempdir =
            tempfile::tempdir().expect("temporary directory for VM config resolution must exist");
        let missing_config = tempdir.path().join("missing-vm.json");
        let command = VmCommand {
            arch: VmArch::X86_64,
            config: Some(missing_config),
            qemu_bin: None,
            kernel: None,
            socket: None,
            no_build: true,
            smp: None,
            memory: DEFAULT_MEMORY.to_owned(),
            bios: None,
            baud: DEFAULT_BAUD,
            shared_dir: None,
            command: None,
        };

        let resolved = resolve(command).expect("VM command resolution must succeed");
        assert_eq!(resolved.profile, &X86_64_VM_PROFILE);
        assert_eq!(resolved.smp, DEFAULT_X86_SMP);
        assert_eq!(resolved.memory, DEFAULT_MEMORY);
        assert_eq!(resolved.bios, None);
        assert_eq!(
            resolved.kernel,
            repo_root()
                .join("target")
                .join(X86_64_VM_PROFILE.cargo_target)
                .join("debug")
                .join(X86_64_VM_PROFILE.kernel_artifact_name)
        );
    }
}
