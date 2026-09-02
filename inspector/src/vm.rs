use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use askama::Template;
use clap::{Args as ClapArgs, Subcommand, ValueEnum};
use console::style;
use directories::ProjectDirs;
use helios_hal::fs::HOST_SHARE_MOUNT_TAG;
use helios_inspector_protocol::debugger::filesystem as debugger_fs;
use helios_inspector_protocol::system::profiling as system_profiling;
use helios_inspector_protocol::system::programs as system_programs;
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

use crate::stats_tui::format_bytes;
use crate::workload_bench::{VmProvenance, WorkloadBenchCommand};
use crate::{SessionCommand, connect_client, run_connected};

mod network;
mod qemu;
mod qmp;

use network::{
    HostPlatform, NetSetupCommand, NetTeardownCommand, QemuNetArgs, VmNetwork, VmNetworkArgs,
    VmNetworkFile, VmNetworkProfile,
};
use qemu::QemuOptions;
use qmp::QmpClient;

const DEFAULT_BAUD: u32 = 115_200;
const DEFAULT_AARCH64_QEMU_BIN: &str = "qemu-system-aarch64";
const DEFAULT_RISCV_QEMU_BIN: &str = "qemu-system-riscv64";
const DEFAULT_X86_QEMU_BIN: &str = "qemu-system-x86_64";
const DEFAULT_AARCH64_MEMORY: &str = "2G";
const DEFAULT_RISCV_MEMORY: &str = "2G";
const DEFAULT_X86_MEMORY: &str = "2G";
const DEFAULT_AARCH64_SMP: u16 = 4;
const DEFAULT_RISCV_SMP: u16 = 4;
const DEFAULT_X86_SMP: u16 = 4;
const DEFAULT_SOCKET_WAIT: Duration = Duration::from_secs(10);
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Size of the scratch disk every profile attaches.
const DEFAULT_DATA_DISK_BYTES: u64 = 256 * 1024 * 1024;
/// The serial the guest kernel identifies its own disk by. Nothing else
/// on the bus carries it, so the boot image is never mistaken for it.
const DATA_DISK_SERIAL: &str = "helios-data";
const DEFAULT_GDB_ENDPOINT: &str = "tcp::1234";
/// The QEMU IOThread the memory balloon's free-page hint queue runs on.
const BALLOON_IOTHREAD_ID: &str = "balloon-io";
/// How long the guest's reported balloon size has to hold still before a
/// wait calls it settled short of the target it was given.
const BALLOON_STILL_FOR: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum VmArch {
    Aarch64,
    Riscv64,
    X86_64,
}

impl VmArch {
    fn profile(self) -> &'static VmProfile {
        match self {
            Self::Aarch64 => &AARCH64_VIRT_HVF_PROFILE,
            Self::Riscv64 => &RISCV64_VM_PROFILE,
            Self::X86_64 => &X86_64_VM_PROFILE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VmBootArtifactKind {
    KernelBinary,
    LimineUefiDiskImage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VmConsoleProfile {
    SerialUnixSocket,
}

/// How a profile attaches the image the guest firmware boots from.
///
/// Only the profiles whose boot artifact is a disk image have one; the
/// guest kernel never writes to it, and on the arches where it is visible
/// at all it is told apart from the scratch disk by its serial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VmBootDiskProfile {
    VirtioPci,
}

/// How a profile attaches the scratch disk the guest kernel owns.
///
/// Every profile has one: the kernel identifies it by the
/// [`DATA_DISK_SERIAL`] serial and proves it round-trips at boot, so a
/// guest booted without it cannot bring its block device up at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VmDataDiskProfile {
    VirtioMmio,
    VirtioPci,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VmHostShareProfile {
    Virtio9pMmio,
    Virtio9pPci,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VmWatchdogProfile {
    I6300Esb,
}

/// How a profile exposes the guest's entropy device.
///
/// Every profile has one: the kernel's root DRBG treats it as its
/// continuous source, and a guest booted without it would run the whole
/// session on nothing but its boot seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VmEntropyProfile {
    VirtioRngMmio,
    VirtioRngPci,
}

/// How a profile exposes the guest's memory balloon.
///
/// Every profile has one: the balloon is the only way a host resizes a
/// running guest's memory, and free-page reporting is what lets the host
/// reclaim what the guest is not using, so a guest booted without it
/// would hold its whole `-m` allocation resident forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VmBalloonProfile {
    VirtioBalloonMmio,
    VirtioBalloonPci,
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
    default_accel: &'static [&'static str],
    default_cpu: Option<&'static str>,
    boot_artifact: VmBootArtifactKind,
    console: VmConsoleProfile,
    network: Option<VmNetworkProfile>,
    boot_disk: Option<VmBootDiskProfile>,
    data_disk: VmDataDiskProfile,
    host_share: Option<VmHostShareProfile>,
    watchdog: Option<VmWatchdogProfile>,
    entropy: VmEntropyProfile,
    /// The translation unit this machine can put its virtio devices
    /// behind, if any.
    iommu: Option<VmIommuProfile>,
    balloon: VmBalloonProfile,
}

/// The aarch64 platform is device-tree only: the kernel reads the GIC
/// register windows and every virtio device's interrupt from the FDT,
/// and the EDK2 build QEMU ships installs the FDT configuration table
/// only when it is not also publishing ACPI tables.
const AARCH64_VIRT_MACHINE: &str = "virt,gic-version=3,acpi=off";

const AARCH64_VIRT_HVF_PROFILE: VmProfile = VmProfile {
    arch: VmArch::Aarch64,
    qemu_bin: DEFAULT_AARCH64_QEMU_BIN,
    cargo_target: "aarch64-unknown-none",
    machine: AARCH64_VIRT_MACHINE,
    kernel_artifact_name: "helios",
    default_smp: DEFAULT_AARCH64_SMP,
    default_memory: DEFAULT_AARCH64_MEMORY,
    default_bios: None,
    default_accel: &["hvf", "kvm"],
    default_cpu: Some("host"),
    boot_artifact: VmBootArtifactKind::LimineUefiDiskImage,
    console: VmConsoleProfile::SerialUnixSocket,
    network: Some(VmNetworkProfile::VirtioMmio),
    boot_disk: Some(VmBootDiskProfile::VirtioPci),
    data_disk: VmDataDiskProfile::VirtioMmio,
    host_share: Some(VmHostShareProfile::Virtio9pMmio),
    watchdog: Some(VmWatchdogProfile::I6300Esb),
    entropy: VmEntropyProfile::VirtioRngMmio,
    // The virtio devices of the `virt` machines are memory-mapped, and
    // virtio-iommu translates PCI endpoints only, so there is nothing
    // here it could confine.
    iommu: None,
    balloon: VmBalloonProfile::VirtioBalloonMmio,
};

#[cfg(test)]
const AARCH64_VIRT_TCG_PROFILE: VmProfile = VmProfile {
    arch: VmArch::Aarch64,
    qemu_bin: DEFAULT_AARCH64_QEMU_BIN,
    cargo_target: "aarch64-unknown-none",
    machine: AARCH64_VIRT_MACHINE,
    kernel_artifact_name: "helios",
    default_smp: DEFAULT_AARCH64_SMP,
    default_memory: DEFAULT_AARCH64_MEMORY,
    default_bios: None,
    default_accel: &["tcg"],
    default_cpu: Some("max"),
    boot_artifact: VmBootArtifactKind::LimineUefiDiskImage,
    console: VmConsoleProfile::SerialUnixSocket,
    network: Some(VmNetworkProfile::VirtioMmio),
    boot_disk: Some(VmBootDiskProfile::VirtioPci),
    data_disk: VmDataDiskProfile::VirtioMmio,
    host_share: Some(VmHostShareProfile::Virtio9pMmio),
    watchdog: Some(VmWatchdogProfile::I6300Esb),
    entropy: VmEntropyProfile::VirtioRngMmio,
    // The virtio devices of the `virt` machines are memory-mapped, and
    // virtio-iommu translates PCI endpoints only, so there is nothing
    // here it could confine.
    iommu: None,
    balloon: VmBalloonProfile::VirtioBalloonMmio,
};

const RISCV64_VM_PROFILE: VmProfile = VmProfile {
    arch: VmArch::Riscv64,
    qemu_bin: DEFAULT_RISCV_QEMU_BIN,
    cargo_target: "riscv64gc-unknown-none-elf",
    machine: "virt",
    kernel_artifact_name: "helios",
    default_smp: DEFAULT_RISCV_SMP,
    default_memory: DEFAULT_RISCV_MEMORY,
    default_bios: Some("default"),
    default_accel: &[],
    default_cpu: None,
    boot_artifact: VmBootArtifactKind::KernelBinary,
    console: VmConsoleProfile::SerialUnixSocket,
    network: Some(VmNetworkProfile::VirtioMmio),
    boot_disk: None,
    data_disk: VmDataDiskProfile::VirtioMmio,
    host_share: Some(VmHostShareProfile::Virtio9pMmio),
    watchdog: Some(VmWatchdogProfile::I6300Esb),
    entropy: VmEntropyProfile::VirtioRngMmio,
    // The virtio devices of the `virt` machines are memory-mapped, and
    // virtio-iommu translates PCI endpoints only, so there is nothing
    // here it could confine.
    iommu: None,
    balloon: VmBalloonProfile::VirtioBalloonMmio,
};

const X86_64_VM_PROFILE: VmProfile = VmProfile {
    arch: VmArch::X86_64,
    qemu_bin: DEFAULT_X86_QEMU_BIN,
    cargo_target: "x86_64-unknown-none",
    machine: "q35",
    kernel_artifact_name: "helios",
    default_smp: DEFAULT_X86_SMP,
    default_memory: DEFAULT_X86_MEMORY,
    default_bios: None,
    default_accel: &["kvm"],
    default_cpu: Some("host"),
    boot_artifact: VmBootArtifactKind::LimineUefiDiskImage,
    console: VmConsoleProfile::SerialUnixSocket,
    network: Some(VmNetworkProfile::VirtioPci),
    boot_disk: Some(VmBootDiskProfile::VirtioPci),
    data_disk: VmDataDiskProfile::VirtioPci,
    host_share: Some(VmHostShareProfile::Virtio9pPci),
    watchdog: Some(VmWatchdogProfile::I6300Esb),
    entropy: VmEntropyProfile::VirtioRngPci,
    iommu: Some(VmIommuProfile::VirtioIommuPci),
    balloon: VmBalloonProfile::VirtioBalloonPci,
};

/// Virtqueue ring layout the inspector asks every virtio device for.
///
/// QEMU offers the packed ring only when the device is created with
/// `packed=on`, so exercising that layout is a property of how the VM is
/// built rather than something the guest can choose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Serialize, Deserialize)]
pub(crate) enum VirtioRingLayout {
    #[default]
    Split,
    Packed,
}

/// Whether the inspector asks every virtio device to use buffers in the
/// order the driver made them available.
///
/// QEMU offers VIRTIO_F_IN_ORDER only when the device is created with
/// `in_order=on`, so like the ring layout this is a property of how the
/// VM is built rather than something the guest can choose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Serialize, Deserialize)]
pub(crate) enum VirtioCompletionOrder {
    #[default]
    Unordered,
    InOrder,
}

/// Whether the machine's virtio-PCI devices sit behind its
/// virtio-iommu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum VirtioPlatformAccess {
    /// The device issues physical addresses.
    #[default]
    Direct,
    /// The device is an endpoint of the machine's translation unit and
    /// issues addresses that unit translates.
    Confined,
}

/// The behaviour every virtio device the inspector creates is asked for:
/// its virtqueue layout, and whether its DMA goes through the machine's
/// translation unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct VirtioDeviceProfile {
    ring: VirtioRingLayout,
    completion: VirtioCompletionOrder,
    access: VirtioPlatformAccess,
}

impl VirtioDeviceProfile {
    /// Adds the device properties that select this profile to a device
    /// option list.
    fn apply(self, options: &mut QemuOptions) {
        if self.ring == VirtioRingLayout::Packed {
            options.set("packed", "on");
        }
        if self.completion == VirtioCompletionOrder::InOrder {
            options.set("in_order", "on");
        }
    }

    /// As [`Self::apply`], for a device on the PCI bus the machine's
    /// translation unit protects.
    ///
    /// Only a PCI endpoint can be confined, and only a
    /// non-transitional function offers VIRTIO_F_ACCESS_PLATFORM at
    /// all, so a confined device is created with the legacy interface
    /// disabled.
    pub(crate) fn apply_pci(self, options: &mut QemuOptions) {
        self.apply(options);
        if self.access == VirtioPlatformAccess::Confined {
            options.set("disable-legacy", "on");
            options.set("iommu_platform", "on");
        }
    }
}

/// How a machine profile attaches its translation unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VmIommuProfile {
    /// The virtio-iommu function of a PCI machine. virtio-iommu can
    /// only protect PCI endpoints, so a machine whose virtio devices are
    /// memory-mapped has no profile here at all.
    VirtioIommuPci,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct VmConfigFile {
    #[serde(default)]
    pub(crate) arch: Option<VmArch>,
    #[serde(default)]
    pub(crate) debug: Option<bool>,
    #[serde(default)]
    pub(crate) release: Option<bool>,
    #[serde(default)]
    pub(crate) kernel_debug: Option<bool>,
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
    pub(crate) cpu: Option<String>,
    #[serde(default)]
    pub(crate) accel: Vec<String>,
    #[serde(default)]
    pub(crate) shared_dir: Option<PathBuf>,
    #[serde(default)]
    pub(crate) gdb: Option<String>,
    #[serde(default)]
    pub(crate) gdb_wait: Option<bool>,
    #[serde(default)]
    pub(crate) monitor: Option<String>,
    #[serde(default)]
    pub(crate) qmp: Option<String>,
    #[serde(default)]
    pub(crate) qemu_log: Option<PathBuf>,
    #[serde(default)]
    pub(crate) qemu_trace: Vec<String>,
    #[serde(default)]
    pub(crate) qemu_trace_log: Option<PathBuf>,
    #[serde(default)]
    pub(crate) qemu_arg: Vec<String>,
    #[serde(default)]
    pub(crate) boot_programs: Vec<String>,
    #[serde(default)]
    pub(crate) no_compiler_plugin: Option<bool>,
    #[serde(default)]
    pub(crate) runtime_dir: Option<PathBuf>,
    #[serde(default)]
    pub(crate) keep_runtime_dir: Option<bool>,
    #[serde(default)]
    pub(crate) data_disk_size: Option<u64>,
    #[serde(default)]
    pub(crate) iommu: Option<bool>,
    #[serde(default)]
    pub(crate) virtio_packed: Option<bool>,
    #[serde(default)]
    pub(crate) virtio_in_order: Option<bool>,
    #[serde(default)]
    pub(crate) network: VmNetworkFile,
}

#[derive(Debug, ClapArgs)]
pub(crate) struct VmCommand {
    #[arg(long, value_enum, default_value_t = VmArch::Riscv64)]
    arch: VmArch,

    /// Enable a practical kernel debugging preset.
    #[arg(long, default_value_t = false, conflicts_with = "release")]
    debug: bool,

    #[arg(long, default_value_t = false)]
    release: bool,

    /// Build the kernel with debuginfo and unstripped symbols for GDB/LLDB.
    #[arg(long, default_value_t = false, conflicts_with = "release")]
    kernel_debug: bool,

    #[arg(long)]
    config: Option<PathBuf>,

    #[arg(long)]
    qemu_bin: Option<PathBuf>,

    #[arg(long)]
    kernel: Option<PathBuf>,

    #[arg(long)]
    socket: Option<PathBuf>,

    /// Use QEMU stdin/stdout pipes as the debug serial transport.
    #[arg(long, default_value_t = false, conflicts_with_all = ["socket", "serial_pty"])]
    serial_stdio: bool,

    /// Use an inspector-created PTY as the debug serial transport.
    #[arg(long, default_value_t = false, conflicts_with_all = ["socket", "serial_stdio"])]
    serial_pty: bool,

    #[arg(long, default_value_t = false)]
    no_build: bool,

    #[arg(long)]
    smp: Option<u16>,

    #[arg(long)]
    memory: Option<String>,

    #[arg(long)]
    bios: Option<String>,

    #[arg(long, default_value_t = DEFAULT_BAUD)]
    baud: u32,

    /// QEMU CPU model override.
    #[arg(long)]
    cpu: Option<String>,

    /// QEMU accelerator option. Repeat to pass multiple `-accel` entries.
    #[arg(long)]
    accel: Vec<String>,

    #[arg(long)]
    shared_dir: Option<PathBuf>,

    /// Size in bytes of the scratch disk every profile attaches.
    #[arg(long)]
    data_disk_size: Option<u64>,

    #[arg(long)]
    gdb: Option<String>,

    /// Start QEMU with CPUs stopped so GDB/LLDB can attach before kernel entry.
    #[arg(long, default_value_t = false)]
    gdb_wait: bool,

    /// QEMU HMP monitor endpoint, for example `stdio` or `unix:/tmp/hmp.sock,server=on,wait=off`.
    #[arg(long)]
    monitor: Option<String>,

    /// QEMU QMP endpoint, for example `unix:/tmp/qmp.sock,server=on,wait=off`.
    #[arg(long)]
    qmp: Option<String>,

    /// File receiving QEMU stdout/stderr.
    #[arg(long)]
    qemu_log: Option<PathBuf>,

    /// QEMU `-d` trace flags. Repeat the flag or pass comma-separated groups.
    #[arg(long, value_delimiter = ',')]
    qemu_trace: Vec<String>,

    /// File receiving QEMU `-d` trace output.
    #[arg(long)]
    qemu_trace_log: Option<PathBuf>,

    /// Extra raw QEMU argument. Repeat for multiple arguments.
    #[arg(long, allow_hyphen_values = true)]
    qemu_arg: Vec<String>,

    /// Restrict bootfs program prebuilds to a named program. Repeat for multiple programs.
    #[arg(long = "boot-program")]
    boot_programs: Vec<String>,

    /// Omit the compiler kernel plugin from bootfs.
    #[arg(long, default_value_t = false)]
    no_compiler_plugin: bool,

    /// Directory for VM sockets, logs, and generated boot artifacts.
    #[arg(long)]
    runtime_dir: Option<PathBuf>,

    /// Keep the generated VM runtime directory after QEMU exits.
    #[arg(long, default_value_t = false)]
    keep_runtime_dir: bool,

    /// Put every virtio device behind a virtio-iommu, so its DMA only
    /// reaches the memory the kernel maps into its domain.
    #[arg(long, default_value_t = false)]
    iommu: bool,

    /// Create every virtio device with the packed virtqueue layout.
    #[arg(long, default_value_t = false)]
    virtio_packed: bool,

    /// Offer VIRTIO_F_IN_ORDER on every virtio device.
    #[arg(long, default_value_t = false)]
    virtio_in_order: bool,

    #[command(flatten)]
    network: VmNetworkArgs,

    #[command(subcommand)]
    command: Option<VmSessionCommand>,
}

#[derive(Debug, Subcommand)]
enum VmSessionCommand {
    Shell(crate::ShellCommand),
    Tracing(crate::TracingCommand),
    Stats,
    Repl,
    AotBench(AotBenchCommand),
    WorkloadBench(WorkloadBenchCommand),
    /// Move the guest's memory balloon and watch the guest follow.
    Balloon(BalloonCommand),
    /// Provision the privileged host state a network backend needs.
    NetSetup(NetSetupCommand),
    /// Remove the host state `net-setup` provisioned.
    NetTeardown(NetTeardownCommand),
}

/// Drives the QMP `balloon` command against the running guest.
///
/// The host names the memory it wants the guest to keep; the guest gives
/// back the difference and reports what it managed. Naming several
/// targets walks the balloon through them in order, which is how the
/// return path — the guest taking its memory back — is exercised without
/// a second boot.
#[derive(Debug, Clone, ClapArgs)]
pub(crate) struct BalloonCommand {
    /// Guest memory size to ask for, QEMU-style (`1536M`, `2G`). Repeat
    /// to move the balloon through several targets. With none, the
    /// current state is printed and nothing is moved.
    targets: Vec<String>,

    /// How long to wait for the guest to reach each target.
    #[arg(long, default_value_t = 30)]
    settle_seconds: u64,

    /// How long to hold each target before moving to the next one.
    #[arg(long, default_value_t = 0)]
    hold_seconds: u64,
}

#[derive(Debug, Clone, ClapArgs)]
struct AotBenchCommand {
    /// Local raw wasm component path to upload and execute inside the guest.
    wasm: PathBuf,

    /// Guest path used for the uploaded wasm sample.
    #[arg(long, default_value = "/aot-bench-input.wasm")]
    remote_path: String,

    /// Guest path used for the generated signed cwasm artifact.
    #[arg(long, default_value = "/aot-bench-output.cwasm")]
    destination_path: String,

    /// Number of AOT+exec iterations to run after upload.
    #[arg(long, default_value_t = 1)]
    iterations: u16,

    /// Ask the guest compiler plugin to emit Wasmtime/Cranelift timing diagnostics.
    #[arg(long, default_value_t = false)]
    compiler_timing: bool,

    /// Write folded kernel/user profile samples collected during the AOT run.
    #[arg(long)]
    profile_output: Option<PathBuf>,

    /// Write folded kernel-only profile samples collected during the AOT run.
    #[arg(long)]
    kernel_profile_output: Option<PathBuf>,

    /// Write folded user-only profile samples collected during the AOT run.
    #[arg(long)]
    user_profile_output: Option<PathBuf>,

    /// Write structured kernel/user perf metrics collected during the AOT run.
    #[arg(long)]
    perf_metrics_output: Option<PathBuf>,
}

#[derive(Debug)]
struct ResolvedVmCommand {
    profile: &'static VmProfile,
    release: bool,
    kernel_debug: bool,
    qemu_bin: PathBuf,
    kernel: PathBuf,
    socket: Option<PathBuf>,
    serial_stdio: bool,
    serial_pty: bool,
    no_build: bool,
    smp: u16,
    memory: String,
    bios: Option<String>,
    baud: u32,
    cpu: Option<String>,
    accel: Vec<String>,
    shared_dir: Option<PathBuf>,
    data_disk_bytes: u64,
    gdb: Option<String>,
    gdb_wait: bool,
    monitor: Option<String>,
    qmp: Option<String>,
    qemu_log: Option<PathBuf>,
    qemu_trace: Vec<String>,
    qemu_trace_log: Option<PathBuf>,
    qemu_arg: Vec<String>,
    boot_programs: Vec<String>,
    no_compiler_plugin: bool,
    runtime_dir: Option<PathBuf>,
    keep_runtime_dir: bool,
    iommu: bool,
    virtio_devices: VirtioDeviceProfile,
    network: VmNetwork,
    qemu_net: Option<QemuNetArgs>,
    command: Option<ResolvedVmSessionCommand>,
    /// Whether the session command drives QEMU's machine protocol and
    /// therefore needs a socket even when the runtime directory is not
    /// being kept.
    needs_qmp: bool,
}

#[derive(Debug, Clone)]
enum ResolvedVmSessionCommand {
    Session(SessionCommand),
    AotBench(AotBenchCommand),
    WorkloadBench(WorkloadBenchCommand),
    Balloon(BalloonCommand),
}

pub(crate) fn run(mut command: VmCommand) -> Result<()> {
    // The privileged network helpers provision the host, they do not boot
    // a guest, so they are dispatched before any build or QEMU work.
    match command.command.take() {
        Some(VmSessionCommand::NetSetup(setup)) => return Ok(network::run_setup(setup)?),
        Some(VmSessionCommand::NetTeardown(teardown)) => {
            return Ok(network::run_teardown(teardown)?);
        }
        session => command.command = session,
    }
    let command = resolve(command)?;
    ensure_qemu_command(&command)?;
    if !command.no_build {
        build_vm(&command)?;
    }
    let mut runtime = VmRuntime::spawn(&command)?;
    let result = connect_and_run(&command, &mut runtime);
    let runtime_dir = runtime.runtime_dir_path().to_path_buf();
    runtime.shutdown();
    result.with_context(|| format!("VM runtime directory: {}", runtime_dir.display()))
}

fn resolve(command: VmCommand) -> Result<ResolvedVmCommand> {
    let file = load_config_file(command.config.as_deref())?;
    let arch = file.arch.unwrap_or(command.arch);
    let profile = arch.profile();
    let debug = command.debug || file.debug.unwrap_or(false);
    let release = command.release || file.release.unwrap_or(false);
    let kernel_debug = debug || command.kernel_debug || file.kernel_debug.unwrap_or(false);
    if release && kernel_debug {
        bail!("--release and --kernel-debug cannot be used together");
    }
    let qemu_bin = command
        .qemu_bin
        .or(file.qemu_bin)
        .unwrap_or_else(|| PathBuf::from(profile.qemu_bin));
    let kernel = command
        .kernel
        .or(file.kernel)
        .unwrap_or_else(|| default_kernel_path(arch, build_profile_dir(release, kernel_debug)));
    let smp = command.smp.or(file.smp).unwrap_or(profile.default_smp);
    let memory = command
        .memory
        .or(file.memory)
        .unwrap_or_else(|| profile.default_memory.to_owned());
    let bios = command
        .bios
        .or(file.bios)
        .or_else(|| profile.default_bios.map(str::to_owned));
    let baud = if command.baud != DEFAULT_BAUD {
        command.baud
    } else {
        file.baud.unwrap_or(DEFAULT_BAUD)
    };
    let mut accel = if file.accel.is_empty() && command.accel.is_empty() {
        default_accel(profile)
    } else {
        file.accel
    };
    accel.extend(command.accel);
    let cpu = command
        .cpu
        .or(file.cpu)
        .or_else(|| default_cpu(profile, &accel).map(str::to_owned));
    let shared_dir = command.shared_dir.or(file.shared_dir);
    let data_disk_bytes = command
        .data_disk_size
        .or(file.data_disk_size)
        .unwrap_or(DEFAULT_DATA_DISK_BYTES);
    if data_disk_bytes == 0 {
        bail!("--data-disk-size must be greater than zero");
    }
    let gdb = command
        .gdb
        .or(file.gdb)
        .or_else(|| debug.then(|| DEFAULT_GDB_ENDPOINT.to_owned()));
    let gdb_wait = debug || command.gdb_wait || file.gdb_wait.unwrap_or(false);
    if gdb_wait && gdb.is_none() {
        bail!("--gdb-wait requires --gdb or --debug");
    }
    let monitor = command.monitor.or(file.monitor);
    if command.serial_stdio && matches!(monitor.as_deref(), Some("stdio")) {
        bail!("--serial-stdio cannot share stdio with --monitor stdio");
    }
    let qmp = command.qmp.or(file.qmp);
    let qemu_log = command.qemu_log.or(file.qemu_log);
    let mut qemu_trace = file.qemu_trace;
    qemu_trace.extend(command.qemu_trace);
    let qemu_trace_log = command.qemu_trace_log.or(file.qemu_trace_log);
    let mut qemu_arg = file.qemu_arg;
    qemu_arg.extend(command.qemu_arg);
    let session_command = command.command.map(Into::into);
    let mut boot_programs = file.boot_programs;
    boot_programs.extend(command.boot_programs);
    if matches!(
        &session_command,
        Some(ResolvedVmSessionCommand::WorkloadBench(_))
    ) {
        let Some(ResolvedVmSessionCommand::WorkloadBench(command)) = &session_command else {
            unreachable!("workload-bench session command was already matched")
        };
        let required = crate::workload_bench::required_boot_programs(command)?;
        extend_unique_boot_programs(
            &mut boot_programs,
            &required.iter().map(String::as_str).collect::<Vec<_>>(),
        );
    }
    let no_compiler_plugin = command.no_compiler_plugin || file.no_compiler_plugin.unwrap_or(false);
    let runtime_dir = command.runtime_dir.or(file.runtime_dir);
    let keep_runtime_dir =
        debug || command.keep_runtime_dir || file.keep_runtime_dir.unwrap_or(false);
    let iommu = command.iommu || file.iommu.unwrap_or(false);
    if iommu && profile.iommu.is_none() {
        bail!(
            "--iommu is not available on {}: its virtio devices are memory-mapped, and \
             virtio-iommu can only confine PCI endpoints",
            arch_label(arch)
        );
    }
    let virtio_devices = VirtioDeviceProfile {
        ring: if command.virtio_packed || file.virtio_packed.unwrap_or(false) {
            VirtioRingLayout::Packed
        } else {
            VirtioRingLayout::Split
        },
        completion: if command.virtio_in_order || file.virtio_in_order.unwrap_or(false) {
            VirtioCompletionOrder::InOrder
        } else {
            VirtioCompletionOrder::Unordered
        },
        access: if iommu {
            VirtioPlatformAccess::Confined
        } else {
            VirtioPlatformAccess::Direct
        },
    };

    let network = VmNetwork::resolve(command.network, file.network);
    let qemu_net = profile
        .network
        .map(|device| network.render(device, virtio_devices, smp, HostPlatform::current()))
        .transpose()?;

    Ok(ResolvedVmCommand {
        profile,
        release,
        kernel_debug,
        qemu_bin,
        kernel,
        socket: command.socket,
        serial_stdio: command.serial_stdio,
        serial_pty: command.serial_pty,
        no_build: command.no_build,
        smp,
        memory,
        bios,
        baud,
        cpu,
        accel,
        shared_dir,
        data_disk_bytes,
        gdb,
        gdb_wait,
        monitor,
        qmp,
        qemu_log,
        qemu_trace,
        qemu_trace_log,
        qemu_arg,
        boot_programs,
        no_compiler_plugin,
        runtime_dir,
        keep_runtime_dir,
        iommu,
        virtio_devices,
        network,
        qemu_net,
        needs_qmp: matches!(session_command, Some(ResolvedVmSessionCommand::Balloon(_))),
        command: session_command,
    })
}

fn extend_unique_boot_programs(programs: &mut Vec<String>, required: &[&str]) {
    for program in required {
        if !programs.iter().any(|existing| existing == program) {
            programs.push((*program).to_owned());
        }
    }
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

/// Picks the profile's accelerator by host capability when the user gave
/// none: the profile's native accelerator (HVF/KVM) only when this host
/// can actually provide it, otherwise TCG. Profiles with no default
/// accelerator keep QEMU's own default.
fn default_accel(profile: &VmProfile) -> Vec<String> {
    let native_host = match profile.arch {
        VmArch::Aarch64 => std::env::consts::ARCH == "aarch64",
        VmArch::X86_64 => std::env::consts::ARCH == "x86_64",
        VmArch::Riscv64 => false,
    };
    for accel in profile.default_accel {
        let available = match *accel {
            // The OS check alone is not enough: virtualized macOS hosts
            // without nested virtualization (e.g. CI runners) ship the
            // Hypervisor framework but report kern.hv_support=0, and QEMU
            // aborts with HV_UNSUPPORTED if HVF is requested anyway.
            "hvf" => {
                native_host
                    && std::env::consts::OS == "macos"
                    && std::process::Command::new("sysctl")
                        .args(["-n", "kern.hv_support"])
                        .output()
                        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "1")
            }
            "kvm" => native_host && Path::new("/dev/kvm").exists(),
            _ => true,
        };
        if available {
            return vec![(*accel).to_owned()];
        }
    }
    if profile.default_accel.is_empty() {
        Vec::new()
    } else {
        vec!["tcg".to_owned()]
    }
}

/// `-cpu host` is only valid under a native accelerator; TCG (or QEMU's
/// default accelerator) needs the emulated `max` model instead.
fn default_cpu(profile: &VmProfile, accel: &[String]) -> Option<&'static str> {
    let native = accel.iter().any(|value| value == "hvf" || value == "kvm");
    match profile.default_cpu {
        Some("host") if !native => Some("max"),
        other => other,
    }
}

fn ensure_qemu_command(command: &ResolvedVmCommand) -> Result<()> {
    if let Some(shared_dir) = &command.shared_dir {
        if !shared_dir.is_dir() {
            bail!("shared directory does not exist: {}", shared_dir.display());
        }
    }
    if command.profile.arch == VmArch::Aarch64
        && command.accel.iter().any(|accel| accel == "hvf")
        && std::env::consts::ARCH != "aarch64"
    {
        bail!("aarch64-virt-hvf requires an aarch64 host; pass --accel tcg explicitly for TCG");
    }
    // Host state the selected backend depends on is checked before the
    // kernel build, so a missing tap costs seconds rather than a rebuild.
    if let Some(qemu_net) = &command.qemu_net {
        command
            .network
            .preflight(&command.qemu_bin, qemu_net.queue_pairs())?;
    }
    Ok(())
}

fn build_vm(command: &ResolvedVmCommand) -> Result<()> {
    let repo_root = repo_root();
    run_step(
        "building helios-cli",
        cargo_build_command(repo_root, command.release, false)
            .arg("-p")
            .arg("helios-cli"),
    )?;
    let prebuild_manifest = run_kernel_prebuild(command)?;
    run_step(
        &format!("building {} kernel", arch_label(command.profile.arch)),
        cargo_build_command(repo_root, command.release, command.kernel_debug)
            .env("HELIOS_KERNEL_PREBUILD_MANIFEST", &prebuild_manifest)
            .arg("--target")
            .arg(command.profile.cargo_target)
            .arg("--bin")
            .arg(command.profile.kernel_artifact_name),
    )?;
    run_step(
        "building inspector",
        cargo_build_command(repo_root, command.release, false)
            .arg("-p")
            .arg("helios-inspector"),
    )?;
    Ok(())
}

fn cargo_build_command(repo_root: &Path, release: bool, kernel_debug: bool) -> Command {
    let mut command = Command::new("cargo");
    command.current_dir(repo_root).arg("build");
    if release {
        command.arg("--release");
    } else if kernel_debug {
        command.arg("--profile").arg("kernel-debug");
    }
    command
}

fn run_kernel_prebuild(command: &ResolvedVmCommand) -> Result<PathBuf> {
    let cli = discover_helios_cli()?;
    let out_dir = repo_root()
        .join("target")
        .join("kernel-prebuild")
        .join(command.profile.cargo_target)
        .join(build_profile_dir(command.release, command.kernel_debug));
    let mut prebuild = Command::new(&cli);
    prebuild
        .current_dir(repo_root())
        .arg("kernel-prebuild")
        .arg("--out-dir")
        .arg(&out_dir)
        .arg("--target")
        .arg(command.profile.cargo_target)
        .arg("--profile")
        .arg(build_profile_dir(command.release, command.kernel_debug))
        .arg("--cargo")
        .arg("cargo");
    for program in &command.boot_programs {
        prebuild.arg("--boot-program").arg(program);
    }
    if command.no_compiler_plugin {
        prebuild.arg("--no-compiler-plugin");
    }
    run_step("prebuilding kernel bootfs", &mut prebuild)?;
    Ok(out_dir.join("kernel-prebuild.json"))
}

fn connect_and_run(command: &ResolvedVmCommand, runtime: &mut VmRuntime) -> Result<()> {
    let qmp_socket = runtime.qmp_socket.clone();
    let client = match runtime.take_transport()? {
        VmTransport::SerialSocket(socket_path) => {
            let socket = socket_path.to_str().ok_or_else(|| {
                anyhow::anyhow!("socket path must be valid UTF-8: {}", socket_path.display())
            })?;
            connect_client(socket, command.baud, true)
                .context("failed to connect inspector RPC client")?
        }
        VmTransport::SerialIo(io) => crate::runtime::block_on(async move {
            crate::ready::connect_after_boot(io)
                .await
                .context("failed to connect inspector RPC client over QEMU stdio serial")
        })?,
    };
    match command.command.clone() {
        Some(ResolvedVmSessionCommand::AotBench(command)) => run_aot_bench(client, command),
        Some(ResolvedVmSessionCommand::WorkloadBench(workload_command)) => run_workload_bench(
            client,
            workload_command,
            VmProvenance {
                arch: arch_label(command.profile.arch),
                release: command.release,
                smp: command.smp,
                memory: command.memory.clone(),
                cpu: command.cpu.clone(),
                accel: command.accel.clone(),
            },
        ),
        Some(ResolvedVmSessionCommand::Balloon(balloon)) => {
            let socket = qmp_socket.context(
                "the balloon command needs a QMP socket; pass --qmp unix:<path>,server=on,wait=off",
            )?;
            run_balloon(client, balloon, &socket)
        }
        Some(ResolvedVmSessionCommand::Session(command)) => run_connected(client, Some(command)),
        None => run_connected(client, None),
    }
}

/// Moves the guest's balloon through the targets the caller named,
/// reporting what the host asked for and what the guest gave up after
/// each move.
///
/// Both sides are printed because they answer different questions: QEMU
/// says how much guest memory it is still backing, the guest's own
/// `helios:system/stats` says how much of its user memory the balloon is
/// holding and how much it has named as free.
fn run_balloon(
    mut client: crate::serial::RpcClient,
    command: BalloonCommand,
    qmp_socket: &Path,
) -> Result<()> {
    let mut qmp = QmpClient::connect(qmp_socket)?;
    report_balloon(&mut qmp, &mut client, "initial")?;

    for target in &command.targets {
        let bytes = qmp::parse_size(target)?;
        println!("{} balloon target {target}", style("set").cyan());
        qmp.set_balloon(bytes)
            .with_context(|| format!("failed to set the balloon target to {target}"))?;
        settle_balloon(&mut client, bytes, command.settle_seconds)?;
        report_balloon(&mut qmp, &mut client, target)?;
        if command.hold_seconds != 0 {
            std::thread::sleep(Duration::from_secs(command.hold_seconds));
            report_balloon(&mut qmp, &mut client, &format!("{target} after hold"))?;
        }
    }
    Ok(())
}

/// Waits for the guest to settle on the target the host named.
///
/// The guest is asked rather than QEMU: it is the side that decides how
/// much it can spare, it publishes that decision through
/// `helios:system/stats`, and its answer arrives over the debug serial
/// rather than over the monitor — which the very memory work the guest
/// is doing keeps busy.
///
/// A guest that stops short is not an error. The kernel refuses to
/// inflate past its own pressure floor and reports the truth, so the
/// wait ends when the guest stops moving and the caller sees where it
/// stopped.
fn settle_balloon(client: &mut crate::serial::RpcClient, target: u64, seconds: u64) -> Result<()> {
    let started = std::time::Instant::now();
    let deadline = started + Duration::from_secs(seconds);
    let mut previous = None;
    let mut still_since = started;
    loop {
        // A guest that is handing memory back may be too busy to answer
        // for a while — a target move on an emulated machine is a lot of
        // work. Not answering is not the same as having stopped, so it
        // does not end the wait or reset the stillness clock.
        let Some(sample) = guest_stats(client) else {
            if std::time::Instant::now() >= deadline {
                println!(
                    "{} guest stopped answering before the {seconds}s wait ran out",
                    style("settled").yellow()
                );
                return Ok(());
            }
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };
        let actual = sample.balloon.as_ref().map(|balloon| balloon.actual_bytes);
        if actual == Some(target) {
            println!(
                "{} guest reached the target after {:.1}s",
                style("settled").green(),
                started.elapsed().as_secs_f64()
            );
            return Ok(());
        }
        if actual != previous {
            previous = actual;
            still_since = std::time::Instant::now();
        } else if still_since.elapsed() >= BALLOON_STILL_FOR {
            println!(
                "{} guest stopped at {} after {:.1}s",
                style("settled").yellow(),
                actual.map_or_else(|| "no balloon".to_owned(), format_bytes),
                started.elapsed().as_secs_f64()
            );
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            println!(
                "{} guest was still moving when the {seconds}s wait ran out",
                style("settled").yellow()
            );
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Reads the guest's own view of its memory, or nothing when the guest
/// is too busy to answer right now.
fn guest_stats(
    client: &mut crate::serial::RpcClient,
) -> Option<helios_inspector_protocol::system::stats::Sample> {
    crate::runtime::block_on(crate::system::fetch_stats(client)).ok()
}

fn report_balloon(
    qmp: &mut QmpClient,
    client: &mut crate::serial::RpcClient,
    label: &str,
) -> Result<()> {
    let Some(sample) = guest_stats(client) else {
        println!(
            "{} {label}: the guest did not answer",
            style("balloon").yellow()
        );
        return Ok(());
    };
    let guest = match &sample.balloon {
        Some(balloon) => format!(
            "guest target={} actual={} reported-free={}",
            format_bytes(balloon.target_bytes),
            format_bytes(balloon.actual_bytes),
            format_bytes(balloon.reported_bytes)
        ),
        None => "guest reports no balloon".to_owned(),
    };
    // QEMU's own view is a cross-check, and the monitor competes for the
    // lock with the memory work the guest is doing, so failing to get it
    // is worth saying rather than worth aborting over.
    let host = match qmp.query_balloon() {
        Ok(info) => format!("qemu-backed={}", format_bytes(info.actual)),
        Err(error) => format!("qemu-backed=unavailable ({error})"),
    };
    println!(
        "{} {label}: {guest} {host} user-memory-available={}",
        style("balloon").green(),
        format_bytes(sample.memory.available_bytes)
    );
    Ok(())
}

fn run_workload_bench(
    mut client: crate::serial::RpcClient,
    command: WorkloadBenchCommand,
    provenance: VmProvenance,
) -> Result<()> {
    crate::run_interruptible(async move {
        let profile_filter = system_profiling::Filter {
            scope: None,
            stack_prefixes: Vec::new(),
        };
        let metric_filter = system_profiling::MetricFilter {
            name_prefixes: Vec::new(),
        };
        let collect_profile = command.profile_output.is_some()
            || command.kernel_profile_output.is_some()
            || command.user_profile_output.is_some()
            || command.perf_metrics_output.is_some();
        let before_profile = if collect_profile {
            system_profiling::clear(&client)
                .await
                .context("failed to clear remote profile samples")?;
            system_profiling::set_enabled(&client, true)
                .await
                .context("failed to enable remote profiling")?;
            system_profiling::folded(&client, &profile_filter, 0)
                .await
                .context("failed to read initial remote profile samples")?
        } else {
            Vec::new()
        };

        if let Err(error) =
            crate::workload_bench::run_inner(&mut client, &command, &provenance).await
        {
            print_recent_guest_errors(&mut client).await;
            return Err(error);
        }

        if collect_profile {
            system_profiling::set_enabled(&client, false)
                .await
                .context("failed to disable remote profiling")?;
            let after_profile = system_profiling::folded(&client, &profile_filter, 0)
                .await
                .context("failed to read final remote profile samples")?;
            let metrics = system_profiling::metrics(&client, &metric_filter, 0)
                .await
                .context("failed to read final remote perf metrics")?;
            write_requested_profile_outputs(&command, &before_profile, &after_profile, &metrics)?;
        }
        Ok(())
    })
}

/// Fetches and prints the guest's recent tracing events (info level and
/// up, so boot markers frame the failure) so a failed remote operation
/// is diagnosable from the CLI without a second tracing session.
async fn print_recent_guest_errors(client: &mut crate::serial::RpcClient) {
    let mut config = crate::system::TracingConfig::new();
    config.limit = 100;
    config.min_level = Some(helios_inspector_protocol::system::tracing::Level::Info);
    match crate::system::fetch_tracing(client, &config).await {
        Ok(events) if events.is_empty() => {}
        Ok(events) => {
            eprintln!("recent guest tracing events:");
            for event in events {
                if let Ok(line) = crate::system::render_tracing_event(&event) {
                    eprintln!("  {line}");
                }
            }
        }
        Err(error) => eprintln!("failed to fetch guest tracing events: {error:#}"),
    }
}

fn run_aot_bench(mut client: crate::serial::RpcClient, command: AotBenchCommand) -> Result<()> {
    crate::run_interruptible(async move {
        let wasm = fs::read(&command.wasm)
            .with_context(|| format!("failed to read {}", command.wasm.display()))?;
        if command.iterations == 0 {
            bail!("aot-bench --iterations must be non-zero");
        }
        debugger_fs::write(&client, &command.remote_path, &wasm, false)
            .await
            .with_context(|| format!("failed to upload {}", command.wasm.display()))?;
        let profile_filter = system_profiling::Filter {
            scope: None,
            stack_prefixes: Vec::new(),
        };
        let metric_filter = system_profiling::MetricFilter {
            name_prefixes: Vec::new(),
        };
        let collect_profile = command.profile_output.is_some()
            || command.kernel_profile_output.is_some()
            || command.user_profile_output.is_some()
            || command.perf_metrics_output.is_some();
        let before_profile = if collect_profile {
            system_profiling::clear(&client)
                .await
                .context("failed to clear remote profile samples")?;
            system_profiling::set_enabled(&client, true)
                .await
                .context("failed to enable remote profiling")?;
            system_profiling::folded(&client, &profile_filter, 0)
                .await
                .context("failed to read initial remote profile samples")?
        } else {
            Vec::new()
        };

        use std::io::Write as _;
        {
            let mut stdout = std::io::stdout().lock();
            writeln!(
                stdout,
                "uploaded {} bytes to {}",
                wasm.len(),
                command.remote_path
            )?;
        }
        for iteration in 1..=command.iterations {
            let started = std::time::Instant::now();
            let outcome = system_programs::aot(
                &client,
                &system_programs::AotRequest {
                    source_path: command.remote_path.clone(),
                    destination_path: command.destination_path.clone(),
                    hint: system_programs::AotHint::Performance,
                    profile: command.compiler_timing,
                },
            )
            .await
            .with_context(|| {
                format!("failed to AOT compile uploaded wasm iteration {iteration}")
            })?;
            let result = match outcome {
                Ok(result) => result,
                Err(error) => {
                    // Surface the guest-side error events before bailing:
                    // the RPC error kind alone (e.g. `Internal`) does not
                    // say which runtime operation actually failed.
                    print_recent_guest_errors(&mut client).await;
                    return Err(anyhow::anyhow!(
                        "remote AOT iteration {iteration} failed: {:?}: {}",
                        error.kind,
                        error.detail
                    ));
                }
            };
            let elapsed = started.elapsed();
            let mut stdout = std::io::stdout().lock();
            writeln!(
                stdout,
                "iteration={iteration} elapsed_ms={} destination_path={}",
                elapsed.as_millis(),
                result.destination_path
            )?;
        }
        if collect_profile {
            system_profiling::set_enabled(&client, false)
                .await
                .context("failed to disable remote profiling")?;
            let after_profile = system_profiling::folded(&client, &profile_filter, 0)
                .await
                .context("failed to read final remote profile samples")?;
            let metrics = system_profiling::metrics(&client, &metric_filter, 0)
                .await
                .context("failed to read final remote perf metrics")?;
            write_requested_profile_outputs(&command, &before_profile, &after_profile, &metrics)?;
        }
        Ok(())
    })
}

trait ProfileOutputRequest {
    fn profile_output(&self) -> Option<&Path>;
    fn kernel_profile_output(&self) -> Option<&Path>;
    fn user_profile_output(&self) -> Option<&Path>;
    fn perf_metrics_output(&self) -> Option<&Path>;
}

impl ProfileOutputRequest for AotBenchCommand {
    fn profile_output(&self) -> Option<&Path> {
        self.profile_output.as_deref()
    }

    fn kernel_profile_output(&self) -> Option<&Path> {
        self.kernel_profile_output.as_deref()
    }

    fn user_profile_output(&self) -> Option<&Path> {
        self.user_profile_output.as_deref()
    }

    fn perf_metrics_output(&self) -> Option<&Path> {
        self.perf_metrics_output.as_deref()
    }
}

impl ProfileOutputRequest for WorkloadBenchCommand {
    fn profile_output(&self) -> Option<&Path> {
        self.profile_output.as_deref()
    }

    fn kernel_profile_output(&self) -> Option<&Path> {
        self.kernel_profile_output.as_deref()
    }

    fn user_profile_output(&self) -> Option<&Path> {
        self.user_profile_output.as_deref()
    }

    fn perf_metrics_output(&self) -> Option<&Path> {
        self.perf_metrics_output.as_deref()
    }
}

fn write_requested_profile_outputs(
    command: &impl ProfileOutputRequest,
    before_profile: &[system_profiling::FoldedSample],
    after_profile: &[system_profiling::FoldedSample],
    metrics: &[system_profiling::MetricSample],
) -> Result<()> {
    use std::io::Write as _;

    if let Some(output) = command.profile_output() {
        write_profile_output(output, before_profile, after_profile, None)
            .with_context(|| format!("failed to write {}", output.display()))?;
        let mut stderr = std::io::stderr().lock();
        writeln!(stderr, "profile_output={}", output.display())?;
    }
    if let Some(output) = command.kernel_profile_output() {
        write_profile_output(
            output,
            before_profile,
            after_profile,
            Some(system_profiling::Scope::Kernel),
        )
        .with_context(|| format!("failed to write {}", output.display()))?;
        let mut stderr = std::io::stderr().lock();
        writeln!(stderr, "kernel_profile_output={}", output.display())?;
    }
    if let Some(output) = command.user_profile_output() {
        write_profile_output(
            output,
            before_profile,
            after_profile,
            Some(system_profiling::Scope::User),
        )
        .with_context(|| format!("failed to write {}", output.display()))?;
        let mut stderr = std::io::stderr().lock();
        writeln!(stderr, "user_profile_output={}", output.display())?;
    }
    if let Some(output) = command.perf_metrics_output() {
        write_perf_metrics_output(output, metrics)
            .with_context(|| format!("failed to write {}", output.display()))?;
        let mut stderr = std::io::stderr().lock();
        writeln!(stderr, "perf_metrics_output={}", output.display())?;
    }
    Ok(())
}

fn write_profile_output(
    output: &Path,
    before: &[system_profiling::FoldedSample],
    after: &[system_profiling::FoldedSample],
    scope: Option<system_profiling::Scope>,
) -> Result<()> {
    fs::write(output, diff_folded_profile(before, after, scope))?;
    Ok(())
}

fn write_perf_metrics_output(
    output: &Path,
    metrics: &[system_profiling::MetricSample],
) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(metrics)?;
    fs::write(output, bytes)?;
    Ok(())
}

#[derive(Template)]
#[template(path = "folded_profile.txt", escape = "none")]
struct FoldedProfileTemplate {
    lines: Vec<FoldedProfileLine>,
}

struct FoldedProfileLine {
    stack: String,
    weight: u64,
}

fn diff_folded_profile(
    before: &[system_profiling::FoldedSample],
    after: &[system_profiling::FoldedSample],
    scope: Option<system_profiling::Scope>,
) -> String {
    let mut lines: Vec<FoldedProfileLine> = after
        .iter()
        .filter(|sample| scope.is_none_or(|scope| sample.scope == scope))
        .filter_map(|sample| {
            let previous = before
                .iter()
                .find(|before| before.scope == sample.scope && before.stack == sample.stack)
                .map(|before| before.weight)
                .unwrap_or(0);
            let weight = sample.weight.saturating_sub(previous);
            (weight != 0).then_some(FoldedProfileLine {
                stack: sample.stack.clone(),
                weight,
            })
        })
        .collect();
    lines.sort_by(|left, right| left.stack.cmp(&right.stack));
    FoldedProfileTemplate { lines }
        .render()
        .expect("folded profile template rendering is infallible")
}

fn prepare_boot_artifact(
    command: &ResolvedVmCommand,
    runtime_dir: Option<&Path>,
) -> Result<PathBuf> {
    match command.profile.boot_artifact {
        VmBootArtifactKind::KernelBinary => Ok(command.kernel.clone()),
        VmBootArtifactKind::LimineUefiDiskImage => prepare_limine_uefi_image(command, runtime_dir),
    }
}

fn prepare_limine_uefi_image(
    command: &ResolvedVmCommand,
    runtime_dir: Option<&Path>,
) -> Result<PathBuf> {
    let kernel = fs::canonicalize(&command.kernel)
        .with_context(|| format!("failed to canonicalize kernel {}", command.kernel.display()))?;
    let image = match runtime_dir {
        Some(dir) => dir.join("kernel.uefi.img"),
        None => kernel.with_extension("uefi.img"),
    };
    let spinner = spinner(&format!(
        "building {} Limine UEFI disk image",
        arch_label(command.profile.arch)
    ));
    let cli = discover_helios_cli()?;
    let status = Command::new(&cli)
        .arg("limine-uefi-image")
        .arg("--kernel")
        .arg(&kernel)
        .arg("--output")
        .arg(&image)
        .arg("--baud")
        .arg(command.baud.to_string())
        .arg("--efi-arch")
        .arg(limine_efi_arch_argument(command.profile.arch))
        .status()
        .with_context(|| format!("failed to spawn {}", cli.display()))?;
    if !status.success() {
        spinner.finish_and_clear();
        bail!("helios-cli limine-uefi-image exited with status {status}");
    }
    spinner.finish_with_message(format!("{} {}", style("built").green(), image.display()));
    Ok(image)
}

fn limine_efi_arch_argument(arch: VmArch) -> &'static str {
    match arch {
        VmArch::Aarch64 => "aarch64",
        VmArch::X86_64 => "x86-64",
        VmArch::Riscv64 => {
            panic!("riscv64 does not use Limine UEFI boot artifacts")
        }
    }
}

fn discover_helios_cli() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("HELIOS_CLI_BIN").map(PathBuf::from) {
        if path.is_file() {
            return Ok(path);
        }
        bail!(
            "HELIOS_CLI_BIN does not point to a file: {}",
            path.display()
        );
    }
    let current_exe = std::env::current_exe().context("failed to locate current executable")?;
    if let Some(candidate) = current_exe.parent().map(|dir| dir.join("helios-cli")) {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    if let Some(candidate) = find_executable_in_path("helios-cli") {
        return Ok(candidate);
    }
    bail!("failed to find helios-cli; run `cargo build -p helios-cli` or set HELIOS_CLI_BIN")
}

fn arch_label(arch: VmArch) -> &'static str {
    match arch {
        VmArch::Aarch64 => "aarch64",
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
    transport: Option<VmTransport>,
    _serial_pty_slave: Option<fs::File>,
    /// The QMP socket the inspector created, when it owns one.
    qmp_socket: Option<PathBuf>,
    runtime_dir: VmRuntimeDir,
    child: Child,
}

enum VmTransport {
    SerialSocket(PathBuf),
    SerialIo(crate::serial::SerialIo),
}

enum VmRuntimeDir {
    Temporary(TempDir),
    Persistent(PathBuf),
}

impl VmRuntimeDir {
    fn create(command: &ResolvedVmCommand) -> Result<Self> {
        if let Some(path) = &command.runtime_dir {
            fs::create_dir_all(path).with_context(|| {
                format!("failed to create VM runtime directory {}", path.display())
            })?;
            return Ok(Self::Persistent(path.clone()));
        }
        if command.keep_runtime_dir {
            let path = default_persistent_runtime_dir()?;
            fs::create_dir_all(&path).with_context(|| {
                format!("failed to create VM runtime directory {}", path.display())
            })?;
            return Ok(Self::Persistent(path));
        }
        tempfile::Builder::new()
            .prefix("helios-inspector-vm.")
            .tempdir()
            .map(Self::Temporary)
            .context("failed to create temporary QEMU runtime directory")
    }

    fn path(&self) -> &Path {
        match self {
            Self::Temporary(tempdir) => tempdir.path(),
            Self::Persistent(path) => path,
        }
    }
}

impl VmRuntime {
    fn spawn(command: &ResolvedVmCommand) -> Result<Self> {
        let runtime_dir = VmRuntimeDir::create(command)?;
        let socket_path = command
            .socket
            .clone()
            .unwrap_or_else(|| runtime_dir.path().join("debug.sock"));
        let qemu_log = command
            .qemu_log
            .clone()
            .unwrap_or_else(|| runtime_dir.path().join("qemu.log"));

        let serial_pty = if command.serial_pty {
            Some(crate::serial::open_pty_transport()?)
        } else {
            None
        };

        if !command.serial_stdio && serial_pty.is_none() {
            prepare_socket_path(&socket_path)?;
        }
        prepare_log_path(&qemu_log)?;
        let artifact = prepare_boot_artifact(command, Some(runtime_dir.path()))?;
        let data_disk = prepare_data_disk(command, runtime_dir.path())?;

        let spinner = spinner(&match &command.qemu_net {
            Some(qemu_net) => format!(
                "starting QEMU for {} with {qemu_net}",
                arch_label(command.profile.arch)
            ),
            None => format!("starting QEMU for {}", arch_label(command.profile.arch)),
        });
        let mut qemu = match &command.qemu_net {
            Some(qemu_net) => qemu_net.command(&command.qemu_bin),
            None => Command::new(&command.qemu_bin),
        };
        qemu.arg("-display").arg("none");
        if let Some(monitor) = monitor_endpoint(command, runtime_dir.path()) {
            qemu.arg("-monitor").arg(monitor);
        } else {
            qemu.arg("-monitor").arg("none");
        }
        if let Some(qmp) = qmp_endpoint(command, runtime_dir.path()) {
            qemu.arg("-qmp").arg(qmp);
        }
        qemu.arg("-machine").arg(command.profile.machine);
        for accel in &command.accel {
            qemu.arg("-accel").arg(accel);
        }
        qemu.arg("-m").arg(&command.memory);
        qemu.arg("-smp").arg(command.smp.to_string());
        if command.serial_stdio {
            qemu.arg("-serial").arg("stdio");
        } else if let Some(serial_pty) = &serial_pty {
            qemu.arg("-serial").arg(&serial_pty.slave_path);
        } else if command.profile.console == VmConsoleProfile::SerialUnixSocket {
            qemu.arg("-serial")
                .arg(format!("unix:{},server=on,wait=on", socket_path.display()));
        }
        if let Some(gdb) = &command.gdb {
            qemu.arg("-gdb").arg(gdb);
            if command.gdb_wait {
                qemu.arg("-S");
            }
        }
        if !command.qemu_trace.is_empty() {
            let trace_log = command
                .qemu_trace_log
                .clone()
                .unwrap_or_else(|| runtime_dir.path().join("qemu-trace.log"));
            prepare_log_path(&trace_log)?;
            qemu.arg("-d").arg(command.qemu_trace.join(","));
            qemu.arg("-D").arg(trace_log);
        }
        if let Some(cpu) = &command.cpu {
            qemu.arg("-cpu").arg(cpu);
        }
        qemu.args(&command.qemu_arg);
        qemu.process_group(0);
        if command.serial_stdio {
            qemu.stdin(Stdio::piped());
            qemu.stdout(Stdio::piped());
        } else {
            qemu.stdin(Stdio::null());
            qemu.stdout(Stdio::from(fs::File::create(&qemu_log).with_context(
                || format!("failed to create {}", qemu_log.display()),
            )?));
        }
        qemu.stderr(Stdio::from(
            fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&qemu_log)
                .with_context(|| format!("failed to open {} for append", qemu_log.display()))?,
        ));
        if matches!(command.profile.arch, VmArch::Aarch64 | VmArch::Riscv64) {
            qemu.arg("-global").arg("virtio-mmio.force-legacy=false");
        }
        configure_firmware(&mut qemu, command, runtime_dir.path())?;
        match command.profile.boot_artifact {
            VmBootArtifactKind::KernelBinary => {
                qemu.arg("-kernel").arg(&artifact);
            }
            VmBootArtifactKind::LimineUefiDiskImage => {}
        }
        // The unit has to be realised before the functions it protects:
        // QEMU binds a PCI function to its address space when the
        // function is created, and one created first would keep the
        // untranslated one.
        if command.iommu {
            configure_iommu(&mut qemu, command.profile.iommu, command.virtio_devices);
        }
        if let Some(qemu_net) = &command.qemu_net {
            qemu_net.apply(&mut qemu);
        }
        if let Some(boot_disk) = command.profile.boot_disk {
            configure_boot_disk(&mut qemu, boot_disk, &artifact, command.virtio_devices);
        }
        configure_data_disk(
            &mut qemu,
            command.profile.data_disk,
            &data_disk,
            command.virtio_devices,
        );
        if let Some(host_share) = command.profile.host_share {
            if let Some(shared_dir) = &command.shared_dir {
                configure_host_share(&mut qemu, host_share, shared_dir, command.virtio_devices);
            }
        }
        if let Some(watchdog) = command.profile.watchdog {
            configure_watchdog(&mut qemu, watchdog);
        }
        configure_entropy_device(&mut qemu, command.profile.entropy, command.virtio_devices);
        configure_balloon(&mut qemu, command.profile.balloon, command.virtio_devices);

        let mut child = qemu.spawn().with_context(|| {
            format!(
                "failed to start QEMU executable {}",
                command.qemu_bin.display()
            )
        })?;
        let mut serial_pty_slave = None;
        let transport = if command.serial_stdio {
            let stdout = child
                .stdout
                .take()
                .context("QEMU stdout pipe was not available for serial stdio")?;
            let stdin = child
                .stdin
                .take()
                .context("QEMU stdin pipe was not available for serial stdio")?;
            VmTransport::SerialIo(crate::serial::open_child_stdio(stdout, stdin)?)
        } else if let Some(serial_pty) = serial_pty {
            serial_pty_slave = Some(serial_pty.slave);
            VmTransport::SerialIo(serial_pty.io)
        } else {
            wait_for_socket(&socket_path, &qemu_log, &mut child)?;
            VmTransport::SerialSocket(socket_path)
        };
        spinner.finish_with_message(format!(
            "{} runtime={} log={}",
            style("ready").green(),
            runtime_dir.path().display(),
            qemu_log.display(),
        ));
        Ok(Self {
            transport: Some(transport),
            _serial_pty_slave: serial_pty_slave,
            qmp_socket: qmp_socket_path(command, runtime_dir.path()),
            runtime_dir,
            child,
        })
    }

    #[cfg(test)]
    fn socket_path(&self) -> &Path {
        match self
            .transport
            .as_ref()
            .expect("VM transport was already taken")
        {
            VmTransport::SerialSocket(socket_path) => socket_path,
            VmTransport::SerialIo(_) => {
                panic!("QEMU stdio serial transport does not have a socket path")
            }
        }
    }

    fn take_transport(&mut self) -> Result<VmTransport> {
        self.transport
            .take()
            .context("VM transport was already taken")
    }

    fn runtime_dir_path(&self) -> &Path {
        self.runtime_dir.path()
    }

    fn shutdown(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn default_persistent_runtime_dir() -> Result<PathBuf> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time is earlier than UNIX_EPOCH")?
        .as_millis();
    Ok(repo_root()
        .join("target")
        .join("inspector-vm")
        .join(format!("run-{}-{timestamp}", std::process::id())))
}

fn monitor_endpoint(command: &ResolvedVmCommand, runtime_dir: &Path) -> Option<String> {
    command.monitor.clone().or_else(|| {
        command
            .keep_runtime_dir
            .then(|| unix_endpoint(runtime_dir, "monitor.sock"))
    })
}

fn qmp_endpoint(command: &ResolvedVmCommand, runtime_dir: &Path) -> Option<String> {
    command.qmp.clone().or_else(|| {
        (command.keep_runtime_dir || command.needs_qmp)
            .then(|| unix_endpoint(runtime_dir, "qmp.sock"))
    })
}

/// The QMP socket the inspector can talk to, when there is one it owns
/// the path of.
///
/// A hand-written `--qmp` endpoint is passed to QEMU verbatim and may
/// name a TCP port or a socket QEMU connects out to, so only the
/// `unix:<path>` form the inspector understands is offered back to the
/// commands that drive QMP themselves.
fn qmp_socket_path(command: &ResolvedVmCommand, runtime_dir: &Path) -> Option<PathBuf> {
    let endpoint = qmp_endpoint(command, runtime_dir)?;
    let path = endpoint.strip_prefix("unix:")?;
    let path = path.split(',').next().unwrap_or(path);
    Some(PathBuf::from(path))
}

fn unix_endpoint(runtime_dir: &Path, name: &str) -> String {
    format!(
        "unix:{},server=on,wait=off",
        runtime_dir.join(name).display()
    )
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

fn prepare_log_path(log_path: &Path) -> Result<()> {
    if let Some(parent) = log_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create log directory {}", parent.display()))?;
    }
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

fn build_profile_dir(release: bool, kernel_debug: bool) -> &'static str {
    if release {
        "release"
    } else if kernel_debug {
        "kernel-debug"
    } else {
        "debug"
    }
}

fn default_kernel_path(arch: VmArch, profile_dir: &str) -> PathBuf {
    let profile = arch.profile();
    repo_root()
        .join("target")
        .join(profile.cargo_target)
        .join(profile_dir)
        .join(profile.kernel_artifact_name)
}

/// Creates the scratch disk image this VM's guest kernel will own.
///
/// It lives in the runtime directory, so every VM gets a disk of its own
/// and a retained runtime directory keeps whatever the guest wrote.
fn prepare_data_disk(command: &ResolvedVmCommand, runtime_dir: &Path) -> Result<PathBuf> {
    let image = runtime_dir.join("data.img");
    let file = fs::File::create(&image)
        .with_context(|| format!("failed to create scratch disk image {}", image.display()))?;
    file.set_len(command.data_disk_bytes)
        .with_context(|| format!("failed to size scratch disk image {}", image.display()))?;
    Ok(image)
}

fn configure_firmware(
    qemu: &mut Command,
    command: &ResolvedVmCommand,
    runtime_dir: &Path,
) -> Result<()> {
    if let Some(bios) = &command.bios {
        qemu.arg("-bios").arg(bios);
        return Ok(());
    }
    if command.profile.boot_artifact == VmBootArtifactKind::LimineUefiDiskImage {
        let code = discover_qemu_edk2_code(command.profile.arch, &command.qemu_bin)?;
        let vars_template = discover_qemu_edk2_vars(command.profile.arch, &code)?;
        let vars = runtime_dir.join(format!("edk2-{}-vars.fd", arch_label(command.profile.arch)));
        fs::copy(&vars_template, &vars).with_context(|| {
            format!(
                "failed to prepare EDK2 variable store {} from {}",
                vars.display(),
                vars_template.display()
            )
        })?;
        qemu.arg("-drive").arg(format!(
            "if=pflash,format=raw,readonly=on,file={}",
            code.display()
        ));
        qemu.arg("-drive")
            .arg(format!("if=pflash,format=raw,file={}", vars.display()));
    }
    Ok(())
}

fn discover_qemu_edk2_code(arch: VmArch, qemu_bin: &Path) -> Result<PathBuf> {
    let env_var = match arch {
        VmArch::Aarch64 => "HELIOS_EDK2_AARCH64_CODE",
        VmArch::X86_64 => "HELIOS_EDK2_X86_64_CODE",
        VmArch::Riscv64 => panic!("riscv64 does not use EDK2 firmware"),
    };
    if let Some(path) = std::env::var_os(env_var).map(PathBuf::from) {
        if path.is_file() {
            return Ok(path);
        }
        bail!("{env_var} does not point to a file: {}", path.display());
    }
    let qemu_bin = if qemu_bin.components().count() == 1 {
        find_executable_in_path(qemu_bin.to_str().unwrap_or("")).unwrap_or_else(|| qemu_bin.into())
    } else {
        qemu_bin.into()
    };
    let mut candidates = Vec::new();
    if let Some(prefix) = qemu_bin.parent().and_then(Path::parent) {
        candidates.push(prefix.join("share/qemu").join(edk2_code_filename(arch)));
    }
    candidates.extend([
        PathBuf::from("/opt/homebrew/share/qemu").join(edk2_code_filename(arch)),
        PathBuf::from("/usr/local/share/qemu").join(edk2_code_filename(arch)),
        PathBuf::from("/usr/share/qemu").join(edk2_code_filename(arch)),
    ]);
    // Debian/Ubuntu package EDK2 as OVMF/AAVMF under their own names.
    match arch {
        VmArch::X86_64 => candidates.extend([
            PathBuf::from("/usr/share/OVMF/OVMF_CODE_4M.fd"),
            PathBuf::from("/usr/share/OVMF/OVMF_CODE.fd"),
        ]),
        VmArch::Aarch64 => candidates.extend([PathBuf::from("/usr/share/AAVMF/AAVMF_CODE.fd")]),
        VmArch::Riscv64 => {}
    }
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "failed to find QEMU EDK2 {} firmware; set {env_var}",
                arch_label(arch)
            )
        })
}

fn edk2_code_filename(arch: VmArch) -> &'static str {
    match arch {
        VmArch::Aarch64 => "edk2-aarch64-code.fd",
        VmArch::X86_64 => "edk2-x86_64-code.fd",
        VmArch::Riscv64 => panic!("riscv64 does not use EDK2 firmware"),
    }
}

fn find_executable_in_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|path| path.join(name))
        .find(|candidate| candidate.is_file())
}

fn discover_qemu_edk2_vars(arch: VmArch, code: &Path) -> Result<PathBuf> {
    for vars in edk2_vars_filenames(arch).map(|name| code.with_file_name(name)) {
        if vars.is_file() {
            return Ok(vars);
        }
    }
    bail!(
        "failed to find QEMU EDK2 {} variable store next to {}",
        arch_label(arch),
        code.display()
    )
}

fn edk2_vars_filenames(arch: VmArch) -> impl Iterator<Item = &'static str> {
    // The OVMF/AAVMF names cover Debian/Ubuntu firmware packages, whose
    // variable stores sit next to the code image under their own names.
    let names: &'static [&'static str] = match arch {
        VmArch::Aarch64 => &["edk2-aarch64-vars.fd", "edk2-arm-vars.fd", "AAVMF_VARS.fd"],
        VmArch::X86_64 => &[
            "edk2-x86_64-vars.fd",
            "edk2-i386-vars.fd",
            "OVMF_VARS_4M.fd",
            "OVMF_VARS.fd",
        ],
        VmArch::Riscv64 => panic!("riscv64 does not use EDK2 firmware"),
    };
    names.iter().copied()
}

/// Attaches the image the guest firmware boots from.
///
/// The guest kernel must never write to it, which is why it carries no
/// scratch-disk serial: a kernel that cannot name a disk leaves it alone.
fn configure_boot_disk(
    qemu: &mut Command,
    boot_disk: VmBootDiskProfile,
    boot_artifact: &Path,
    queues: VirtioDeviceProfile,
) {
    let mut drive = QemuOptions::keyed();
    drive.set("if", "none");
    drive.set("format", "raw");
    drive.set("file", boot_artifact.display());
    drive.set("id", "bootdisk");
    qemu.arg("-drive").arg(drive.to_string());
    let mut device = QemuOptions::new(match boot_disk {
        VmBootDiskProfile::VirtioPci => "virtio-blk-pci",
    });
    device.set("drive", "bootdisk");
    device.set("bootindex", 0);
    queues.apply_pci(&mut device);
    qemu.arg("-device").arg(device.to_string());
}

/// Attaches the scratch disk the guest kernel owns.
///
/// The serial is the contract: the guest identifies its disk by name, so
/// the same bus can carry a boot image the kernel must not touch.
fn configure_data_disk(
    qemu: &mut Command,
    data_disk: VmDataDiskProfile,
    image: &Path,
    queues: VirtioDeviceProfile,
) {
    let mut drive = QemuOptions::keyed();
    drive.set("if", "none");
    drive.set("format", "raw");
    drive.set("file", image.display());
    drive.set("id", "data");
    qemu.arg("-drive").arg(drive.to_string());
    let mut device = QemuOptions::new(match data_disk {
        VmDataDiskProfile::VirtioMmio => "virtio-blk-device",
        VmDataDiskProfile::VirtioPci => "virtio-blk-pci",
    });
    device.set("drive", "data");
    device.set("serial", DATA_DISK_SERIAL);
    apply_transport(
        data_disk == VmDataDiskProfile::VirtioPci,
        queues,
        &mut device,
    );
    qemu.arg("-device").arg(device.to_string());
}

fn configure_host_share(
    qemu: &mut Command,
    host_share: VmHostShareProfile,
    shared_dir: &Path,
    queues: VirtioDeviceProfile,
) {
    let mut fsdev = QemuOptions::new("local");
    fsdev.set("id", "hostfs");
    fsdev.set("path", shared_dir.display());
    fsdev.set("security_model", "none");
    fsdev.set("multidevs", "remap");
    qemu.arg("-fsdev").arg(fsdev.to_string());
    let mut device = QemuOptions::new(match host_share {
        VmHostShareProfile::Virtio9pMmio => "virtio-9p-device",
        VmHostShareProfile::Virtio9pPci => "virtio-9p-pci",
    });
    device.set("fsdev", "hostfs");
    device.set("mount_tag", HOST_SHARE_MOUNT_TAG);
    apply_transport(
        host_share == VmHostShareProfile::Virtio9pPci,
        queues,
        &mut device,
    );
    qemu.arg("-device").arg(device.to_string());
}

/// Gives the guest a virtio-entropy device backed by the host's own
/// `/dev/urandom`.
///
/// QEMU's default `rng-builtin` backend draws from the QEMU process's
/// RNG; naming `/dev/urandom` explicitly ties guest entropy to the host
/// kernel pool instead, which is what a real deployment's device would
/// do.
fn configure_entropy_device(
    qemu: &mut Command,
    entropy: VmEntropyProfile,
    queues: VirtioDeviceProfile,
) {
    let mut object = QemuOptions::new("rng-random");
    object.set("filename", "/dev/urandom");
    object.set("id", "rng0");
    qemu.arg("-object").arg(object.to_string());
    let mut device = QemuOptions::new(match entropy {
        VmEntropyProfile::VirtioRngMmio => "virtio-rng-device",
        VmEntropyProfile::VirtioRngPci => "virtio-rng-pci",
    });
    device.set("rng", "rng0");
    apply_transport(
        entropy == VmEntropyProfile::VirtioRngPci,
        queues,
        &mut device,
    );
    qemu.arg("-device").arg(device.to_string());
}

/// Attaches the machine's translation unit.
///
/// The unit is itself a virtio-PCI function, but it is never one of its
/// own endpoints: it publishes its request and event rings at physical
/// addresses, so it is created without the platform-access property its
/// endpoints carry.
fn configure_iommu(
    qemu: &mut Command,
    iommu: Option<VmIommuProfile>,
    devices: VirtioDeviceProfile,
) {
    let Some(iommu) = iommu else {
        unreachable!("--iommu is refused on a machine profile that declares no translation unit")
    };
    let mut device = QemuOptions::new(match iommu {
        VmIommuProfile::VirtioIommuPci => "virtio-iommu-pci",
    });
    devices.apply(&mut device);
    qemu.arg("-device").arg(device.to_string());
}

/// A memory-mapped virtio device cannot be an IOMMU endpoint, so only a
/// PCI function carries the platform-access properties.
fn apply_transport(pci: bool, devices: VirtioDeviceProfile, options: &mut QemuOptions) {
    if pci {
        devices.apply_pci(options);
    } else {
        devices.apply(options);
    }
}

/// Gives the guest a memory balloon with every reclamation path this
/// kernel drives turned on.
///
/// `free-page-reporting` is what makes an idle guest give its host back
/// real pages; `free-page-hint` is the migration-time form of the same
/// information; `deflate-on-oom` lets the guest take its memory back
/// before it starts killing programs. None of them is on by default in
/// QEMU, and a guest that negotiated a feature the device never offered
/// would silently run without it.
fn configure_balloon(qemu: &mut Command, balloon: VmBalloonProfile, devices: VirtioDeviceProfile) {
    // QEMU walks a free-page hint sequence on a thread of its own and
    // refuses to create the device without one, because the hint queue
    // is drained while the guest is still running.
    let mut iothread = QemuOptions::new("iothread");
    iothread.set("id", BALLOON_IOTHREAD_ID);
    qemu.arg("-object").arg(iothread.to_string());

    let mut device = QemuOptions::new(match balloon {
        VmBalloonProfile::VirtioBalloonMmio => "virtio-balloon-device",
        VmBalloonProfile::VirtioBalloonPci => "virtio-balloon-pci",
    });
    device.set("free-page-reporting", "on");
    device.set("free-page-hint", "on");
    device.set("deflate-on-oom", "on");
    device.set("iothread", BALLOON_IOTHREAD_ID);
    apply_transport(
        balloon == VmBalloonProfile::VirtioBalloonPci,
        devices,
        &mut device,
    );
    qemu.arg("-device").arg(device.to_string());
}

fn configure_watchdog(qemu: &mut Command, watchdog: VmWatchdogProfile) {
    match watchdog {
        VmWatchdogProfile::I6300Esb => {
            qemu.arg("-device").arg("i6300esb");
            qemu.arg("-watchdog-action").arg("reset");
        }
    }
}

impl From<VmSessionCommand> for ResolvedVmSessionCommand {
    fn from(value: VmSessionCommand) -> Self {
        match value {
            VmSessionCommand::Shell(command) => Self::Session(SessionCommand::Shell(command)),
            VmSessionCommand::Tracing(command) => Self::Session(SessionCommand::Tracing(command)),
            VmSessionCommand::Stats => Self::Session(SessionCommand::Stats),
            VmSessionCommand::Repl => Self::Session(SessionCommand::Repl),
            VmSessionCommand::AotBench(command) => Self::AotBench(command),
            VmSessionCommand::WorkloadBench(command) => Self::WorkloadBench(command),
            VmSessionCommand::Balloon(command) => Self::Balloon(command),
            VmSessionCommand::NetSetup(_) | VmSessionCommand::NetTeardown(_) => {
                unreachable!("the privileged network helpers never reach a guest session")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{ErrorKind, Read};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    use anyhow::{Result, bail};
    use helios_inspector_protocol::debugger::programs as debugger_programs;

    use super::*;

    const WATCHDOG_SELF_TEST_DELAY_MS: &str = "5000";
    const WATCHDOG_TIMEOUT_SECS: &str = "10";
    const WATCHDOG_STAGE_TIMEOUT: Duration = Duration::from_secs(120);
    const DIRECT_EXEC_TIMEOUT: Duration = Duration::from_secs(900);
    const SERIAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    const SERIAL_READ_TIMEOUT: Duration = Duration::from_millis(500);
    const DEBUGGER_RUN_STAGE_MARKER: &[u8] = b"[KDBG run:begin]";

    fn default_network_args() -> VmNetworkArgs {
        VmNetworkArgs {
            backend: None,
            queues: None,
            ifname: None,
            bridge: None,
            socket_vmnet_path: None,
            socket_vmnet_client: None,
            device_props: Vec::new(),
        }
    }

    fn default_network() -> VmNetwork {
        VmNetwork::resolve(default_network_args(), VmNetworkFile::default())
    }

    /// Every reclamation path the guest kernel drives has to be on the
    /// device, because QEMU offers none of them by default and a
    /// feature the device never offered is one the guest silently runs
    /// without.
    #[test]
    fn every_profile_attaches_a_balloon_with_every_reclamation_path() {
        for profile in [
            &AARCH64_VIRT_HVF_PROFILE,
            &RISCV64_VM_PROFILE,
            &X86_64_VM_PROFILE,
        ] {
            let mut qemu = Command::new("qemu");
            configure_balloon(&mut qemu, profile.balloon, VirtioDeviceProfile::default());
            let rendered: Vec<String> = qemu
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect();
            let device = rendered.last().expect("the balloon device is rendered");
            assert!(device.contains("free-page-reporting=on"), "{device}");
            assert!(device.contains("free-page-hint=on"), "{device}");
            assert!(device.contains("deflate-on-oom=on"), "{device}");
            assert!(
                device.contains(&format!("iothread={BALLOON_IOTHREAD_ID}")),
                "{device}"
            );
            assert!(
                rendered
                    .iter()
                    .any(|argument| argument == &format!("iothread,id={BALLOON_IOTHREAD_ID}")),
                "the balloon's IOThread has to be created before the device names it: {rendered:?}"
            );
        }
        assert_eq!(
            AARCH64_VIRT_HVF_PROFILE.balloon,
            VmBalloonProfile::VirtioBalloonMmio
        );
        assert_eq!(
            RISCV64_VM_PROFILE.balloon,
            VmBalloonProfile::VirtioBalloonMmio
        );
        assert_eq!(
            X86_64_VM_PROFILE.balloon,
            VmBalloonProfile::VirtioBalloonPci
        );
    }

    /// A balloon session needs a socket to speak QMP over, whether or
    /// not the runtime directory is being kept.
    #[test]
    fn a_balloon_session_gets_a_qmp_socket_of_its_own() {
        let mut command = watchdog_test_command(VmArch::Aarch64);
        command.needs_qmp = true;
        let runtime_dir = Path::new("/tmp/helios-vm");
        assert_eq!(
            qmp_socket_path(&command, runtime_dir),
            Some(PathBuf::from("/tmp/helios-vm/qmp.sock"))
        );

        command.needs_qmp = false;
        assert_eq!(qmp_socket_path(&command, runtime_dir), None);
    }

    #[test]
    fn aarch64_profiles_are_modern_virt_only() {
        assert_eq!(AARCH64_VIRT_HVF_PROFILE.arch, VmArch::Aarch64);
        assert_eq!(
            AARCH64_VIRT_HVF_PROFILE.machine,
            "virt,gic-version=3,acpi=off"
        );
        assert_eq!(AARCH64_VIRT_HVF_PROFILE.default_smp, 4);
        assert_eq!(AARCH64_VIRT_HVF_PROFILE.default_accel, &["hvf", "kvm"]);
        assert_eq!(AARCH64_VIRT_HVF_PROFILE.default_cpu, Some("host"));
        assert_eq!(
            AARCH64_VIRT_HVF_PROFILE.boot_artifact,
            VmBootArtifactKind::LimineUefiDiskImage
        );
        assert_eq!(
            AARCH64_VIRT_HVF_PROFILE.network,
            Some(VmNetworkProfile::VirtioMmio)
        );
        assert_eq!(
            AARCH64_VIRT_HVF_PROFILE.boot_disk,
            Some(VmBootDiskProfile::VirtioPci)
        );
        assert_eq!(
            AARCH64_VIRT_HVF_PROFILE.data_disk,
            VmDataDiskProfile::VirtioMmio
        );
        assert_eq!(
            AARCH64_VIRT_HVF_PROFILE.host_share,
            Some(VmHostShareProfile::Virtio9pMmio)
        );
        assert_eq!(
            AARCH64_VIRT_HVF_PROFILE.entropy,
            VmEntropyProfile::VirtioRngMmio
        );
        assert_eq!(
            AARCH64_VIRT_TCG_PROFILE.entropy,
            VmEntropyProfile::VirtioRngMmio
        );
        assert_eq!(
            AARCH64_VIRT_TCG_PROFILE.machine,
            AARCH64_VIRT_HVF_PROFILE.machine
        );
        assert_eq!(AARCH64_VIRT_TCG_PROFILE.default_accel, &["tcg"]);
        assert_eq!(AARCH64_VIRT_TCG_PROFILE.default_cpu, Some("max"));
        assert_eq!(AARCH64_VIRT_TCG_PROFILE.default_smp, 4);
        assert_ne!(AARCH64_VIRT_HVF_PROFILE.machine, "sbsa-ref");
        assert!(!AARCH64_VIRT_HVF_PROFILE.machine.contains("raspi"));
    }

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
            Some(VmNetworkProfile::VirtioMmio)
        );
        assert_eq!(RISCV64_VM_PROFILE.boot_disk, None);
        assert_eq!(RISCV64_VM_PROFILE.data_disk, VmDataDiskProfile::VirtioMmio);
        assert_eq!(
            RISCV64_VM_PROFILE.host_share,
            Some(VmHostShareProfile::Virtio9pMmio)
        );
        assert_eq!(
            RISCV64_VM_PROFILE.watchdog,
            Some(VmWatchdogProfile::I6300Esb)
        );
        assert_eq!(RISCV64_VM_PROFILE.entropy, VmEntropyProfile::VirtioRngMmio);
    }

    #[test]
    fn x86_64_profile_matches_qemu_first_baseline() {
        assert_eq!(X86_64_VM_PROFILE.arch, VmArch::X86_64);
        assert_eq!(X86_64_VM_PROFILE.machine, "q35");
        assert_ne!(X86_64_VM_PROFILE.machine, "pc");
        assert!(!X86_64_VM_PROFILE.machine.contains("i440fx"));
        assert_eq!(
            X86_64_VM_PROFILE.boot_artifact,
            VmBootArtifactKind::LimineUefiDiskImage
        );
        assert_eq!(X86_64_VM_PROFILE.network, Some(VmNetworkProfile::VirtioPci));
        assert_eq!(
            X86_64_VM_PROFILE.boot_disk,
            Some(VmBootDiskProfile::VirtioPci)
        );
        assert_eq!(X86_64_VM_PROFILE.data_disk, VmDataDiskProfile::VirtioPci);
        assert_eq!(
            X86_64_VM_PROFILE.host_share,
            Some(VmHostShareProfile::Virtio9pPci)
        );
        assert_eq!(
            X86_64_VM_PROFILE.watchdog,
            Some(VmWatchdogProfile::I6300Esb)
        );
        assert_eq!(X86_64_VM_PROFILE.entropy, VmEntropyProfile::VirtioRngPci);
    }

    /// A `vm` invocation with nothing but its defaults, for tests that
    /// only care about one option.
    fn minimal_command() -> VmCommand {
        VmCommand {
            arch: VmArch::X86_64,
            debug: false,
            release: false,
            kernel_debug: false,
            config: None,
            qemu_bin: None,
            kernel: None,
            socket: None,
            serial_stdio: false,
            serial_pty: false,
            no_build: true,
            smp: None,
            memory: None,
            bios: None,
            baud: DEFAULT_BAUD,
            cpu: None,
            accel: Vec::new(),
            shared_dir: None,
            data_disk_size: None,
            gdb: None,
            gdb_wait: false,
            monitor: None,
            qmp: None,
            qemu_log: None,
            qemu_trace: Vec::new(),
            qemu_trace_log: None,
            qemu_arg: Vec::new(),
            boot_programs: Vec::new(),
            no_compiler_plugin: false,
            runtime_dir: None,
            keep_runtime_dir: false,
            iommu: false,
            virtio_packed: false,
            virtio_in_order: false,
            network: default_network_args(),
            command: None,
        }
    }

    #[test]
    fn resolve_uses_profile_defaults_without_local_config() {
        let tempdir =
            tempfile::tempdir().expect("temporary directory for VM config resolution must exist");
        let missing_config = tempdir.path().join("missing-vm.json");
        let command = VmCommand {
            arch: VmArch::X86_64,
            debug: false,
            release: false,
            kernel_debug: false,
            config: Some(missing_config),
            qemu_bin: None,
            kernel: None,
            socket: None,
            serial_stdio: false,
            serial_pty: false,
            no_build: true,
            smp: None,
            memory: None,
            bios: None,
            baud: DEFAULT_BAUD,
            cpu: None,
            accel: Vec::new(),
            shared_dir: None,
            data_disk_size: None,
            gdb: None,
            gdb_wait: false,
            monitor: None,
            qmp: None,
            qemu_log: None,
            qemu_trace: Vec::new(),
            qemu_trace_log: None,
            qemu_arg: Vec::new(),
            boot_programs: Vec::new(),
            no_compiler_plugin: false,
            runtime_dir: None,
            keep_runtime_dir: false,
            iommu: false,
            virtio_packed: false,
            virtio_in_order: false,
            network: default_network_args(),
            command: None,
        };

        let resolved = resolve(command).expect("VM command resolution must succeed");
        assert_eq!(resolved.profile, &X86_64_VM_PROFILE);
        assert!(!resolved.release);
        assert_eq!(resolved.smp, DEFAULT_X86_SMP);
        assert_eq!(resolved.memory, DEFAULT_X86_MEMORY);
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

    /// virtio-iommu can only confine PCI endpoints, so asking for it on
    /// a machine whose virtio devices are memory-mapped is refused
    /// rather than quietly ignored.
    #[test]
    fn iommu_is_refused_on_a_memory_mapped_machine() {
        let tempdir =
            tempfile::tempdir().expect("temporary directory for VM config resolution must exist");
        let config = tempdir.path().join("missing-vm.json");

        for arch in [VmArch::Aarch64, VmArch::Riscv64] {
            let error = resolve(VmCommand {
                arch,
                iommu: true,
                config: Some(config.clone()),
                ..minimal_command()
            })
            .expect_err("a memory-mapped machine cannot confine its devices");
            assert!(
                error.to_string().contains("--iommu is not available"),
                "unexpected error for {arch:?}: {error}"
            );
        }

        let resolved = resolve(VmCommand {
            arch: VmArch::X86_64,
            iommu: true,
            config: Some(config),
            ..minimal_command()
        })
        .expect("q35 carries its virtio devices on PCI");
        assert!(resolved.iommu);
        assert_eq!(
            resolved.virtio_devices.access,
            VirtioPlatformAccess::Confined
        );
    }

    /// Every endpoint the unit protects is told to translate; the unit
    /// itself is not one of its own endpoints.
    #[test]
    fn a_confined_machine_marks_only_its_endpoints() {
        let devices = VirtioDeviceProfile {
            ring: VirtioRingLayout::Split,
            completion: VirtioCompletionOrder::Unordered,
            access: VirtioPlatformAccess::Confined,
        };

        let mut endpoint = QemuOptions::new("virtio-blk-pci");
        devices.apply_pci(&mut endpoint);
        assert_eq!(
            endpoint.to_string(),
            "virtio-blk-pci,disable-legacy=on,iommu_platform=on"
        );

        let mut unit = QemuOptions::new("virtio-iommu-pci");
        devices.apply(&mut unit);
        assert_eq!(unit.to_string(), "virtio-iommu-pci");

        // A memory-mapped device is never an endpoint either.
        let mut mmio = QemuOptions::new("virtio-blk-device");
        apply_transport(false, devices, &mut mmio);
        assert_eq!(mmio.to_string(), "virtio-blk-device");
    }

    #[test]
    fn resolve_debug_preset_enables_kernel_debug_workbench() {
        let tempdir =
            tempfile::tempdir().expect("temporary directory for VM config resolution must exist");
        let command = VmCommand {
            arch: VmArch::X86_64,
            debug: true,
            release: false,
            kernel_debug: false,
            config: Some(tempdir.path().join("missing-vm.json")),
            qemu_bin: None,
            kernel: None,
            socket: None,
            serial_stdio: false,
            serial_pty: false,
            no_build: true,
            smp: None,
            memory: None,
            bios: None,
            baud: DEFAULT_BAUD,
            cpu: None,
            accel: Vec::new(),
            shared_dir: None,
            data_disk_size: None,
            gdb: None,
            gdb_wait: false,
            monitor: None,
            qmp: None,
            qemu_log: None,
            qemu_trace: Vec::new(),
            qemu_trace_log: None,
            qemu_arg: Vec::new(),
            boot_programs: Vec::new(),
            no_compiler_plugin: false,
            runtime_dir: None,
            keep_runtime_dir: false,
            iommu: false,
            virtio_packed: false,
            virtio_in_order: false,
            network: default_network_args(),
            command: None,
        };

        let resolved = resolve(command).expect("VM debug command resolution must succeed");
        assert!(resolved.kernel_debug);
        assert_eq!(resolved.gdb.as_deref(), Some(DEFAULT_GDB_ENDPOINT));
        assert!(resolved.gdb_wait);
        assert!(resolved.keep_runtime_dir);
        assert_eq!(
            resolved.kernel,
            repo_root()
                .join("target")
                .join(X86_64_VM_PROFILE.cargo_target)
                .join("kernel-debug")
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
        let command = watchdog_test_command(arch);
        run_step(
            "building helios-cli",
            cargo_build_command(repo_root(), command.release, false)
                .arg("-p")
                .arg("helios-cli"),
        )?;
        let prebuild_manifest = run_kernel_prebuild(&command)?;
        let status = std::process::Command::new("cargo")
            .current_dir(repo_root())
            .arg("build")
            .arg("--target")
            .arg(arch.profile().cargo_target)
            .arg("--bin")
            .arg(arch.profile().kernel_artifact_name)
            .env("HELIOS_KERNEL_PREBUILD_MANIFEST", prebuild_manifest)
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
        let profile = arch.profile();
        ResolvedVmCommand {
            profile,
            release: false,
            kernel_debug: false,
            qemu_bin: PathBuf::from(profile.qemu_bin),
            kernel: default_kernel_path(arch, build_profile_dir(false, false)),
            socket: None,
            serial_stdio: false,
            serial_pty: false,
            no_build: true,
            smp: profile.default_smp,
            memory: profile.default_memory.to_owned(),
            bios: profile.default_bios.map(str::to_owned),
            baud: DEFAULT_BAUD,
            cpu: profile.default_cpu.map(str::to_owned),
            accel: profile
                .default_accel
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            shared_dir: None,
            data_disk_bytes: DEFAULT_DATA_DISK_BYTES,
            gdb: None,
            gdb_wait: false,
            monitor: None,
            qmp: None,
            qemu_log: None,
            qemu_trace: Vec::new(),
            qemu_trace_log: None,
            qemu_arg: Vec::new(),
            boot_programs: vec!["debugger".to_owned()],
            no_compiler_plugin: true,
            runtime_dir: None,
            keep_runtime_dir: false,
            iommu: false,
            virtio_devices: VirtioDeviceProfile::default(),
            needs_qmp: false,
            network: default_network(),
            qemu_net: profile.network.map(|device| {
                default_network()
                    .render(
                        device,
                        VirtioDeviceProfile::default(),
                        profile.default_smp,
                        HostPlatform::current(),
                    )
                    .expect("the default user backend renders on every host")
            }),
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
                    &["http://neverssl.com/".to_owned()],
                ),
            )
            .await
        })
        .ok_or_else(|| anyhow::anyhow!("timed out waiting for direct curl exec-path result"))??
        .map_err(|error| anyhow::anyhow!("direct curl exec-path failed: {error:?}"))?;
        assert_eq!(curl.exit_code, 0, "curl exited non-zero: {curl:?}");
        let curl_stdout = String::from_utf8_lossy(&curl.output.stdout).to_ascii_lowercase();
        assert!(
            curl_stdout.contains("neverssl"),
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
        .ok_or_else(|| anyhow::anyhow!("timed out waiting for direct CPython exec-path result"))??
        .map_err(|error| anyhow::anyhow!("direct CPython exec-path failed: {error:?}"))?;
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
            kernel_debug: false,
            qemu_bin: PathBuf::from(profile.qemu_bin),
            kernel: default_kernel_path(arch, build_profile_dir(true, false)),
            socket: None,
            serial_stdio: false,
            serial_pty: false,
            no_build: true,
            smp: profile.default_smp,
            memory: profile.default_memory.to_owned(),
            bios: profile.default_bios.map(str::to_owned),
            baud: DEFAULT_BAUD,
            cpu: profile.default_cpu.map(str::to_owned),
            accel: profile
                .default_accel
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            shared_dir: Some(repo_root().to_path_buf()),
            data_disk_bytes: DEFAULT_DATA_DISK_BYTES,
            gdb: None,
            gdb_wait: false,
            monitor: None,
            qmp: None,
            qemu_log: None,
            qemu_trace: Vec::new(),
            qemu_trace_log: None,
            qemu_arg: Vec::new(),
            boot_programs: Vec::new(),
            no_compiler_plugin: false,
            runtime_dir: None,
            keep_runtime_dir: false,
            iommu: false,
            virtio_devices: VirtioDeviceProfile::default(),
            needs_qmp: false,
            network: default_network(),
            qemu_net: profile.network.map(|device| {
                default_network()
                    .render(
                        device,
                        VirtioDeviceProfile::default(),
                        profile.default_smp,
                        HostPlatform::current(),
                    )
                    .expect("the default user backend renders on every host")
            }),
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
