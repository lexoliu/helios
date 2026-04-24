use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::process::CommandExt as _;
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
const DEFAULT_RISCV_MEMORY: &str = "2G";
const DEFAULT_RISCV_SMP: u16 = 4;
const DEFAULT_X86_SMP: u16 = 2;
const DEFAULT_SOCKET_WAIT: Duration = Duration::from_secs(10);
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(50);
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
    default_memory: DEFAULT_RISCV_MEMORY,
    default_bios: Some("default"),
    boot_artifact: VmBootArtifactKind::KernelBinary,
    console: VmConsoleProfile::SerialUnixSocket,
    network: Some(VmNetworkProfile::VirtioMmioUser),
    block: Some(VmBlockProfile::VirtioMmioDataDisk),
    host_share: Some(VmHostShareProfile::Virtio9pMmio),
    watchdog: Some(VmWatchdogProfile::I6300Esb),
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
    pub(crate) release: Option<bool>,
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
    #[serde(default)]
    pub(crate) gdb: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub(crate) struct VmCommand {
    #[arg(long, value_enum, default_value_t = VmArch::Riscv64)]
    arch: VmArch,

    #[arg(long, default_value_t = false)]
    release: bool,

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

    #[arg(long)]
    gdb: Option<String>,

    #[command(subcommand)]
    command: Option<VmSessionCommand>,
}

#[derive(Debug, Subcommand)]
enum VmSessionCommand {
    Shell(crate::ShellCommand),
    Tracing(crate::TracingCommand),
    Stats,
    Repl,
}

#[derive(Debug)]
struct ResolvedVmCommand {
    profile: &'static VmProfile,
    release: bool,
    qemu_bin: PathBuf,
    kernel: PathBuf,
    socket: Option<PathBuf>,
    no_build: bool,
    smp: u16,
    memory: String,
    bios: Option<String>,
    baud: u32,
    shared_dir: Option<PathBuf>,
    gdb: Option<String>,
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
    let release = command.release || file.release.unwrap_or(false);
    let qemu_bin = command
        .qemu_bin
        .or(file.qemu_bin)
        .unwrap_or_else(|| PathBuf::from(profile.qemu_bin));
    let kernel = command
        .kernel
        .or(file.kernel)
        .unwrap_or_else(|| default_kernel_path(arch, release));
    let smp = command.smp.or(file.smp).unwrap_or(profile.default_smp);
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
    let gdb = command.gdb.or(file.gdb);

    Ok(ResolvedVmCommand {
        profile,
        release,
        qemu_bin,
        kernel,
        socket: command.socket,
        no_build: command.no_build,
        smp,
        memory,
        bios,
        baud,
        shared_dir,
        gdb,
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
        cargo_build_command(repo_root, command.release)
            .arg("--target")
            .arg(command.profile.cargo_target)
            .arg("--bin")
            .arg(command.profile.kernel_artifact_name),
    )?;
    run_step(
        "building inspector",
        cargo_build_command(repo_root, command.release)
            .arg("-p")
            .arg("helios-inspector"),
    )?;
    Ok(())
}

fn cargo_build_command(repo_root: &Path, release: bool) -> Command {
    let mut command = Command::new("cargo");
    command.current_dir(repo_root).arg("build");
    if release {
        command.arg("--release");
    }
    command
}

fn connect_and_run(command: &ResolvedVmCommand, socket_path: &Path) -> Result<()> {
    let socket = socket_path.to_str().ok_or_else(|| {
        anyhow::anyhow!("socket path must be valid UTF-8: {}", socket_path.display())
    })?;
    let client = connect_client(socket, command.baud, true)
        .context("failed to connect inspector RPC client")?;
    run_connected(client, command.command.clone())
}

fn prepare_boot_artifact(
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
    _tempdir: TempDir,
    child: Child,
}

impl VmRuntime {
    fn spawn(command: &ResolvedVmCommand) -> Result<Self> {
        let tempdir = tempfile::Builder::new()
            .prefix("helios-inspector-vm.")
            .tempdir()
            .context("failed to create temporary QEMU runtime directory")?;
        let socket_path = command
            .socket
            .clone()
            .unwrap_or_else(|| tempdir.path().join("debug.sock"));
        let qemu_log = tempdir.path().join("qemu.log");

        prepare_socket_path(&socket_path)?;
        let artifact = prepare_boot_artifact(command, Some(tempdir.path()))?;
        let block_image = prepare_block_image(command, tempdir.path())?;

        let spinner = spinner(&format!(
            "starting QEMU for {}",
            arch_label(command.profile.arch)
        ));
        let mut qemu = Command::new(&command.qemu_bin);
        qemu.arg("-display").arg("none").arg("-monitor").arg("none");
        qemu.arg("-machine").arg(command.profile.machine);
        qemu.arg("-m").arg(&command.memory);
        qemu.arg("-smp").arg(command.smp.to_string());
        if command.profile.console == VmConsoleProfile::SerialUnixSocket {
            qemu.arg("-serial")
                .arg(format!("unix:{},server=on,wait=on", socket_path.display()));
        }
        if let Some(gdb) = &command.gdb {
            qemu.arg("-gdb").arg(gdb);
        }
        if command.profile.arch == VmArch::X86_64 {
            qemu.arg("-cpu").arg("max");
        }
        qemu.process_group(0);
        qemu.stdin(Stdio::null());
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
            configure_block_device(&mut qemu, block, &artifact, block_image.as_deref());
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

fn prepare_socket_path(socket_path: &Path) -> Result<()> {
    if let Some(parent) = socket_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create socket directory {}", parent.display()))?;
    }
    if !socket_path.exists() {
        return Ok(());
    }

    let metadata = fs::symlink_metadata(socket_path).with_context(|| {
        format!(
            "failed to inspect existing socket path {}",
            socket_path.display()
        )
    })?;
    if !metadata.file_type().is_socket() {
        bail!(
            "refusing to overwrite existing non-socket path {}",
            socket_path.display()
        );
    }
    fs::remove_file(socket_path)
        .with_context(|| format!("failed to remove stale socket {}", socket_path.display()))?;
    Ok(())
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
            let log = fs::read_to_string(qemu_log)
                .with_context(|| format!("failed to read QEMU log {}", qemu_log.display()))?;
            bail!(
                "QEMU exited before opening the debug serial socket {}\n{}",
                socket_path.display(),
                log
            );
        }
        std::thread::sleep(SOCKET_POLL_INTERVAL);
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

fn default_kernel_path(arch: VmArch, release: bool) -> PathBuf {
    let profile = arch.profile();
    repo_root()
        .join("target")
        .join(profile.cargo_target)
        .join(if release { "release" } else { "debug" })
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
            let image = data_image.unwrap_or_else(|| {
                panic!("virtio-mmio block device requires a prepared data image")
            });
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
            qemu.arg("-device").arg(format!(
                "virtio-9p-device,fsdev=hostfs,mount_tag={HOST_SHARE_MOUNT_TAG}"
            ));
        }
    }
}

fn configure_watchdog(qemu: &mut Command, watchdog: VmWatchdogProfile) {
    match watchdog {
        VmWatchdogProfile::I6300Esb => {
            qemu.arg("-device").arg("i6300esb");
            qemu.arg("-watchdog-action").arg("reset");
        }
    }
}

impl From<VmSessionCommand> for SessionCommand {
    fn from(value: VmSessionCommand) -> Self {
        match value {
            VmSessionCommand::Shell(command) => Self::Shell(command),
            VmSessionCommand::Tracing(command) => Self::Tracing(command),
            VmSessionCommand::Stats => Self::Stats,
            VmSessionCommand::Repl => Self::Repl,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{ErrorKind, Read};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    use anyhow::{bail, Context as _, Result};
    use helios_inspector_protocol::debugger::programs as debugger_programs;

    use super::*;

    const WATCHDOG_SELF_TEST_DELAY_MS: &str = "5000";
    const WATCHDOG_TIMEOUT_SECS: &str = "10";
    const WATCHDOG_STAGE_TIMEOUT: Duration = Duration::from_secs(120);
    const DIRECT_EXEC_TIMEOUT: Duration = Duration::from_secs(900);
    const SERIAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    const SERIAL_READ_TIMEOUT: Duration = Duration::from_millis(500);
    const DEBUGGER_RUN_STAGE_MARKER: &[u8] = b"[KDBG run:begin]";

    #[test]
    fn riscv64_profile_matches_qemu_first_baseline() {
        assert_eq!(RISCV64_VM_PROFILE.arch, VmArch::Riscv64);
        assert_eq!(RISCV64_VM_PROFILE.machine, "virt");
        assert_eq!(RISCV64_VM_PROFILE.default_memory, DEFAULT_RISCV_MEMORY);
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
        assert_eq!(
            RISCV64_VM_PROFILE.watchdog,
            Some(VmWatchdogProfile::I6300Esb)
        );
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
            release: false,
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
            gdb: None,
            command: None,
        };

        let resolved = resolve(command).expect("VM command resolution must succeed");
        assert_eq!(resolved.profile, &X86_64_VM_PROFILE);
        assert!(!resolved.release);
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

    #[test]
    #[ignore = "requires qemu and cross-compiled kernels"]
    fn watchdog_self_test_resets_x86_and_riscv() -> Result<()> {
        assert_watchdog_reset(VmArch::X86_64).context("x86 watchdog self-test failed")?;
        assert_watchdog_reset(VmArch::Riscv64).context("riscv watchdog self-test failed")?;
        Ok(())
    }

    fn assert_watchdog_reset(arch: VmArch) -> Result<()> {
        build_watchdog_test_kernel(arch)?;
        let command = watchdog_test_command(arch);
        let mut runtime = VmRuntime::spawn(&command)?;
        wait_for_stage_occurrences(runtime.socket_path(), DEBUGGER_RUN_STAGE_MARKER, 2)?;
        runtime.shutdown();
        Ok(())
    }

    fn build_watchdog_test_kernel(arch: VmArch) -> Result<()> {
        let status = std::process::Command::new("cargo")
            .current_dir(repo_root())
            .arg("build")
            .arg("--target")
            .arg(arch.profile().cargo_target)
            .arg("--bin")
            .arg(arch.profile().kernel_artifact_name)
            .env("HELIOS_BOOT_PROGRAMS", "debugger")
            .env("HELIOS_WATCHDOG_SELF_TEST", "1")
            .env("HELIOS_WATCHDOG_TIMEOUT_SECS", WATCHDOG_TIMEOUT_SECS)
            .env(
                "HELIOS_WATCHDOG_SELF_TEST_DELAY_MS",
                WATCHDOG_SELF_TEST_DELAY_MS,
            )
            .status()
            .context("failed to spawn cargo for watchdog self-test kernel build")?;
        if status.success() {
            return Ok(());
        }
        bail!("watchdog self-test kernel build exited with status {status}");
    }

    fn watchdog_test_command(arch: VmArch) -> ResolvedVmCommand {
        ResolvedVmCommand {
            profile: arch.profile(),
            release: false,
            qemu_bin: PathBuf::from(arch.profile().qemu_bin),
            kernel: default_kernel_path(arch, false),
            socket: None,
            no_build: true,
            smp: arch.profile().default_smp,
            memory: match arch {
                VmArch::Riscv64 => "2G".to_owned(),
                VmArch::X86_64 => DEFAULT_MEMORY.to_owned(),
            },
            bios: arch.profile().default_bios.map(str::to_owned),
            baud: DEFAULT_BAUD,
            shared_dir: None,
            gdb: None,
            command: None,
        }
    }

    #[test]
    #[ignore = "requires qemu, a release riscv guest build, and staged host artifacts"]
    fn exec_path_runs_host_curl_in_riscv_release_vm() -> Result<()> {
        let command = direct_exec_command(VmArch::Riscv64);
        build_vm(&command)?;
        let mut runtime = VmRuntime::spawn(&command)?;
        let socket = runtime
            .socket_path()
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("socket path must be valid UTF-8"))?;
        let client = connect_client(socket, DEFAULT_BAUD, true)
            .context("failed to connect direct-exec RPC client")?;

        let curl = crate::runtime::block_on(async {
            crate::runtime::timeout(
                DIRECT_EXEC_TIMEOUT,
                debugger_programs::exec_path(
                    &client,
                    "/host/artifacts/wasi-tools/curl-stripped.wasm",
                    &["http://example.com".to_owned()],
                ),
            )
            .await
        })
        .ok_or_else(|| anyhow::anyhow!("timed out waiting for direct curl exec-path result"))??;
        assert_eq!(curl.exit_code, 0, "curl exited non-zero: {curl:?}");
        let curl_stdout = String::from_utf8_lossy(&curl.output.stdout).to_ascii_lowercase();
        assert!(
            curl_stdout.contains("<title>example domain</title>"),
            "unexpected curl stdout: {}",
            String::from_utf8_lossy(&curl.output.stdout)
        );

        runtime.shutdown();
        Ok(())
    }

    #[test]
    #[ignore = "requires qemu, a release riscv guest build, and staged host artifacts"]
    fn exec_path_runs_host_cpython_in_riscv_release_vm() -> Result<()> {
        let command = direct_exec_command(VmArch::Riscv64);
        build_vm(&command)?;
        let mut runtime = VmRuntime::spawn(&command)?;
        let socket = runtime
            .socket_path()
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("socket path must be valid UTF-8"))?;
        let client = connect_client(socket, DEFAULT_BAUD, true)
            .context("failed to connect direct-exec RPC client")?;

        let python = crate::runtime::block_on(async {
            crate::runtime::timeout(
                DIRECT_EXEC_TIMEOUT,
                debugger_programs::exec_path(
                    &client,
                    "/host/artifacts/python3-root/python3.wasm",
                    &["-c".to_owned(), "print(40+2)".to_owned()],
                ),
            )
            .await
        })
        .ok_or_else(|| {
            anyhow::anyhow!("timed out waiting for direct CPython exec-path result")
        })??;
        assert_eq!(python.exit_code, 0, "CPython exited non-zero: {python:?}");
        let python_stdout = String::from_utf8_lossy(&python.output.stdout);
        assert_eq!(python_stdout.trim(), "42", "unexpected CPython stdout");

        runtime.shutdown();
        Ok(())
    }

    #[test]
    #[ignore = "requires qemu, a release riscv guest build, and staged host artifacts"]
    fn shell_runs_host_cpython_in_riscv_release_vm() -> Result<()> {
        let command = direct_exec_command(VmArch::Riscv64);
        build_vm(&command)?;
        let mut runtime = VmRuntime::spawn(&command)?;
        let socket = runtime
            .socket_path()
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("socket path must be valid UTF-8"))?;
        let mut client = connect_client(socket, DEFAULT_BAUD, true)
            .context("failed to connect shell RPC client")?;

        let python = crate::runtime::block_on(async {
            crate::runtime::timeout(
                DIRECT_EXEC_TIMEOUT,
                crate::programs::exec(
                    &mut client,
                    crate::programs::REMOTE_SHELL_PATH,
                    &[
                        "-c".to_owned(),
                        "/host/artifacts/python3-root/python3.wasm -c \"print(40+2)\"".to_owned(),
                    ],
                ),
            )
            .await
        })
        .ok_or_else(|| anyhow::anyhow!("timed out waiting for shell CPython result"))??;
        assert_eq!(
            python.exit_code, 0,
            "shell CPython exited non-zero: {python:?}"
        );
        let python_stdout = String::from_utf8_lossy(&python.output.stdout);
        assert_eq!(
            python_stdout.trim(),
            "42",
            "unexpected shell CPython stdout"
        );

        runtime.shutdown();
        Ok(())
    }

    fn direct_exec_command(arch: VmArch) -> ResolvedVmCommand {
        let profile = arch.profile();
        ResolvedVmCommand {
            profile,
            release: true,
            qemu_bin: PathBuf::from(profile.qemu_bin),
            kernel: default_kernel_path(arch, true),
            socket: None,
            no_build: true,
            smp: profile.default_smp,
            memory: profile.default_memory.to_owned(),
            bios: profile.default_bios.map(str::to_owned),
            baud: DEFAULT_BAUD,
            shared_dir: Some(repo_root().to_path_buf()),
            gdb: None,
            command: None,
        }
    }

    fn connect_serial_socket(socket_path: &Path) -> Result<UnixStream> {
        let started = Instant::now();
        loop {
            match UnixStream::connect(socket_path) {
                Ok(stream) => {
                    stream
                        .set_read_timeout(Some(SERIAL_READ_TIMEOUT))
                        .context("failed to configure serial socket read timeout")?;
                    return Ok(stream);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::ConnectionRefused | ErrorKind::NotFound
                    ) && started.elapsed() < SERIAL_CONNECT_TIMEOUT => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to connect to QEMU debug serial socket {}",
                            socket_path.display()
                        )
                    });
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_for_stage_occurrences(
        socket_path: &Path,
        marker: &[u8],
        expected: usize,
    ) -> Result<()> {
        let deadline = Instant::now() + WATCHDOG_STAGE_TIMEOUT;
        let mut serial = connect_serial_socket(socket_path)?;
        let mut line = Vec::new();
        let mut seen = 0usize;
        let mut buffer = [0_u8; 256];
        let mut observed_stages = Vec::new();
        let mut recent_lines = Vec::new();
        let mut reconnects = 0usize;

        while Instant::now() < deadline {
            match serial.read(&mut buffer) {
                Ok(0) => {
                    reconnects += 1;
                    serial = connect_serial_socket(socket_path)?;
                }
                Ok(count) => {
                    for &byte in &buffer[..count] {
                        match byte {
                            b'\n' => {
                                if !line.is_empty() {
                                    recent_lines.push(String::from_utf8_lossy(&line).into_owned());
                                    if recent_lines.len() > 32 {
                                        recent_lines.remove(0);
                                    }
                                }
                                if line.starts_with(b"[KDBG ") {
                                    observed_stages
                                        .push(String::from_utf8_lossy(&line).into_owned());
                                    if observed_stages.len() > 32 {
                                        observed_stages.remove(0);
                                    }
                                }
                                if line.as_slice() == marker {
                                    seen += 1;
                                    if seen == expected {
                                        return Ok(());
                                    }
                                }
                                line.clear();
                            }
                            b'\r' => {}
                            other => line.push(other),
                        }
                    }
                }
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(error) => {
                    return Err(error).context("failed while reading QEMU debug serial socket");
                }
            }
        }

        bail!(
            "timed out waiting for {expected} debugger run markers; observed {seen}; reconnects: {reconnects}; recent stages: {}; recent lines: {}",
            observed_stages.join(" | "),
            recent_lines.join(" | ")
        )
    }
}
