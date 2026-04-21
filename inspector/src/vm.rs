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
/// Virtio 9p mount tag matching the kernel's host share device expectation.
const HOST_SHARE_MOUNT_TAG: &str = "hostshare";
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
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum VmArch {
    Riscv64,
    X86_64,
}

impl VmArch {
    fn qemu_bin(self) -> &'static str {
        match self {
            Self::Riscv64 => DEFAULT_RISCV_QEMU_BIN,
            Self::X86_64 => DEFAULT_X86_QEMU_BIN,
        }
    }

    fn cargo_target(self) -> &'static str {
        match self {
            Self::Riscv64 => "riscv64gc-unknown-none-elf",
            Self::X86_64 => "x86_64-unknown-none",
        }
    }

    fn default_smp(self) -> u16 {
        match self {
            Self::Riscv64 => DEFAULT_RISCV_SMP,
            Self::X86_64 => DEFAULT_X86_SMP,
        }
    }

    fn qemu_machine(self) -> &'static str {
        match self {
            Self::Riscv64 => "virt",
            Self::X86_64 => "q35",
        }
    }

    fn kernel_artifact_name(self) -> &'static str {
        "helios"
    }
}

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
    arch: VmArch,
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
    let release = command.release || file.release.unwrap_or(false);
    let qemu_bin = command
        .qemu_bin
        .or(file.qemu_bin)
        .unwrap_or_else(|| PathBuf::from(arch.qemu_bin()));
    let kernel = command
        .kernel
        .or(file.kernel)
        .unwrap_or_else(|| default_kernel_path(arch, release));
    let smp = command
        .smp
        .or(file.smp)
        .unwrap_or_else(|| arch.default_smp());
    let memory = if command.memory != DEFAULT_MEMORY {
        command.memory
    } else {
        file.memory.unwrap_or_else(|| DEFAULT_MEMORY.to_owned())
    };
    let bios = command.bios.or(file.bios).or_else(|| match arch {
        VmArch::Riscv64 => Some("default".to_owned()),
        VmArch::X86_64 => None,
    });
    let baud = if command.baud != DEFAULT_BAUD {
        command.baud
    } else {
        file.baud.unwrap_or(DEFAULT_BAUD)
    };
    let shared_dir = command.shared_dir.or(file.shared_dir);

    Ok(ResolvedVmCommand {
        arch,
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
    if command.arch == VmArch::Riscv64 && command.smp < 2 {
        bail!(
            "QEMU must run with at least 2 harts because the embedded debugger occupies a dedicated hart"
        )
    }
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
        &format!("building {} kernel", arch_label(command.arch)),
        cargo_build_command(repo_root, command.release)
            .arg("--target")
            .arg(command.arch.cargo_target())
            .arg("--bin")
            .arg(command.arch.kernel_artifact_name()),
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
    let boot_sync = true;
    let client = connect_client(socket, command.baud, boot_sync)
        .context("failed to connect inspector RPC client")?;
    run_connected(client, command.command.clone())
}

fn prepare_boot_artifact(
    command: &ResolvedVmCommand,
    runtime_dir: Option<&Path>,
) -> Result<PathBuf> {
    match command.arch {
        VmArch::Riscv64 => Ok(command.kernel.clone()),
        VmArch::X86_64 => prepare_x86_bios_image(command, runtime_dir),
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

        let spinner = spinner(&format!("starting QEMU for {}", arch_label(command.arch)));
        let mut qemu = Command::new(&command.qemu_bin);
        qemu.arg("-display").arg("none").arg("-monitor").arg("none");
        qemu.arg("-machine").arg(command.arch.qemu_machine());
        qemu.arg("-m").arg(&command.memory);
        qemu.arg("-smp").arg(command.smp.to_string());
        qemu.arg("-serial")
            .arg(format!("unix:{},server=on,wait=on", socket_path.display()));
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
        match command.arch {
            VmArch::Riscv64 => {
                qemu.arg("-global").arg("virtio-mmio.force-legacy=false");
                qemu.arg("-device").arg("i6300esb");
                qemu.arg("-watchdog-action").arg("reset");
                if let Some(bios) = &command.bios {
                    qemu.arg("-bios").arg(bios);
                }
                qemu.arg("-kernel").arg(&artifact);
                qemu.arg("-netdev").arg("user,id=net0");
                qemu.arg("-device").arg("virtio-net-device,netdev=net0");
                if let Some(shared_dir) = &command.shared_dir {
                    qemu.arg("-fsdev").arg(format!(
                        "local,id=hostfs,path={},security_model=none,multidevs=remap",
                        shared_dir.display()
                    ));
                    qemu.arg("-device").arg(format!(
                        "virtio-9p-device,fsdev=hostfs,mount_tag={HOST_SHARE_MOUNT_TAG}"
                    ));
                }
            }
            VmArch::X86_64 => {
                qemu.arg("-cpu").arg("max");
                qemu.arg("-device").arg("i6300esb");
                qemu.arg("-watchdog-action").arg("reset");
                qemu.arg("-drive")
                    .arg(format!("format=raw,file={}", artifact.display()));
            }
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
                "QEMU exited before opening the debug serial socket {}
{}",
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
    repo_root()
        .join("target")
        .join(arch.cargo_target())
        .join(if release { "release" } else { "debug" })
        .join(arch.kernel_artifact_name())
}

#[cfg(test)]
mod tests {
    use std::io::{ErrorKind, Read};
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use anyhow::{bail, Context as _, Result};

    use super::{
        default_kernel_path, repo_root, ResolvedVmCommand, VmArch, VmRuntime, DEFAULT_BAUD,
        DEFAULT_MEMORY,
    };

    const WATCHDOG_SELF_TEST_DELAY_MS: &str = "5000";
    const WATCHDOG_TIMEOUT_SECS: &str = "10";
    const WATCHDOG_STAGE_TIMEOUT: Duration = Duration::from_secs(120);
    const SERIAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    const SERIAL_READ_TIMEOUT: Duration = Duration::from_millis(500);
    const DEBUGGER_RUN_STAGE_MARKER: &[u8] = b"[KDBG run:begin]";

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
            .arg(arch.cargo_target())
            .arg("--bin")
            .arg(arch.kernel_artifact_name())
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
            arch,
            release: false,
            qemu_bin: arch.qemu_bin().into(),
            kernel: default_kernel_path(arch, false),
            socket: None,
            no_build: true,
            smp: arch.default_smp(),
            memory: match arch {
                VmArch::Riscv64 => "2G".to_owned(),
                VmArch::X86_64 => DEFAULT_MEMORY.to_owned(),
            },
            bios: (arch == VmArch::Riscv64).then(|| "default".to_owned()),
            baud: DEFAULT_BAUD,
            shared_dir: None,
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
