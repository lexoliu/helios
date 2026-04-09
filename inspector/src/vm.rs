use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use bootloader::BiosBoot;
use clap::{Args as ClapArgs, Subcommand, ValueEnum};
use console::style;
use directories::ProjectDirs;
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
    arch: VmArch,
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
    let qemu_bin = command
        .qemu_bin
        .or(file.qemu_bin)
        .unwrap_or_else(|| PathBuf::from(arch.qemu_bin()));
    let kernel = command
        .kernel
        .or(file.kernel)
        .unwrap_or_else(|| default_kernel_path(arch));
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
        Command::new("cargo")
            .current_dir(repo_root)
            .arg("build")
            .arg("--target")
            .arg(command.arch.cargo_target())
            .arg("--bin")
            .arg(command.arch.kernel_artifact_name()),
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

fn prepare_boot_artifact(command: &ResolvedVmCommand) -> Result<PathBuf> {
    match command.arch {
        VmArch::Riscv64 => Ok(command.kernel.clone()),
        VmArch::X86_64 => prepare_x86_bios_image(command),
    }
}

fn prepare_x86_bios_image(command: &ResolvedVmCommand) -> Result<PathBuf> {
    let kernel = fs::canonicalize(&command.kernel)
        .with_context(|| format!("failed to canonicalize kernel {}", command.kernel.display()))?;
    let image = kernel.with_extension("bios.img");
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

        let artifact = prepare_boot_artifact(command)?;

        let spinner = spinner(&format!("starting QEMU for {}", arch_label(command.arch)));
        let mut qemu = Command::new(&command.qemu_bin);
        qemu.arg("-display").arg("none").arg("-monitor").arg("none");
        qemu.arg("-machine").arg(command.arch.qemu_machine());
        qemu.arg("-m").arg(&command.memory);
        qemu.arg("-smp").arg(command.smp.to_string());
        qemu.arg("-serial")
            .arg(format!("unix:{},server=on,wait=on", socket_path.display()));
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
                    qemu.arg("-device")
                        .arg("virtio-9p-device,fsdev=hostfs,mount_tag=hostshare");
                }
            }
            VmArch::X86_64 => {
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
    repo_root()
        .join("target")
        .join(arch.cargo_target())
        .join("debug")
        .join(arch.kernel_artifact_name())
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
