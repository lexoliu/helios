use std::fs;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use askama::Template;
use clap::{Args as ClapArgs, Subcommand, ValueEnum};
use console::style;
use directories::ProjectDirs;
use helios_hal::fs::HOST_SHARE_MOUNT_TAG;
use helios_inspector_protocol::debugger::filesystem as debugger_fs;
use helios_inspector_protocol::system::profiling as system_profiling;
use helios_inspector_protocol::system::programs as system_programs;
use helios_profdata::{KernelProfileStore, KernelProfileStoreError, ProfileUseError};
use helios_workspace_root::{WorkspaceRoot, WorkspaceRootError};
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

use crate::stats_tui::format_bytes;
use crate::workload_bench::{
    DEFAULT_WORKLOAD_TIMEOUT_SECONDS, VmProvenance, WorkloadBenchCommand, WorkloadBenchError,
    WorkloadSelectionError, guest_step_under_deadline,
};
use crate::{
    ConnectError, InterruptError, SessionCommand, SessionError, connect_client, run_connected,
};

mod input;
mod network;
mod qemu;
mod qmp;
mod raw_profile;

use input::{InputScript, InputScriptError};
use network::{
    HostPlatform, NetSetupCommand, NetTeardownCommand, QemuNetArgs, VmNetwork, VmNetworkArgs,
    VmNetworkError, VmNetworkFile, VmNetworkProfile, VmNetworkSetupError,
};
use qemu::QemuOptions;
use qmp::{QmpClient, QmpError, SizeError};
use raw_profile::{ProfileCommand, RawProfileCollectError};

/// Why a `vm` session did not run.
///
/// The four stages are separate variants because a failure in each is
/// answered somewhere else: the flags and the config file, the guest
/// build, the machine QEMU was asked to construct, and the session that
/// ran on it once it was up.
#[derive(Debug, thiserror::Error)]
pub(crate) enum VmError {
    #[error("{0}")]
    NetworkSetup(#[from] VmNetworkSetupError),
    #[error("{0}")]
    Config(#[from] VmConfigError),
    #[error("{0}")]
    Build(#[from] VmBuildError),
    #[error("{0}")]
    Runtime(#[from] VmRuntimeError),
    /// A session that failed, with the runtime directory it left behind
    /// and QEMU's own account of the machine.
    ///
    /// QEMU's log is the only record of a machine it refused to build or
    /// a device backend that never started, and the runtime directory is
    /// about to go away, so both travel with the failure.
    #[error("VM runtime directory: {runtime_dir}\n{report}: {source}")]
    Session {
        runtime_dir: String,
        report: String,
        #[source]
        source: VmSessionError,
    },
}

/// Why the flags, the config file and this host do not describe a VM
/// that can be booted.
#[derive(Debug, thiserror::Error)]
pub(crate) enum VmConfigError {
    #[error("--release and --kernel-debug cannot be used together")]
    ReleaseWithKernelDebug,
    #[error(
        "--profile-generate builds its own optimised kernel and cannot be combined with \
         --release, --debug or --kernel-debug"
    )]
    ProfileGenerateWithOtherProfile,
    #[error(
        "--profile-use builds an optimised kernel from a collected profile and cannot be \
         combined with --profile-generate, --debug or --kernel-debug"
    )]
    ProfileUseWithOtherProfile,
    #[error("{0}")]
    ProfileUse(#[from] ProfileUseError),
    #[error(
        "a --release {arch} kernel is built against the fetched kernel profile \
         (docs/pgo.md): {source}"
    )]
    ReleaseKernelProfile {
        arch: &'static str,
        #[source]
        source: KernelProfileStoreError,
    },
    #[error(
        "--without-kernel-profile is the plain control of a target whose release builds read \
         the kernel profile, and a --release {arch} kernel reads none (docs/pgo.md)"
    )]
    WithoutKernelProfileOnPlainTarget { arch: &'static str },
    #[error("{0}")]
    WorkloadSelection(#[from] WorkloadSelectionError),
    #[error("failed to read inspector VM config {path}: {source}")]
    ReadConfig {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to decode inspector VM config {path}: {source}")]
    DecodeConfig {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("{0}")]
    NoNativeAccelerator(#[from] NoNativeAccelerator),
    #[error("--data-disk-size must be greater than zero")]
    ZeroDataDiskSize,
    #[error("--gdb-wait requires --gdb or --debug")]
    GdbWaitWithoutGdb,
    #[error("{0}")]
    VsockUnsupported(#[from] crate::vsock::VsockUnsupported),
    #[error(
        "--rpc-transport vsock keeps the guest console on the serial line, \
         which --serial-stdio and --serial-pty take over"
    )]
    VsockWithSerialConsole,
    #[error("--vsock-cid must be 3 or greater; 0, 1 and 2 are reserved")]
    ReservedVsockCid,
    #[error("--serial-stdio cannot share stdio with --monitor stdio")]
    SerialStdioWithMonitorStdio,
    #[error(
        "--acpi is not available on {arch}: its machine publishes one firmware description \
         and the kernel already takes that one"
    )]
    AcpiUnavailable { arch: &'static str },
    #[error(
        "--iommu is not available on {arch}: its virtio devices are memory-mapped, and \
         virtio-iommu can only confine PCI endpoints"
    )]
    IommuUnavailable { arch: &'static str },
    #[error("{0}")]
    Network(#[from] VmNetworkError),
    #[error("{0}")]
    AudioDev(#[from] AudioDevError),
    #[error("shared directory does not exist: {path}")]
    SharedDirMissing { path: String },
    #[error("aarch64-virt-hvf requires an aarch64 host; pass --accel tcg explicitly for TCG")]
    HvfNeedsAarch64Host,
    #[error("{0}")]
    WorkspaceRoot(#[from] WorkspaceRootError),
}

/// Why the guest image or the host tools a boot needs could not be
/// built.
#[derive(Debug, thiserror::Error)]
pub(crate) enum VmBuildError {
    #[error("{0}")]
    Step(#[from] BuildStepError),
    #[error("{0}")]
    Tool(#[from] ToolDiscoveryError),
    #[error("{0}")]
    WorkspaceRoot(#[from] WorkspaceRootError),
}

/// One build or provisioning step run as a child process.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BuildStepError {
    #[error("failed to spawn {label}: {source}")]
    Spawn {
        label: String,
        #[source]
        source: io::Error,
    },
    #[error("{label} exited with status {status}")]
    Exited {
        label: String,
        status: std::process::ExitStatus,
    },
}

/// Why a host tool the inspector drives could not be found.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ToolDiscoveryError {
    #[error("HELIOS_CLI_BIN does not point to a file: {path}")]
    CliBinNotAFile { path: String },
    #[error("failed to locate current executable: {source}")]
    CurrentExe {
        #[source]
        source: io::Error,
    },
    #[error("failed to find helios-cli; run `cargo build -p helios-cli` or set HELIOS_CLI_BIN")]
    CliMissing,
}

/// Why the machine itself could not be constructed or started.
#[derive(Debug, thiserror::Error)]
pub(crate) enum VmRuntimeError {
    #[error("failed to create VM runtime directory {path}: {source}")]
    CreateRuntimeDir {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to create temporary QEMU runtime directory: {source}")]
    CreateTempRuntimeDir {
        #[source]
        source: io::Error,
    },
    #[error("system time is earlier than UNIX_EPOCH: {source}")]
    SystemTimeBeforeEpoch {
        #[source]
        source: std::time::SystemTimeError,
    },
    #[error("{0}")]
    WorkspaceRoot(#[from] WorkspaceRootError),
    #[error("{0}")]
    Serial(#[from] crate::serial::SerialError),
    #[error("{0}")]
    Tool(#[from] ToolDiscoveryError),
    #[error("{0}")]
    Step(#[from] BuildStepError),
    #[error("failed to create socket directory {path}: {source}")]
    CreateSocketDir {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to inspect existing socket path {path}: {source}")]
    InspectSocketPath {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("refusing to overwrite existing non-socket path {path}")]
    SocketPathNotASocket { path: String },
    #[error("failed to remove stale socket {path}: {source}")]
    RemoveStaleSocket {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to create log directory {path}: {source}")]
    CreateLogDir {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to canonicalize kernel {path}: {source}")]
    CanonicalizeKernel {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to create scratch disk image {path}: {source}")]
    CreateDataDisk {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to size scratch disk image {path}: {source}")]
    SizeDataDisk {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("QEMU option paths must be valid UTF-8: {path}")]
    OptionPathNotUtf8 { path: String },
    #[error("{env_var} does not point to a file: {path}")]
    Edk2EnvNotAFile { env_var: &'static str, path: String },
    #[error("failed to find QEMU EDK2 {arch} firmware; set {env_var}")]
    Edk2CodeMissing {
        arch: &'static str,
        env_var: &'static str,
    },
    #[error("failed to find QEMU EDK2 {arch} variable store next to {code}")]
    Edk2VarsMissing { arch: &'static str, code: String },
    #[error("failed to prepare EDK2 variable store {vars} from {template}: {source}")]
    Edk2VarsCopy {
        vars: String,
        template: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to {step} {path}: {source}")]
    QemuLog {
        /// `create` or `open … for append`, spelled as the message reads
        /// it.
        step: &'static str,
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to start QEMU executable {path}: {source}")]
    SpawnQemu {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("QEMU {pipe} pipe was not available for serial stdio")]
    ChildPipeMissing { pipe: &'static str },
    #[error("failed to poll QEMU process state: {source}")]
    PollQemu {
        #[source]
        source: io::Error,
    },
    #[error("failed to read QEMU log {path}: {source}")]
    ReadQemuLog {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("QEMU exited before opening the debug serial socket {socket}\n{log}")]
    QemuExitedBeforeSocket { socket: String, log: String },
    #[error("timed out waiting for QEMU to create debug serial socket {socket}")]
    SocketTimedOut { socket: String },
    #[error("VM transport was already taken")]
    TransportAlreadyTaken,
    #[error("{0}")]
    SocketPathTooLong(#[from] SocketPathTooLong),
    #[error("failed to create the VM socket directory in {path}: {source}")]
    CreateSocketDirectory {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to replace the stale socket link {path}: {source}")]
    ReplaceSocketLink {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to record the socket directory {target} as {link}: {source}")]
    RecordSocketLink {
        target: String,
        link: String,
        #[source]
        source: io::Error,
    },
}

/// Why the session that ran on the booted machine failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum VmSessionError {
    #[error("{0}")]
    Runtime(#[from] VmRuntimeError),
    #[error("socket path must be valid UTF-8: {path}")]
    SocketPathNotUtf8 { path: String },
    #[error("failed to connect inspector RPC client{over}: {source}")]
    Connect {
        /// The transport the connection was attempted over, empty for the
        /// serial socket the plain `--socket` path uses.
        over: &'static str,
        #[source]
        source: ConnectError,
    },
    #[error("failed to connect inspector RPC client over vsock: {source}")]
    ConnectVsock {
        #[source]
        source: crate::vsock::VsockConnectError,
    },
    #[error("the {action} command needs a QMP socket; pass --qmp unix:<path>,server=on,wait=off")]
    NeedsQmp { action: &'static str },
    #[error("{0}")]
    Qmp(#[from] QmpError),
    #[error("{0}")]
    InputScript(#[from] InputScriptError),
    #[error("the thread taking the captures ended without reporting what it did")]
    CaptureThreadLost,
    #[error(
        "the guest program {program} exited before the captures were taken; a program a capture \
         is taken of has to still be drawing when it is"
    )]
    GuestProgramExitedEarly { program: String },
    #[error("failed to prepare the screendump directory {path}: {source}")]
    ScreendumpDirectory {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to resolve the screendump path {path}: {source}")]
    ScreendumpPath {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to capture the guest scanout into {path}: {source}")]
    Screendump {
        path: String,
        #[source]
        source: QmpError,
    },
    #[error("failed to send statement {statement} of the input script {path}: {source}")]
    SendInput {
        path: String,
        statement: usize,
        #[source]
        source: QmpError,
    },
    #[error("{0}")]
    Size(#[from] SizeError),
    #[error("failed to set the balloon target to {target}: {source}")]
    SetBalloonTarget {
        target: String,
        #[source]
        source: QmpError,
    },
    #[error("{0}")]
    WorkloadBench(#[from] WorkloadBenchError),
    #[error("{0}")]
    AotBench(#[from] AotBenchError),
    #[error("{0}")]
    RawProfile(#[from] RawProfileCollectError),
    #[error("{0}")]
    ProfileOutput(#[from] ProfileOutputError),
    #[error("{0}")]
    Session(#[from] SessionError),
    #[error("{0}")]
    Interrupt(#[from] InterruptError),
    #[error("{0}")]
    Profiling(#[from] ProfilingStepError),
}

/// One profiling RPC around a bench run, named by the step it was for.
///
/// Every one of them fails the same way — the guest refused or never
/// answered — so the step is what tells them apart in the message.
#[derive(Debug, thiserror::Error)]
#[error("failed to {step}: {source}")]
pub(crate) struct ProfilingStepError {
    /// The profiling step, spelled the way the message reads it.
    step: &'static str,
    #[source]
    source: helios_inspector_protocol::RpcError,
}

/// Why an `aot-bench` run did not produce its measurements.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AotBenchError {
    #[error("failed to read {path}: {source}")]
    ReadWasm {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("aot-bench --iterations must be non-zero")]
    ZeroIterations,
    #[error("failed to upload {path}: {source}")]
    Upload {
        path: String,
        #[source]
        source: helios_inspector_protocol::RpcError,
    },
    #[error("failed to AOT compile uploaded wasm iteration {iteration}: {source}")]
    Compile {
        iteration: u16,
        #[source]
        source: helios_inspector_protocol::RpcError,
    },
    #[error("remote AOT iteration {iteration} failed: {kind:?}: {detail}")]
    Refused {
        iteration: u16,
        kind: system_programs::ExecErrorKind,
        detail: String,
    },
    #[error("failed to report an aot-bench iteration: {source}")]
    Report {
        #[source]
        source: io::Error,
    },
    #[error("{0}")]
    ProfileOutput(#[from] ProfileOutputError),
    #[error("{0}")]
    RawProfile(#[from] RawProfileCollectError),
}

/// Why a profile or metric file a run asked for was not written.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ProfileOutputError {
    #[error("failed to write {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to encode the perf metrics for {path}: {source}")]
    Encode {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to report the written profile outputs: {source}")]
    Report {
        #[source]
        source: io::Error,
    },
}

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
/// The QEMU chardev the guest's debug serial line is bound to.
const DEBUG_SERIAL_CHARDEV: &str = "helios-debug-serial";
/// The runtime-directory file holding a raw copy of the debug serial
/// line, host side. `--debug-serial-log` puts it elsewhere.
const DEBUG_SERIAL_LOG_NAME: &str = "debug-serial.log";
/// Longest unix socket path the inspector will hand to QEMU.
///
/// `sockaddr_un::sun_path` holds 108 bytes on Linux and 104 on macOS,
/// the terminator included, so 103 is what fits on either host the
/// inspector runs on. QEMU refuses a longer path rather than truncating
/// it, and it refuses it after the guest image is built.
const UNIX_SOCKET_PATH_MAX: usize = 103;
/// The runtime directory's link to the socket directory the sockets of
/// that VM actually live in.
const SOCKET_DIR_LINK_NAME: &str = "sockets";
const DEBUG_SOCKET_NAME: &str = "debug.sock";
const MONITOR_SOCKET_NAME: &str = "monitor.sock";
const QMP_SOCKET_NAME: &str = "qmp.sock";
/// The QEMU IOThread the memory balloon's free-page hint queue runs on.
const BALLOON_IOTHREAD_ID: &str = "balloon-io";
/// How long the guest's reported balloon size has to hold still before a
/// wait calls it settled short of the target it was given.
const BALLOON_STILL_FOR: Duration = Duration::from_secs(10);
/// How much of QEMU's log a failed session quotes.
///
/// The log holds QEMU's own stdout and stderr and stays small, because
/// the guest console goes to the serial socket instead; the cap is there
/// so a `-d` trace run cannot bury the error that matters.
const QEMU_REPORT_TAIL_BYTES: usize = 8192;

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

/// How a profile exposes the guest's display adapter.
///
/// Attached only when a session asks for the desktop devices: a guest
/// that draws nothing has no use for a scanout, and every lane that does
/// not ask for one boots the machine it booted before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VmDisplayProfile {
    VirtioGpuMmio,
    VirtioGpuPci,
}

/// How a profile exposes the guest's keyboard, tablet and mouse.
///
/// The three arrive together because they are one desktop's input: the
/// tablet is what an absolute pointer event moves, the mouse is what a
/// relative one moves, and a guest given only one of them would silently
/// ignore half of an input script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VmInputProfile {
    VirtioInputMmio,
    VirtioInputPci,
}

/// How a profile exposes the guest's sound device.
///
/// Attached only when a session names a host audio backend, because the
/// device is created against one: QEMU refuses a virtio-sound device
/// whose `audiodev` names nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VmSoundProfile {
    VirtioSoundMmio,
    VirtioSoundPci,
}

/// How a profile exposes the guest's vsock transport.
///
/// Unlike the other devices this one is optional at run time as well as
/// per profile: the backend is `vhost-vsock`, which exists only where
/// the host kernel provides `/dev/vhost-vsock`, so a session asks for it
/// explicitly and the inspector refuses rather than silently omitting
/// the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VmVsockProfile {
    VhostVsockMmio,
    VhostVsockPci,
}

/// Which transport carries the inspector RPC to the guest debugger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum VmRpcTransport {
    /// The debug serial line, shared with the guest console.
    #[default]
    Serial,
    /// A vsock connection to the guest debugger, leaving the serial line
    /// to the console alone. Requires a host that can provide a vsock
    /// device; the session fails rather than falling back.
    Vsock,
}

/// Which build of the kernel image a session boots.
///
/// The five are exclusive by construction rather than by a rule spread over
/// booleans: each names one cargo profile, one `target/<triple>/` directory,
/// and whether the image carries LLVM instrumentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum KernelBuildProfile {
    /// Cargo's `dev` profile, the default.
    #[default]
    Debug,
    /// `kernel-debug`: debuginfo and unstripped symbols for GDB and LLDB.
    KernelDebug,
    /// `release`.
    Release,
    /// `profile-generate`: release plus `-C profile-generate`, the image a
    /// PGO collection boots (docs/pgo.md).
    ProfileGenerate,
    /// `profile-use`: release plus `-C profile-use`, the image a
    /// collected profile optimises (docs/pgo.md). The profile itself is
    /// named by [`KernelBuildSpec::profile_use`]: a build kind says what
    /// kind of build it is, and two PGO kernels from two profiles are the
    /// same kind of build.
    ProfileUse,
}

impl KernelBuildProfile {
    /// The `target/<triple>/` subdirectory the artifacts land in.
    fn directory(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::KernelDebug => "kernel-debug",
            Self::Release => "release",
            Self::ProfileGenerate => "profile-generate",
            Self::ProfileUse => "profile-use",
        }
    }

    /// Whether the guest image this profile builds is an optimised one.
    fn optimised(self) -> bool {
        matches!(
            self,
            Self::Release | Self::ProfileGenerate | Self::ProfileUse
        )
    }

    /// Whether the image carries LLVM instrumentation.
    fn instrumented(self) -> bool {
        matches!(self, Self::ProfileGenerate)
    }

    /// The profile the guest programs of the bootfs are built with.
    ///
    /// `helios-cli kernel-prebuild` knows two: an optimised one and a
    /// debug one. Nothing instruments the guest programs — the profile
    /// being collected is the kernel's — so an instrumented kernel boots
    /// the same optimised user space a release kernel does.
    fn guest_programs(self) -> &'static str {
        if self.optimised() { "release" } else { "debug" }
    }

    /// The profile the host tools built alongside the guest use. They are
    /// never instrumented — nothing profiles the inspector — and they are
    /// found next to the running binary, so they stay in the two
    /// directories a host build ever uses.
    fn host(self) -> Self {
        if self.optimised() {
            Self::Release
        } else {
            Self::Debug
        }
    }
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
    /// The `-machine` option list that makes this machine publish ACPI
    /// tables instead of a device tree, when it can publish both.
    acpi_machine: Option<&'static str>,
    /// The translation unit this machine can put its virtio devices
    /// behind, if any.
    iommu: Option<VmIommuProfile>,
    balloon: VmBalloonProfile,
    /// How this profile would attach a display adapter, its input
    /// devices and its sound device, when a session asks for the desktop
    /// (`--desktop`) or names an audio backend (`--audiodev`).
    display: VmDisplayProfile,
    input: VmInputProfile,
    sound: VmSoundProfile,
    /// How this profile would attach a vsock device, when a session asks
    /// for the vsock RPC transport.
    vsock: VmVsockProfile,
    /// Linker script fragment that places this target's LLVM
    /// instrumentation sections, for `--profile-generate` builds.
    ///
    /// `x86_64-unknown-none` has none: it links with LLD's own layout,
    /// which places the `__llvm_prf_*` sections as ordinary orphans, keeps
    /// them under `--gc-sections` because their `__start_`/`__stop_`
    /// symbols are referenced, and synthesises those symbols itself. The
    /// two targets that bring their own linker script have to say where
    /// the sections go, or the counters land outside the image the boot
    /// code loads (docs/pgo.md).
    profile_generate_linker_script: Option<&'static str>,
    /// Whether a `--release` kernel for this target is built against the
    /// fetched kernel profile (`docs/pgo.md`, #226, #313).
    ///
    /// Performance is measured on one architecture (AGENTS.md §3.6), and
    /// it is the one whose releases carry a profile: on the others a
    /// release build is a plain release build, because there is no
    /// profile of that target to spend and a `.profdata` carries the
    /// function hashes of the target it was collected on.
    release_kernel_profile: bool,
}

/// The `virt` machine as the aarch64 kernel boots it by default: EDK2
/// installs the FDT configuration table only when it is not also
/// publishing ACPI tables, so the device-tree description needs ACPI
/// turned off.
const AARCH64_VIRT_MACHINE: &str = "virt,gic-version=3,acpi=off";

/// The same machine publishing ACPI tables instead, which is what
/// `--acpi` selects and what every Arm server platform looks like. The
/// kernel then takes its whole description from the MADT, the SPCR and
/// the DSDT.
const AARCH64_VIRT_ACPI_MACHINE: &str = "virt,gic-version=3,acpi=on";

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
    acpi_machine: Some(AARCH64_VIRT_ACPI_MACHINE),
    iommu: None,
    balloon: VmBalloonProfile::VirtioBalloonMmio,
    display: VmDisplayProfile::VirtioGpuMmio,
    input: VmInputProfile::VirtioInputMmio,
    sound: VmSoundProfile::VirtioSoundMmio,
    vsock: VmVsockProfile::VhostVsockMmio,
    profile_generate_linker_script: Some("aarch64/profile-generate.ld"),
    release_kernel_profile: false,
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
    acpi_machine: Some(AARCH64_VIRT_ACPI_MACHINE),
    iommu: None,
    balloon: VmBalloonProfile::VirtioBalloonMmio,
    display: VmDisplayProfile::VirtioGpuMmio,
    input: VmInputProfile::VirtioInputMmio,
    sound: VmSoundProfile::VirtioSoundMmio,
    vsock: VmVsockProfile::VhostVsockMmio,
    profile_generate_linker_script: Some("aarch64/profile-generate.ld"),
    release_kernel_profile: false,
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
    acpi_machine: None,
    iommu: None,
    balloon: VmBalloonProfile::VirtioBalloonMmio,
    display: VmDisplayProfile::VirtioGpuMmio,
    input: VmInputProfile::VirtioInputMmio,
    sound: VmSoundProfile::VirtioSoundMmio,
    vsock: VmVsockProfile::VhostVsockMmio,
    profile_generate_linker_script: Some("riscv/profile-generate.x"),
    release_kernel_profile: false,
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
    acpi_machine: None,
    iommu: Some(VmIommuProfile::VirtioIommuPci),
    balloon: VmBalloonProfile::VirtioBalloonPci,
    display: VmDisplayProfile::VirtioGpuPci,
    input: VmInputProfile::VirtioInputPci,
    sound: VmSoundProfile::VirtioSoundPci,
    vsock: VmVsockProfile::VhostVsockPci,
    profile_generate_linker_script: None,
    // The one architecture performance is measured on, and the one
    // release.yml collects and publishes a profile for.
    release_kernel_profile: true,
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

/// The host display backend QEMU opens for the session.
///
/// `none` is the default and what every existing lane boots: the
/// machine still has whatever display device the session attached, and
/// its scanout is read through QMP rather than shown. A backend this
/// host's QEMU was not built with is refused by QEMU, naming itself;
/// nothing here substitutes another one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum VmDisplayBackend {
    #[default]
    None,
    Cocoa,
    Gtk,
    Sdl,
}

impl VmDisplayBackend {
    /// The token QEMU's `-display` takes.
    fn token(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Cocoa => "cocoa",
            Self::Gtk => "gtk",
            Self::Sdl => "sdl",
        }
    }
}

/// The host audio backend the guest's sound device plays into.
///
/// `none` attaches no sound device at all, which is what every lane that
/// does not ask for audio boots. `wav` writes the guest's playback to a
/// file and needs no host audio at all, so it is the sink a headless
/// runner records with; anything else names a backend of the host's own
/// and is QEMU's to refuse.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum VmAudioDev {
    #[default]
    None,
    Wav {
        path: PathBuf,
    },
    Host {
        backend: String,
    },
}

/// Why `--audiodev` does not name a backend QEMU could be handed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AudioDevError {
    #[error("--audiodev wav needs the file to write: pass --audiodev wav:<path>")]
    WavWithoutPath,
    #[error(
        "--audiodev {text:?} is not a backend name: QEMU names them in one word, like \
         `coreaudio`, `pa` or `alsa`, and `wav:<path>` writes to a file"
    )]
    NotABackendName { text: String },
}

impl VmAudioDev {
    /// The identifier the sound device names its backend by.
    const ID: &'static str = "snd0";

    fn parse(text: &str) -> Result<Self, AudioDevError> {
        if text == "none" {
            return Ok(Self::None);
        }
        if let Some(path) = text.strip_prefix("wav:") {
            if path.is_empty() {
                return Err(AudioDevError::WavWithoutPath);
            }
            return Ok(Self::Wav {
                path: PathBuf::from(path),
            });
        }
        // A backend whose name carries a comma or an equals sign would
        // smuggle further options into the `-audiodev` list; QEMU's own
        // names never do.
        let named = !text.is_empty()
            && text
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-');
        if !named {
            return Err(AudioDevError::NotABackendName {
                text: text.to_owned(),
            });
        }
        Ok(Self::Host {
            backend: text.to_owned(),
        })
    }

    /// The `-audiodev` option list QEMU creates the backend from, or
    /// `None` when the session asked for no sound at all.
    fn options(&self) -> Option<QemuOptions> {
        let mut options = match self {
            Self::None => return None,
            Self::Wav { path } => {
                let mut options = QemuOptions::new("wav");
                options.set("path", path.display());
                options
            }
            Self::Host { backend } => QemuOptions::new(backend.clone()),
        };
        options.set("id", Self::ID);
        Some(options)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct VmConfigFile {
    #[serde(default)]
    pub(crate) arch: Option<VmArch>,
    #[serde(default)]
    pub(crate) rpc_transport: Option<VmRpcTransport>,
    #[serde(default)]
    pub(crate) vsock_cid: Option<u32>,
    #[serde(default)]
    pub(crate) debug: Option<bool>,
    #[serde(default)]
    pub(crate) release: Option<bool>,
    #[serde(default)]
    pub(crate) profile_generate: Option<bool>,
    #[serde(default)]
    pub(crate) profile_use: Option<PathBuf>,
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
    pub(crate) debug_serial_log: Option<PathBuf>,
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
    pub(crate) acpi: Option<bool>,
    #[serde(default)]
    pub(crate) iommu: Option<bool>,
    #[serde(default)]
    pub(crate) virtio_packed: Option<bool>,
    #[serde(default)]
    pub(crate) virtio_in_order: Option<bool>,
    #[serde(default)]
    pub(crate) desktop: Option<bool>,
    #[serde(default)]
    pub(crate) display: Option<VmDisplayBackend>,
    #[serde(default)]
    pub(crate) audiodev: Option<String>,
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

    /// Build an instrumented kernel: release plus `-C profile-generate`,
    /// whose profile the `profile` action collects (docs/pgo.md).
    ///
    /// An instrumented kernel counts every branch it takes, so it is a
    /// collection artifact and never a measurement one.
    #[arg(long, default_value_t = false, conflicts_with_all = ["release", "debug", "kernel_debug"])]
    profile_generate: bool,

    /// Build an optimised kernel from a collected profile: release plus
    /// `-C profile-use=<file>`, where the file is the merged
    /// `.profdata` a `--profile-generate` collection produced
    /// (docs/pgo.md).
    ///
    /// The profile is named, never discovered: a PGO kernel is only as
    /// good as the profile it was built from, so which profile that was
    /// is part of the command that built it.
    #[arg(long, value_name = "FILE", conflicts_with_all = ["debug", "kernel_debug", "profile_generate"])]
    profile_use: Option<PathBuf>,

    /// Build the `--release` kernel of a target whose release builds read
    /// the fetched kernel profile without one: the plain control of a
    /// profile-guided measurement (docs/pgo.md, #322). It lands in the
    /// `release` directory, where a plain build of any other target
    /// lands. Asking for it on a target that reads no profile is refused,
    /// because there the control and the candidate are one build.
    #[arg(
        long,
        default_value_t = false,
        requires = "release",
        conflicts_with = "profile_use"
    )]
    without_kernel_profile: bool,

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
    ///
    /// Left unset, the profile's native accelerator is required and the
    /// run fails naming the check that refused it; emulation is never a
    /// fallback, so pass `--accel tcg` to ask for it on purpose.
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

    /// File receiving a raw copy of every byte on the debug serial
    /// line, host side, before the inspector frames it. Defaults to
    /// `debug-serial.log` in the runtime directory.
    #[arg(long)]
    debug_serial_log: Option<PathBuf>,

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

    /// Boot the guest with ACPI tables instead of a device tree, so the
    /// kernel takes its platform description from the MADT, the SPCR
    /// and the DSDT.
    #[arg(long, default_value_t = false)]
    acpi: bool,

    /// Put every virtio device behind a virtio-iommu, so its DMA only
    /// reaches the memory the kernel maps into its domain.
    #[arg(long, default_value_t = false)]
    iommu: bool,
    /// Transport carrying the inspector RPC to the guest debugger.
    ///
    /// `vsock` needs a host that can provide a vsock device; the session
    /// fails with an explanation rather than falling back to serial.
    #[arg(long, value_enum)]
    rpc_transport: Option<VmRpcTransport>,

    /// Context id given to the guest's vsock device.
    #[arg(long)]
    vsock_cid: Option<u32>,

    /// Create every virtio device with the packed virtqueue layout.
    #[arg(long, default_value_t = false)]
    virtio_packed: bool,

    /// Offer VIRTIO_F_IN_ORDER on every virtio device.
    #[arg(long, default_value_t = false)]
    virtio_in_order: bool,

    /// Attach the desktop devices: a virtio-GPU and the keyboard,
    /// tablet and mouse an input script drives.
    ///
    /// The scanout is read through QMP by the `screendump` action, so
    /// this is worth asking for with `--display none` as well as with a
    /// host window.
    #[arg(long, default_value_t = false)]
    desktop: bool,

    /// Host display backend QEMU opens for the guest's scanout.
    ///
    /// The default is `none`: the machine keeps whatever display device
    /// it was given and nothing is shown. A backend this host's QEMU was
    /// not built with is refused by QEMU rather than swapped for
    /// another.
    #[arg(long, value_enum)]
    display: Option<VmDisplayBackend>,

    /// Host audio backend the guest's sound device plays into:
    /// `none`, `wav:<path>`, or a backend of this host's own.
    ///
    /// Anything but `none` attaches a virtio-sound device to the guest.
    #[arg(long, value_name = "BACKEND")]
    audiodev: Option<String>,

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
    /// Write the guest's current scanout to a PNG.
    Screendump(ScreendumpCommand),
    /// Drive the guest's keyboard and pointer from an input script.
    Input(InputCommand),
    /// Build the guest image and the inspector, and stop there.
    ///
    /// The boots that follow reuse what this leaves in the target
    /// directory, so whatever is timing them — a per-class budget, a
    /// job step — measures guests rather than a cold cargo cache. With
    /// no `--boot-program` the bootfs carries every program, which is
    /// the superset every workload class boots from.
    Build,
    /// Write the guest kernel's LLVM raw profile and merge it.
    ///
    /// Only an instrumented kernel (`--profile-generate`) carries one; any
    /// other says so rather than handing back an empty profile.
    Profile(ProfileCommand),
    /// Print the guest kernel artifact this checkout would boot, and
    /// stop there.
    ///
    /// The path is resolved exactly as a boot resolves it — the
    /// workspace root, then the architecture and the profile — so a
    /// caller that has to identify the guest an image would boot does
    /// not have to rebuild the mapping from architecture to Cargo target
    /// and artifact name for itself. A paired benchmark run uses it to
    /// refuse two checkouts whose guest images turn out to be the same
    /// build, which the comparison between them could say nothing about.
    KernelPath,
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

/// Captures the machine's scanout through QMP.
///
/// The capture is QEMU's own view of the display device's surface, so a
/// headless session sees exactly what a host window would have shown and
/// a lane can keep the image as evidence of what the guest drew.
///
/// A capture of a guest that is *doing* something needs the guest to be
/// doing it at the time, and one session runs one action — so the guest
/// program and the input script that would otherwise need two boots are
/// options here. `--run` starts a program in the guest and leaves it
/// running; `--input` drives the desktop once it has started; the
/// captures follow.
#[derive(Debug, Clone, ClapArgs)]
pub(crate) struct ScreendumpCommand {
    /// Where to write the PNG. Repeat to take several captures, one
    /// after the other.
    #[arg(required = true)]
    paths: Vec<PathBuf>,

    /// How long to let the guest draw before each capture.
    #[arg(long, default_value_t = 0)]
    settle_seconds: u64,

    /// Guest path of a program to start before capturing, and leave
    /// running while the captures are taken.
    #[arg(long)]
    run: Option<String>,

    /// One argument for `--run`. Repeat for several, in order.
    ///
    /// Hyphens are allowed through: what follows is the *guest*
    /// program's own flag, and reading `--seconds` as one of this
    /// command's would make every guest program that takes options
    /// unreachable from here.
    #[arg(long = "run-arg", allow_hyphen_values = true)]
    run_args: Vec<String>,

    /// How long to wait, after the last capture, for the `--run`
    /// program to finish, so that whatever it printed reaches this
    /// session's output.
    ///
    /// A program that is still running when the wait ends is left
    /// running and the machine is torn down around it, which is what a
    /// program written to draw until somebody stops it wants.
    #[arg(long, default_value_t = 30)]
    run_wait_seconds: u64,

    /// An input script to run against the guest's keyboard and pointers
    /// once `--run` has started and before the first capture. Same
    /// grammar as the `input` action.
    #[arg(long)]
    input: Option<PathBuf>,

    /// How long to wait between the statements of `--input`.
    #[arg(long, default_value_t = 0)]
    input_interval_ms: u64,
}

/// Runs an input script against the guest's keyboard and pointer.
#[derive(Debug, Clone, ClapArgs)]
pub(crate) struct InputCommand {
    /// The script to run: one statement per line, `key <qcode>`,
    /// `abs <x> <y>`, `rel <dx> <dy>` or `btn <left|right|middle>
    /// <down|up>`, with `#` starting a comment.
    script: PathBuf,

    /// How long to wait between statements, so a guest that redraws
    /// between them has the chance to.
    #[arg(long, default_value_t = 0)]
    interval_ms: u64,
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

    /// Write the guest kernel's LLVM raw profile after the run, and the
    /// `llvm-profdata merge` of it beside the file. Only an instrumented
    /// kernel (`--profile-generate`) carries one (docs/pgo.md).
    #[arg(long)]
    llvm_raw_profile_output: Option<PathBuf>,
}

#[derive(Debug)]
struct ResolvedVmCommand {
    profile: &'static VmProfile,
    build: KernelBuildSpec,
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
    debug_serial_log: Option<PathBuf>,
    qemu_trace: Vec<String>,
    qemu_trace_log: Option<PathBuf>,
    qemu_arg: Vec<String>,
    runtime_dir: Option<PathBuf>,
    keep_runtime_dir: bool,
    acpi: bool,
    iommu: bool,
    desktop: bool,
    display: VmDisplayBackend,
    audiodev: VmAudioDev,
    virtio_devices: VirtioDeviceProfile,
    rpc_transport: VmRpcTransport,
    vsock_cid: u32,
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
    Screendump(ScreendumpCommand),
    Input(InputCommand),
    Profile(ProfileCommand),
}

impl ResolvedVmSessionCommand {
    /// Whether this action drives QEMU's machine protocol, and so needs
    /// a QMP socket whether or not the runtime directory is kept.
    ///
    /// The name of the action travels with the answer because it is what
    /// the refusal has to say when the socket is a hand-written endpoint
    /// the inspector cannot talk to.
    fn qmp_action(&self) -> Option<&'static str> {
        match self {
            Self::Balloon(_) => Some("balloon"),
            Self::Screendump(_) => Some("screendump"),
            Self::Input(_) => Some("input"),
            Self::Session(_) | Self::AotBench(_) | Self::WorkloadBench(_) | Self::Profile(_) => {
                None
            }
        }
    }
}

pub(crate) fn run(mut command: VmCommand) -> Result<(), VmError> {
    // The privileged network helpers provision the host, they do not boot
    // a guest, so they are dispatched before any build or QEMU work.
    match command.command.take() {
        Some(VmSessionCommand::NetSetup(setup)) => return Ok(network::run_setup(setup)?),
        Some(VmSessionCommand::NetTeardown(teardown)) => {
            return Ok(network::run_teardown(teardown)?);
        }
        // `build` produces artifacts and boots nothing, so it neither
        // preflights the QEMU host state nor spawns a guest.
        Some(VmSessionCommand::Build) => {
            let file = load_config_file(command.config.as_deref())?;
            return Ok(build_vm(&resolve_build(&command, &file, None)?)?);
        }
        // Answers from the build spec and boots nothing, so like `build`
        // it needs neither an accelerator nor a guest.
        Some(VmSessionCommand::KernelPath) => {
            let file = load_config_file(command.config.as_deref())?;
            let build = resolve_build(&command, &file, None)?;
            let kernel = resolve_kernel_path(command.kernel, file.kernel, &build)
                .map_err(VmConfigError::from)?;
            println!("{}", kernel.display());
            return Ok(());
        }
        session => command.command = session,
    }
    let command = resolve(command)?;
    ensure_qemu_command(&command)?;
    if !command.no_build {
        build_vm(&command.build)?;
    }
    let mut runtime = VmRuntime::spawn(&command)?;
    let result = connect_and_run(&command, &mut runtime);
    let runtime_dir = runtime.runtime_dir_path().display().to_string();
    // QEMU's own log is the only account of a machine it refused to
    // build or a device backend that never started, and the runtime
    // directory is about to go away, so a failed session reads it here.
    let report = result.is_err().then(|| runtime.qemu_report());
    runtime.shutdown();
    result.map_err(|source| VmError::Session {
        runtime_dir,
        report: report.unwrap_or_default(),
        source,
    })
}

/// Everything a guest build needs, and nothing a boot needs: the build
/// never starts QEMU, so it resolves no accelerator, CPU model, or host
/// state (#179).
#[derive(Clone, Debug, PartialEq, Eq)]
struct KernelBuildSpec {
    profile: &'static VmProfile,
    kind: KernelBuildProfile,
    /// The merged profile a [`KernelBuildProfile::ProfileUse`] build
    /// reads, absolute so that the `--config` override cargo receives
    /// does not depend on the directory the build is issued from.
    /// `None` for every other kind.
    profile_use: Option<PathBuf>,
    boot_programs: Vec<String>,
    no_compiler_plugin: bool,
}

fn debug_shortcut(command: &VmCommand, file: &VmConfigFile) -> bool {
    command.debug || file.debug.unwrap_or(false)
}

fn resolve_build(
    command: &VmCommand,
    file: &VmConfigFile,
    session_command: Option<&ResolvedVmSessionCommand>,
) -> Result<KernelBuildSpec, VmConfigError> {
    let arch = file.arch.unwrap_or(command.arch);
    let profile = arch.profile();
    let release = command.release || file.release.unwrap_or(false);
    let profile_generate = command.profile_generate || file.profile_generate.unwrap_or(false);
    let kernel_debug =
        debug_shortcut(command, file) || command.kernel_debug || file.kernel_debug.unwrap_or(false);
    if release && kernel_debug {
        return Err(VmConfigError::ReleaseWithKernelDebug);
    }
    if profile_generate && (release || kernel_debug) {
        return Err(VmConfigError::ProfileGenerateWithOtherProfile);
    }
    let profile_use = command
        .profile_use
        .clone()
        .or_else(|| file.profile_use.clone());
    if profile_use.is_some() && (profile_generate || kernel_debug) {
        return Err(VmConfigError::ProfileUseWithOtherProfile);
    }
    // Before the build kind exists, so that a profile from another
    // toolchain costs a sixteen-byte read rather than a kernel compile
    // that ends in an LLVM error naming no file.
    let profile_use = profile_use
        .map(|path| {
            helios_profdata::validate(&path)?;
            absolute_profile(&path)
        })
        .transpose()?;
    // A release build of the architecture performance is measured on
    // reads the fetched profile, so the kernel a developer boots, the
    // kernel the lanes measure and the kernel a release ships are the
    // same build (#226, #313). An explicit `--profile-use` is still the profile that
    // wins: it is how one profile is measured against another.
    // A session that builds nothing and names the image it boots needs no
    // profile: a profile describes a build, and there is none here.
    let builds_its_own_kernel =
        !(command.no_build && (command.kernel.is_some() || file.kernel.is_some()));
    let profile_use = match profile_use {
        Some(explicit) => Some(explicit),
        None if release && builds_its_own_kernel => release_kernel_profile(
            profile,
            &KernelProfileStore::new(&repo_root()?),
            command.without_kernel_profile,
        )?,
        None => None,
    };
    let kind = if profile_use.is_some() {
        KernelBuildProfile::ProfileUse
    } else if profile_generate {
        KernelBuildProfile::ProfileGenerate
    } else if release {
        KernelBuildProfile::Release
    } else if kernel_debug {
        KernelBuildProfile::KernelDebug
    } else {
        KernelBuildProfile::Debug
    };
    let mut boot_programs = file.boot_programs.clone();
    boot_programs.extend(command.boot_programs.iter().cloned());
    if let Some(ResolvedVmSessionCommand::WorkloadBench(bench)) = session_command {
        for program in crate::workload_bench::required_boot_programs(bench)? {
            if !boot_programs.contains(&program) {
                boot_programs.push(program);
            }
        }
    }
    let no_compiler_plugin = command.no_compiler_plugin || file.no_compiler_plugin.unwrap_or(false);
    Ok(KernelBuildSpec {
        profile,
        kind,
        profile_use,
        boot_programs,
        no_compiler_plugin,
    })
}

/// The profile a `--release` kernel of this target is built against.
///
/// `None` for a target whose releases carry none: performance is
/// measured on one architecture (AGENTS.md §3.6) and it is the one whose
/// releases publish a profile, so on the others a release build is a
/// plain release build rather than one silently missing its profile.
///
/// For the target that does carry one, an empty store is a refusal and
/// never a plain kernel wearing a PGO label: the error names the fetch
/// command and the release job that publishes the asset.
fn release_kernel_profile(
    profile: &VmProfile,
    store: &KernelProfileStore,
    without_kernel_profile: bool,
) -> Result<Option<PathBuf>, VmConfigError> {
    if !profile.release_kernel_profile {
        if without_kernel_profile {
            return Err(VmConfigError::WithoutKernelProfileOnPlainTarget {
                arch: arch_label(profile.arch),
            });
        }
        return Ok(None);
    }
    // The control of a PGO measurement: the same target, built the way
    // every other target's release kernel is. Asked for by name and
    // never reached by a missing store, which stays a refusal below.
    if without_kernel_profile {
        return Ok(None);
    }
    let (_, path) = store
        .fetched()
        .map_err(|source| VmConfigError::ReleaseKernelProfile {
            arch: arch_label(profile.arch),
            source,
        })?;
    Ok(Some(absolute_profile(&path)?))
}

/// The profile path as cargo will read it.
///
/// `-C profile-use` is resolved by rustc against its own working
/// directory, which is the workspace root rather than the one the
/// inspector was invoked from, so a relative path typed on the command
/// line has to be made absolute here or it names a different file in the
/// build than it did in the shell.
fn absolute_profile(path: &Path) -> Result<PathBuf, ProfileUseError> {
    path.canonicalize().map_err(|source| ProfileUseError::Read {
        path: path.display().to_string(),
        source,
    })
}

/// The kernel image a session boots: an explicit path from the command
/// line or the config file, else the workspace artifact of the resolved
/// build.
fn resolve_kernel_path(
    explicit: Option<PathBuf>,
    configured: Option<PathBuf>,
    build: &KernelBuildSpec,
) -> Result<PathBuf, WorkspaceRootError> {
    match explicit.or(configured) {
        Some(kernel) => Ok(kernel),
        None => default_kernel_path(build.profile.arch, build.kind.directory()),
    }
}

fn resolve(mut command: VmCommand) -> Result<ResolvedVmCommand, VmConfigError> {
    let file = load_config_file(command.config.as_deref())?;
    let session_command: Option<ResolvedVmSessionCommand> = command.command.take().map(Into::into);
    let build = resolve_build(&command, &file, session_command.as_ref())?;
    let profile = build.profile;
    let arch = profile.arch;
    let debug = debug_shortcut(&command, &file);
    let qemu_bin = command
        .qemu_bin
        .or(file.qemu_bin)
        .unwrap_or_else(|| PathBuf::from(profile.qemu_bin));
    let kernel = resolve_kernel_path(command.kernel, file.kernel, &build)?;
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
        default_accel(profile)?
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
        return Err(VmConfigError::ZeroDataDiskSize);
    }
    let gdb = command
        .gdb
        .or(file.gdb)
        .or_else(|| debug.then(|| DEFAULT_GDB_ENDPOINT.to_owned()));
    let gdb_wait = debug || command.gdb_wait || file.gdb_wait.unwrap_or(false);
    if gdb_wait && gdb.is_none() {
        return Err(VmConfigError::GdbWaitWithoutGdb);
    }
    let rpc_transport = command
        .rpc_transport
        .or(file.rpc_transport)
        .unwrap_or_default();
    let vsock_cid = command
        .vsock_cid
        .or(file.vsock_cid)
        .unwrap_or_else(crate::vsock::default_guest_cid);
    if rpc_transport == VmRpcTransport::Vsock {
        // Refuse here, before anything is built or booted: a host that
        // cannot provide the device would otherwise fail deep inside
        // QEMU with a message about a device model that does not exist.
        crate::vsock::preflight()?;
        if command.serial_stdio || command.serial_pty {
            return Err(VmConfigError::VsockWithSerialConsole);
        }
        if vsock_cid < 3 {
            return Err(VmConfigError::ReservedVsockCid);
        }
    }
    let monitor = command.monitor.or(file.monitor);
    if command.serial_stdio && matches!(monitor.as_deref(), Some("stdio")) {
        return Err(VmConfigError::SerialStdioWithMonitorStdio);
    }
    let qmp = command.qmp.or(file.qmp);
    let qemu_log = command.qemu_log.or(file.qemu_log);
    let debug_serial_log = command.debug_serial_log.or(file.debug_serial_log);
    let mut qemu_trace = file.qemu_trace;
    qemu_trace.extend(command.qemu_trace);
    let qemu_trace_log = command.qemu_trace_log.or(file.qemu_trace_log);
    let mut qemu_arg = file.qemu_arg;
    qemu_arg.extend(command.qemu_arg);
    let runtime_dir = command.runtime_dir.or(file.runtime_dir);
    let keep_runtime_dir =
        debug || command.keep_runtime_dir || file.keep_runtime_dir.unwrap_or(false);
    let acpi = command.acpi || file.acpi.unwrap_or(false);
    if acpi && profile.acpi_machine.is_none() {
        return Err(VmConfigError::AcpiUnavailable {
            arch: arch_label(arch),
        });
    }
    let iommu = command.iommu || file.iommu.unwrap_or(false);
    if iommu && profile.iommu.is_none() {
        return Err(VmConfigError::IommuUnavailable {
            arch: arch_label(arch),
        });
    }
    let desktop = command.desktop || file.desktop.unwrap_or(false);
    let display = command.display.or(file.display).unwrap_or_default();
    let audiodev = match command.audiodev.or(file.audiodev) {
        Some(text) => VmAudioDev::parse(&text)?,
        None => VmAudioDev::default(),
    };
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
        build,
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
        debug_serial_log,
        qemu_trace,
        qemu_trace_log,
        qemu_arg,
        runtime_dir,
        keep_runtime_dir,
        acpi,
        iommu,
        desktop,
        display,
        audiodev,
        virtio_devices,
        rpc_transport,
        vsock_cid,
        network,
        qemu_net,
        needs_qmp: session_command
            .as_ref()
            .is_some_and(|command| command.qmp_action().is_some()),
        command: session_command,
    })
}

fn load_config_file(path: Option<&Path>) -> Result<VmConfigFile, VmConfigError> {
    let path = path.map(Path::to_path_buf).or_else(default_config_path);
    let Some(path) = path else {
        return Ok(VmConfigFile::default());
    };
    if !path.is_file() {
        return Ok(VmConfigFile::default());
    }
    let bytes = fs::read(&path).map_err(|source| VmConfigError::ReadConfig {
        path: path.display().to_string(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| VmConfigError::DecodeConfig {
        path: path.display().to_string(),
        source,
    })
}

fn default_config_path() -> Option<PathBuf> {
    ProjectDirs::from("cool", "lexo", "helios-inspector")
        .map(|dirs| dirs.config_dir().join("vm.json"))
}

/// Why one of a profile's native accelerators cannot run on this host.
///
/// Each variant names the exact thing that was inspected, because the
/// whole point of refusing to fall back is that the caller learns which
/// check failed instead of silently getting an emulator.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AcceleratorUnavailable {
    #[error("`{accelerator}` needs a {required} host and this one is {host}")]
    HostArchitecture {
        accelerator: &'static str,
        required: &'static str,
        host: &'static str,
    },
    #[error("`hvf` needs a macOS host and this one runs {host_os}")]
    HvfHostOs { host_os: &'static str },
    #[error("`hvf` needs `kern.hv_support=1`; this host reports {reported}")]
    HvfUnsupported { reported: String },
    #[error("`hvf` needs `kern.hv_support`, which `sysctl` could not read: {source}")]
    HvfProbeFailed {
        #[source]
        source: io::Error,
    },
    #[error("`kvm` needs /dev/kvm and this host has no such node")]
    KvmNodeMissing,
    #[error("`kvm` needs read/write access to /dev/kvm: {source}")]
    KvmNodeUnusable {
        #[source]
        source: io::Error,
    },
}

/// No accelerator the profile calls native is available on this host.
///
/// Emulation is a deliberate choice, never a fallback: a lane that
/// quietly dropped to TCG reported HVF numbers that were TCG numbers for
/// weeks (#118), so the inspector refuses to pick the emulator for the
/// caller and says exactly what it inspected.
#[derive(Debug, thiserror::Error)]
#[error(
    "no accelerator the {arch} profile can use is available on this host:\n{}\n\
     pass `--accel tcg` to run under emulation on purpose",
    .checks.iter().map(|check| format!("  - {check}")).collect::<Vec<_>>().join("\n")
)]
pub(crate) struct NoNativeAccelerator {
    arch: &'static str,
    checks: Vec<AcceleratorUnavailable>,
}

/// Picks the profile's accelerator when the caller named none: the first
/// native accelerator this host can actually provide, and an error
/// naming every failed check when it can provide none. Profiles with no
/// native accelerator keep QEMU's own default.
fn default_accel(profile: &VmProfile) -> Result<Vec<String>, NoNativeAccelerator> {
    if profile.default_accel.is_empty() {
        return Ok(Vec::new());
    }
    let mut checks = Vec::new();
    for accel in profile.default_accel {
        match probe_accel(profile.arch, accel) {
            Ok(()) => return Ok(vec![(*accel).to_owned()]),
            Err(check) => checks.push(check),
        }
    }
    Err(NoNativeAccelerator {
        arch: arch_label(profile.arch),
        checks,
    })
}

/// Inspects the host state one named accelerator needs.
fn probe_accel(arch: VmArch, accel: &'static str) -> Result<(), AcceleratorUnavailable> {
    let required = match arch {
        VmArch::Aarch64 => "aarch64",
        VmArch::X86_64 => "x86_64",
        // No host this runs on executes riscv64 natively, so no riscv64
        // profile names a native accelerator in the first place.
        VmArch::Riscv64 => "riscv64",
    };
    if std::env::consts::ARCH != required {
        return Err(AcceleratorUnavailable::HostArchitecture {
            accelerator: accel,
            required,
            host: std::env::consts::ARCH,
        });
    }
    match accel {
        // The OS check alone is not enough: virtualized macOS hosts
        // without nested virtualization (e.g. CI runners) ship the
        // Hypervisor framework but report kern.hv_support=0, and QEMU
        // aborts with HV_UNSUPPORTED if HVF is requested anyway.
        "hvf" => {
            if std::env::consts::OS != "macos" {
                return Err(AcceleratorUnavailable::HvfHostOs {
                    host_os: std::env::consts::OS,
                });
            }
            let output = std::process::Command::new("sysctl")
                .args(["-n", "kern.hv_support"])
                .output()
                .map_err(|source| AcceleratorUnavailable::HvfProbeFailed { source })?;
            let reported = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if reported == "1" {
                Ok(())
            } else {
                Err(AcceleratorUnavailable::HvfUnsupported { reported })
            }
        }
        // Existence is not enough either: the node is root:kvm 0660 on a
        // stock udev, and a caller outside that group learns so here
        // rather than from QEMU aborting after the kernel build.
        "kvm" => match File::options().read(true).write(true).open("/dev/kvm") {
            Ok(_) => Ok(()),
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                Err(AcceleratorUnavailable::KvmNodeMissing)
            }
            Err(source) => Err(AcceleratorUnavailable::KvmNodeUnusable { source }),
        },
        _ => Ok(()),
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

fn ensure_qemu_command(command: &ResolvedVmCommand) -> Result<(), VmConfigError> {
    if let Some(shared_dir) = &command.shared_dir
        && !shared_dir.is_dir()
    {
        return Err(VmConfigError::SharedDirMissing {
            path: shared_dir.display().to_string(),
        });
    }
    if command.profile.arch == VmArch::Aarch64
        && command.accel.iter().any(|accel| accel == "hvf")
        && std::env::consts::ARCH != "aarch64"
    {
        return Err(VmConfigError::HvfNeedsAarch64Host);
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

fn build_vm(command: &KernelBuildSpec) -> Result<(), VmBuildError> {
    let repo_root = repo_root()?;
    run_step(
        "building helios-cli",
        cargo_build_command(&repo_root, command.kind.host())
            .arg("-p")
            .arg("helios-cli"),
    )?;
    let prebuild_manifest = run_kernel_prebuild(command)?;
    let kernel_label = match &command.profile_use {
        // Which profile a PGO kernel was built from is part of what it
        // is, so the build says it rather than leaving a release build
        // and a profile-guided one looking alike.
        Some(profile) => format!(
            "building {} kernel against {}",
            arch_label(command.profile.arch),
            profile.display()
        ),
        None => format!("building {} kernel", arch_label(command.profile.arch)),
    };
    run_step(
        &kernel_label,
        kernel_build_command(&repo_root, command)
            .env("HELIOS_KERNEL_PREBUILD_MANIFEST", &prebuild_manifest)
            .arg("--target")
            .arg(command.profile.cargo_target)
            .arg("--bin")
            .arg(command.profile.kernel_artifact_name),
    )?;
    run_step(
        "building inspector",
        cargo_build_command(&repo_root, command.kind.host())
            .arg("-p")
            .arg("helios-inspector"),
    )?;
    Ok(())
}

fn cargo_build_command(repo_root: &Path, build: KernelBuildProfile) -> Command {
    let mut command = Command::new("cargo");
    command.current_dir(repo_root).arg("build");
    match build {
        KernelBuildProfile::Debug => {}
        KernelBuildProfile::KernelDebug => {
            command.arg("--profile").arg("kernel-debug");
        }
        KernelBuildProfile::Release => {
            command.arg("--release");
        }
        KernelBuildProfile::ProfileGenerate => {
            command.arg("--profile").arg("profile-generate");
        }
        KernelBuildProfile::ProfileUse => {
            command.arg("--profile").arg("profile-use");
        }
    }
    command
}

/// The cargo invocation that builds the guest image itself.
///
/// An instrumented build differs from every other one by its rustflags, and
/// they arrive as a `--config` override rather than through `RUSTFLAGS`:
/// cargo joins a `--config` array with the one `.cargo/config.toml` sets for
/// the same target, where the environment variable would replace it and cost
/// the target its link arguments and ISA features.
fn kernel_build_command(repo_root: &Path, command: &KernelBuildSpec) -> Command {
    let mut cargo = cargo_build_command(repo_root, command.kind);
    if command.kind.instrumented() {
        cargo
            .arg("--config")
            .arg(profile_generate_rustflags(command.profile));
    }
    if let Some(profile) = &command.profile_use {
        cargo
            .arg("--config")
            .arg(profile_use_rustflags(command.profile, profile));
    }
    cargo
}

/// The `--config` override that turns a kernel build into an instrumented
/// one, for the target this profile boots.
fn profile_generate_rustflags(profile: &VmProfile) -> String {
    let mut flags = vec![
        // rustc would satisfy `__llvm_profile_runtime` by injecting
        // compiler-rt's profile runtime, which assumes a libc; the kernel
        // defines the symbol and writes the profile itself
        // (`kernel/src/profiling`).
        "-C".to_owned(),
        "profile-generate".to_owned(),
        "-Z".to_owned(),
        "no-profiler-runtime".to_owned(),
        // Value profiling calls the runtime on every indirect call and
        // grows the profile as new call targets appear, which the kernel's
        // window-at-a-time export cannot describe (docs/pgo.md).
        "-C".to_owned(),
        "llvm-args=-disable-vp=true".to_owned(),
        // Gates the kernel's profile runtime on the same flags that emit the
        // instrumentation, so the two can never be built apart.
        "--cfg".to_owned(),
        "helios_profile_generate".to_owned(),
    ];
    if let Some(script) = profile.profile_generate_linker_script {
        flags.push("-C".to_owned());
        flags.push(format!("link-arg=-T{script}"));
    }
    // The array is serialised rather than spelled out, so a flag that needs
    // quoting cannot silently produce a config cargo misreads.
    let flags = toml::Value::try_from(flags)
        .expect("a list of strings is a TOML array")
        .to_string();
    format!("target.\"{}\".rustflags={flags}", profile.cargo_target)
}

/// The `--config` override that builds this target's kernel against a
/// collected profile.
///
/// It arrives the same way the instrumented build's flags do, and for the
/// same reason: cargo joins a `--config` array with the one
/// `.cargo/config.toml` sets for the target, where `RUSTFLAGS` would
/// replace it and cost the target its link arguments and its ISA
/// features.
fn profile_use_rustflags(profile: &VmProfile, used: &Path) -> String {
    let flags = vec![
        "-C".to_owned(),
        format!("profile-use={}", used.display()),
        // A profile is a snapshot of one set of workloads on one revision,
        // so a kernel always has functions it says nothing about: a
        // function added since the collection, or one no collected
        // workload ever called. LLVM is silent about those by default,
        // which makes a profile that covers almost nothing look exactly
        // like one that covers everything. This asks it to name them, and
        // they stay warnings: a stale profile costs optimisation, never
        // the build.
        "-C".to_owned(),
        "llvm-args=-pgo-warn-missing-function".to_owned(),
        // The instrumented build collects with value profiling off
        // (`profile_generate_rustflags`), so every function in the
        // profile carries zero value sites while a default use build
        // expects as many as its indirect calls. LLVM reports each of
        // those as "inconsistent number of value sites ... possibly due
        // to the use of a stale profile" — a wrong diagnosis of a
        // correct profile, three hundred times over on the x86-64
        // kernel. The two halves state the same thing about value
        // profiling or they disagree about what the profile contains.
        "-C".to_owned(),
        "llvm-args=-disable-vp=true".to_owned(),
    ];
    let flags = toml::Value::try_from(flags)
        .expect("a list of strings is a TOML array")
        .to_string();
    format!("target.\"{}\".rustflags={flags}", profile.cargo_target)
}

fn run_kernel_prebuild(command: &KernelBuildSpec) -> Result<PathBuf, VmBuildError> {
    let cli = discover_helios_cli(command.kind)?;
    let repo_root = repo_root()?;
    let out_dir = repo_root
        .join("target")
        .join("kernel-prebuild")
        .join(command.profile.cargo_target)
        .join(command.kind.directory());
    let mut prebuild = Command::new(&cli);
    prebuild
        .current_dir(&repo_root)
        .arg("kernel-prebuild")
        .arg("--out-dir")
        .arg(&out_dir)
        .arg("--target")
        .arg(command.profile.cargo_target)
        .arg("--profile")
        .arg(command.kind.guest_programs())
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

fn connect_and_run(
    command: &ResolvedVmCommand,
    runtime: &mut VmRuntime,
) -> Result<(), VmSessionError> {
    let qmp_socket = runtime.qmp_socket.clone();
    let client = match runtime.take_transport()? {
        VmTransport::SerialSocket(socket_path) => {
            let socket = socket_path
                .to_str()
                .ok_or_else(|| VmSessionError::SocketPathNotUtf8 {
                    path: socket_path.display().to_string(),
                })?;
            connect_client(socket, command.baud, true)
                .map_err(|source| VmSessionError::Connect { over: "", source })?
        }
        VmTransport::SerialIo(io) => crate::runtime::block_on(async move {
            crate::ready::connect_after_boot(io)
                .await
                .map_err(|error| VmSessionError::Connect {
                    over: " over QEMU stdio serial",
                    source: ConnectError::from(error),
                })
        })?,
        VmTransport::VsockAfterSerial {
            serial_socket,
            guest_cid,
        } => {
            let socket =
                serial_socket
                    .to_str()
                    .ok_or_else(|| VmSessionError::SocketPathNotUtf8 {
                        path: serial_socket.display().to_string(),
                    })?;
            let baud = command.baud;
            crate::runtime::block_on(async move {
                let io = crate::serial::open(socket, baud)
                    .await
                    .map_err(ConnectError::from)?;
                let (read, _write) = io.into_split();
                let read = crate::ready::wait_for_boot(read)
                    .await
                    .map_err(ConnectError::from)?;
                // The console echo starts before the RPC connection, not
                // after it: nothing else reads the serial socket once the
                // RPC has moved off it, and a socket QEMU cannot write
                // into stops the guest console — but more importantly a
                // connection that never comes up is exactly when the
                // guest's own account of why is worth having.
                crate::ready::echo_serial_console(read);
                let (vsock_read, vsock_write) =
                    crate::vsock::connect(guest_cid, helios_inspector_protocol::VSOCK_RPC_PORT)
                        .await?;
                let mut client =
                    helios_inspector_protocol::transport::Client::new(vsock_read, vsock_write);
                crate::ready::wait_until_ready(&mut client)
                    .await
                    .map_err(ConnectError::from)?;
                Ok::<_, VsockSessionError>(client)
            })
            .map_err(|error| match error {
                VsockSessionError::Vsock { source } => VmSessionError::ConnectVsock { source },
                VsockSessionError::Connect { source } => VmSessionError::Connect {
                    over: " over vsock",
                    source,
                },
            })?
        }
    };
    match command.command.clone() {
        Some(ResolvedVmSessionCommand::AotBench(command)) => run_aot_bench(client, command),
        Some(ResolvedVmSessionCommand::WorkloadBench(workload_command)) => run_workload_bench(
            client,
            workload_command,
            VmProvenance {
                arch: arch_label(command.profile.arch),
                release: command.build.kind.optimised(),
                smp: command.smp,
                memory: command.memory.clone(),
                cpu: command.cpu.clone(),
                accel: command.accel.clone(),
            },
        ),
        Some(ResolvedVmSessionCommand::Balloon(balloon)) => {
            let socket = qmp_socket.ok_or(VmSessionError::NeedsQmp { action: "balloon" })?;
            run_balloon(client, balloon, &socket)
        }
        Some(ResolvedVmSessionCommand::Screendump(screendump)) => {
            let socket = qmp_socket.ok_or(VmSessionError::NeedsQmp {
                action: "screendump",
            })?;
            run_screendump(client, screendump, &socket)
        }
        Some(ResolvedVmSessionCommand::Input(input)) => {
            let socket = qmp_socket.ok_or(VmSessionError::NeedsQmp { action: "input" })?;
            run_input(input, &socket)
        }
        Some(ResolvedVmSessionCommand::Profile(profile)) => {
            crate::run_interruptible(async move { Ok(raw_profile::run(&client, &profile).await?) })
        }
        Some(ResolvedVmSessionCommand::Session(command)) => {
            Ok(run_connected(client, Some(command))?)
        }
        None => Ok(run_connected(client, None)?),
    }
}

/// The two ways the vsock hand-off can fail, kept apart so the message
/// says whether the guest never came up on the serial line or the vsock
/// connection itself was refused.
#[derive(Debug, thiserror::Error)]
enum VsockSessionError {
    #[error("{source}")]
    Vsock {
        #[from]
        source: crate::vsock::VsockConnectError,
    },
    #[error("{source}")]
    Connect {
        #[from]
        source: ConnectError,
    },
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
) -> Result<(), VmSessionError> {
    let mut qmp = QmpClient::connect(qmp_socket)?;
    report_balloon(&mut qmp, &mut client, "initial");

    for target in &command.targets {
        let bytes = qmp::parse_size(target)?;
        println!("{} balloon target {target}", style("set").cyan());
        qmp.set_balloon(bytes)
            .map_err(|source| VmSessionError::SetBalloonTarget {
                target: target.clone(),
                source,
            })?;
        settle_balloon(&mut client, bytes, command.settle_seconds);
        report_balloon(&mut qmp, &mut client, target);
        if command.hold_seconds != 0 {
            std::thread::sleep(Duration::from_secs(command.hold_seconds));
            report_balloon(&mut qmp, &mut client, &format!("{target} after hold"));
        }
    }
    Ok(())
}

/// Captures the guest's scanout into each file the caller named.
///
/// The guest is not asked anything: the capture is of the display
/// device's surface as QEMU holds it, which is the whole point of taking
/// it from the host. A session whose guest never drove the device still
/// produces an image — QEMU's blank scanout — and that is the evidence
/// that the machine had a display at all.
fn run_screendump(
    client: crate::serial::RpcClient,
    command: ScreendumpCommand,
    qmp_socket: &Path,
) -> Result<(), VmSessionError> {
    let Some(program) = command.run.clone() else {
        return capture_scanout(&command, qmp_socket);
    };
    let arguments = command.run_args.clone();
    let socket = qmp_socket.to_path_buf();
    let capture_command = command.clone();
    let wait = Duration::from_secs(command.run_wait_seconds);
    crate::runtime::block_on(async move {
        // The captures are blocking work on QEMU's monitor socket, and
        // the guest program has to keep running while they happen. A
        // thread for the blocking half and the executor for the guest
        // half is what lets one session do both: the thread's result
        // arrives on a channel the executor is woken by, so the guest's
        // RPC keeps being driven throughout.
        // One slot: the thread sends exactly one result and then ends.
        let (sender, receiver) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = sender.send_blocking(capture_scanout(&capture_command, &socket));
        });
        println!(
            "{} {} in the guest",
            style("started").cyan(),
            display_command(&program, &arguments)
        );
        let mut client = client;
        let guest = crate::programs::exec(&mut client, &program, &arguments);
        let mut guest = core::pin::pin!(guest);
        let captured = futures_lite::future::or(
            async { CaptureRace::Captured(receiver.recv().await) },
            async { CaptureRace::GuestExited(guest.as_mut().await) },
        )
        .await;
        match captured {
            CaptureRace::Captured(Ok(result)) => {
                result?;
                // Whatever the program printed is the guest's own account
                // of what it drew, and it is worth having beside the PNG.
                // A program that is still drawing when the wait ends is
                // left drawing.
                match crate::runtime::timeout(wait, guest).await {
                    Some(outcome) => report_guest_run(&program, outcome),
                    None => println!(
                        "{} {} is still running after {}s",
                        style("running").cyan(),
                        program,
                        command.run_wait_seconds
                    ),
                }
                Ok(())
            }
            CaptureRace::Captured(Err(_)) => Err(VmSessionError::CaptureThreadLost),
            CaptureRace::GuestExited(outcome) => {
                report_guest_run(&program, outcome);
                Err(VmSessionError::GuestProgramExitedEarly {
                    program: program.clone(),
                })
            }
        }
    })
}

/// Which half of a `screendump --run` finished first.
enum CaptureRace {
    Captured(Result<Result<(), VmSessionError>, async_channel::RecvError>),
    GuestExited(GuestRunOutcome),
}

/// How the `--run` program ended, once it did.
type GuestRunOutcome =
    Result<helios_inspector_protocol::system::programs::ExecResult, crate::programs::ProgramError>;

fn display_command(program: &str, arguments: &[String]) -> String {
    let mut rendered = String::from(program);
    for argument in arguments {
        rendered.push(' ');
        rendered.push_str(argument);
    }
    rendered
}

/// Print what a `--run` program said and how it ended.
fn report_guest_run(program: &str, outcome: GuestRunOutcome) {
    match outcome {
        Ok(result) => {
            print!("{}", String::from_utf8_lossy(&result.output.stdout));
            eprint!("{}", String::from_utf8_lossy(&result.output.stderr));
            println!(
                "{} {program} exited with {}",
                style("guest").cyan(),
                result.exit_code
            );
        }
        Err(error) => println!("{} {program}: {error}", style("guest").red()),
    }
}

/// Drive the desktop, if asked, and write each capture.
fn capture_scanout(command: &ScreendumpCommand, qmp_socket: &Path) -> Result<(), VmSessionError> {
    let mut qmp = QmpClient::connect(qmp_socket)?;
    if let Some(script) = &command.input {
        send_input_script(&mut qmp, script, command.input_interval_ms)?;
    }
    for path in &command.paths {
        if command.settle_seconds != 0 {
            std::thread::sleep(Duration::from_secs(command.settle_seconds));
        }
        // QEMU resolves a relative filename against its own working
        // directory, which is the inspector's only by accident of how it
        // was spawned; an absolute path names the same file either way.
        let path = std::path::absolute(path).map_err(|source| VmSessionError::ScreendumpPath {
            path: path.display().to_string(),
            source,
        })?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| VmSessionError::ScreendumpDirectory {
                path: parent.display().to_string(),
                source,
            })?;
        }
        qmp.screendump(&path)
            .map_err(|source| VmSessionError::Screendump {
                path: path.display().to_string(),
                source,
            })?;
        println!("{} {}", style("captured").green(), path.display());
    }
    Ok(())
}

/// Runs an input script against the guest's keyboard and pointer.
///
/// The whole script is parsed before the first event is sent: a script
/// with a typo in its last line is a script that would otherwise leave
/// the guest half-driven, in a state no later step could account for.
fn run_input(command: InputCommand, qmp_socket: &Path) -> Result<(), VmSessionError> {
    let mut qmp = QmpClient::connect(qmp_socket)?;
    send_input_script(&mut qmp, &command.script, command.interval_ms)
}

/// Parse `script` and send every statement in it.
///
/// Shared with `screendump --input`, which drives the desktop and then
/// captures it in one session rather than in two boots.
fn send_input_script(
    qmp: &mut QmpClient,
    script_path: &Path,
    interval_ms: u64,
) -> Result<(), VmSessionError> {
    let script = InputScript::read(script_path)?;
    for (index, statement) in script.statements().iter().enumerate() {
        for batch in statement.batches() {
            qmp.input_send_event(&batch)
                .map_err(|source| VmSessionError::SendInput {
                    path: script_path.display().to_string(),
                    statement: index + 1,
                    source,
                })?;
        }
        if interval_ms != 0 {
            std::thread::sleep(Duration::from_millis(interval_ms));
        }
    }
    println!(
        "{} {} statement(s) from {}",
        style("sent").green(),
        script.statements().len(),
        script_path.display()
    );
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
fn settle_balloon(client: &mut crate::serial::RpcClient, target: u64, seconds: u64) {
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
                return;
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
            return;
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
            return;
        }
        if std::time::Instant::now() >= deadline {
            println!(
                "{} guest was still moving when the {seconds}s wait ran out",
                style("settled").yellow()
            );
            return;
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

fn report_balloon(qmp: &mut QmpClient, client: &mut crate::serial::RpcClient, label: &str) {
    let Some(sample) = guest_stats(client) else {
        println!(
            "{} {label}: the guest did not answer",
            style("balloon").yellow()
        );
        return;
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
        "{} {label}: {guest} {host} machine-memory-available={}",
        style("balloon").green(),
        format_bytes(sample.memory.available_bytes)
    );
}

fn run_workload_bench(
    mut client: crate::serial::RpcClient,
    command: WorkloadBenchCommand,
    provenance: VmProvenance,
) -> Result<(), VmSessionError> {
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
        let seconds = command.workload_timeout_seconds;
        let before_profile = if collect_profile {
            guest_step_under_deadline("the profile reset", seconds, async {
                Ok::<_, VmSessionError>(
                    profiling_step(
                        "clear remote profile samples",
                        system_profiling::clear(&client),
                    )
                    .await?,
                )
            })
            .await?;
            guest_step_under_deadline("the profiler hand-off", seconds, async {
                Ok::<_, VmSessionError>(
                    profiling_step(
                        "enable remote profiling",
                        system_profiling::set_enabled(&client, true),
                    )
                    .await?,
                )
            })
            .await?;
            guest_step_under_deadline("the initial profile read", seconds, async {
                Ok::<_, VmSessionError>(
                    profiling_step(
                        "read initial remote profile samples",
                        system_profiling::folded(&client, &profile_filter, 0),
                    )
                    .await?,
                )
            })
            .await?
        } else {
            Vec::new()
        };

        if let Err(error) =
            crate::workload_bench::run_inner(&mut client, &command, &provenance).await
        {
            print_recent_guest_errors(&mut client, seconds).await;
            return Err(error.into());
        }

        if collect_profile {
            guest_step_under_deadline("the profiler stop", seconds, async {
                Ok::<_, VmSessionError>(
                    profiling_step(
                        "disable remote profiling",
                        system_profiling::set_enabled(&client, false),
                    )
                    .await?,
                )
            })
            .await?;
            let after_profile =
                guest_step_under_deadline("the final profile read", seconds, async {
                    Ok::<_, VmSessionError>(
                        profiling_step(
                            "read final remote profile samples",
                            system_profiling::folded(&client, &profile_filter, 0),
                        )
                        .await?,
                    )
                })
                .await?;
            let metrics = guest_step_under_deadline("the perf metric read", seconds, async {
                Ok::<_, VmSessionError>(
                    profiling_step(
                        "read final remote perf metrics",
                        system_profiling::metrics(&client, &metric_filter, 0),
                    )
                    .await?,
                )
            })
            .await?;
            write_requested_profile_outputs(&command, &before_profile, &after_profile, &metrics)?;
        }
        if let Some(output) = command.llvm_raw_profile_output() {
            raw_profile::collect_beside(&client, output).await?;
        }
        Ok(())
    })
}

/// Runs one profiling RPC, naming the step it was for.
///
/// Every profiling call around a bench run fails the same way — the
/// guest refused or never answered — so the step it was for is what
/// tells them apart in the message.
async fn profiling_step<T>(
    step: &'static str,
    call: impl std::future::Future<Output = Result<T, helios_inspector_protocol::RpcError>>,
) -> Result<T, ProfilingStepError> {
    call.await
        .map_err(|source| ProfilingStepError { step, source })
}

/// Fetches and prints the guest's recent tracing events (info level and
/// up, so boot markers frame the failure) so a failed remote operation
/// is diagnosable from the CLI without a second tracing session.
///
/// It runs under the same deadline as the run's other guest steps: the
/// call that brings us here is often a guest that stopped answering, and
/// a diagnostic that hangs replaces the failure it was fetched to
/// explain.
async fn print_recent_guest_errors(client: &mut crate::serial::RpcClient, seconds: u32) {
    let mut config = crate::system::TracingConfig::new();
    config.limit = 100;
    config.min_level = Some(helios_inspector_protocol::system::tracing::Level::Info);
    let fetched = guest_step_under_deadline("the tracing fetch", seconds, async {
        Ok::<_, WorkloadBenchError>(crate::system::fetch_tracing(client, &config).await?)
    })
    .await;
    match fetched {
        Ok(events) if events.is_empty() => {}
        Ok(events) => {
            eprintln!("recent guest tracing events:");
            for event in events {
                if let Ok(line) = crate::system::render_tracing_event(&event) {
                    eprintln!("  {line}");
                }
            }
        }
        Err(error) => eprintln!("failed to fetch guest tracing events: {error}"),
    }
}

fn run_aot_bench(
    mut client: crate::serial::RpcClient,
    command: AotBenchCommand,
) -> Result<(), VmSessionError> {
    crate::run_interruptible(async move {
        let wasm = fs::read(&command.wasm).map_err(|source| AotBenchError::ReadWasm {
            path: command.wasm.display().to_string(),
            source,
        })?;
        if command.iterations == 0 {
            return Err(AotBenchError::ZeroIterations.into());
        }
        debugger_fs::write(&client, &command.remote_path, &wasm, false)
            .await
            .map_err(|source| AotBenchError::Upload {
                path: command.wasm.display().to_string(),
                source,
            })?;
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
            profiling_step(
                "clear remote profile samples",
                system_profiling::clear(&client),
            )
            .await?;
            profiling_step(
                "enable remote profiling",
                system_profiling::set_enabled(&client, true),
            )
            .await?;
            profiling_step(
                "read initial remote profile samples",
                system_profiling::folded(&client, &profile_filter, 0),
            )
            .await?
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
            )
            .map_err(|source| AotBenchError::Report { source })?;
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
            .map_err(|source| AotBenchError::Compile { iteration, source })?;
            let result = match outcome {
                Ok(result) => result,
                Err(error) => {
                    // Surface the guest-side error events before failing:
                    // the RPC error kind alone (e.g. `Internal`) does not
                    // say which runtime operation actually failed.
                    print_recent_guest_errors(&mut client, DEFAULT_WORKLOAD_TIMEOUT_SECONDS).await;
                    return Err(AotBenchError::Refused {
                        iteration,
                        kind: error.kind,
                        detail: error.detail,
                    }
                    .into());
                }
            };
            let elapsed = started.elapsed();
            let mut stdout = std::io::stdout().lock();
            writeln!(
                stdout,
                "iteration={iteration} elapsed_ms={:.3} destination_path={}",
                elapsed.as_secs_f64() * 1_000.0,
                result.destination_path
            )
            .map_err(|source| AotBenchError::Report { source })?;
        }
        if collect_profile {
            profiling_step(
                "disable remote profiling",
                system_profiling::set_enabled(&client, false),
            )
            .await?;
            let after_profile = profiling_step(
                "read final remote profile samples",
                system_profiling::folded(&client, &profile_filter, 0),
            )
            .await?;
            let metrics = profiling_step(
                "read final remote perf metrics",
                system_profiling::metrics(&client, &metric_filter, 0),
            )
            .await?;
            write_requested_profile_outputs(&command, &before_profile, &after_profile, &metrics)?;
        }
        if let Some(output) = command.llvm_raw_profile_output() {
            raw_profile::collect_beside(&client, output).await?;
        }
        Ok(())
    })
}

trait ProfileOutputRequest {
    fn profile_output(&self) -> Option<&Path>;
    fn kernel_profile_output(&self) -> Option<&Path>;
    fn user_profile_output(&self) -> Option<&Path>;
    fn perf_metrics_output(&self) -> Option<&Path>;
    /// Where the guest kernel's own LLVM raw profile is written, when the
    /// run is collecting one.
    fn llvm_raw_profile_output(&self) -> Option<&Path>;
}

impl ProfileOutputRequest for AotBenchCommand {
    fn llvm_raw_profile_output(&self) -> Option<&Path> {
        self.llvm_raw_profile_output.as_deref()
    }

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
    fn llvm_raw_profile_output(&self) -> Option<&Path> {
        self.llvm_raw_profile_output.as_deref()
    }

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
) -> Result<(), ProfileOutputError> {
    use std::io::Write as _;

    for (output, scope, label) in [
        (command.profile_output(), None, "profile_output"),
        (
            command.kernel_profile_output(),
            Some(system_profiling::Scope::Kernel),
            "kernel_profile_output",
        ),
        (
            command.user_profile_output(),
            Some(system_profiling::Scope::User),
            "user_profile_output",
        ),
    ] {
        let Some(output) = output else {
            continue;
        };
        write_profile_output(output, before_profile, after_profile, scope)?;
        let mut stderr = std::io::stderr().lock();
        writeln!(stderr, "{label}={}", output.display())
            .map_err(|source| ProfileOutputError::Report { source })?;
    }
    if let Some(output) = command.perf_metrics_output() {
        write_perf_metrics_output(output, metrics)?;
        let mut stderr = std::io::stderr().lock();
        writeln!(stderr, "perf_metrics_output={}", output.display())
            .map_err(|source| ProfileOutputError::Report { source })?;
    }
    Ok(())
}

fn write_profile_output(
    output: &Path,
    before: &[system_profiling::FoldedSample],
    after: &[system_profiling::FoldedSample],
    scope: Option<system_profiling::Scope>,
) -> Result<(), ProfileOutputError> {
    fs::write(output, diff_folded_profile(before, after, scope)).map_err(|source| {
        ProfileOutputError::Write {
            path: output.display().to_string(),
            source,
        }
    })
}

fn write_perf_metrics_output(
    output: &Path,
    metrics: &[system_profiling::MetricSample],
) -> Result<(), ProfileOutputError> {
    let bytes =
        serde_json::to_vec_pretty(metrics).map_err(|source| ProfileOutputError::Encode {
            path: output.display().to_string(),
            source,
        })?;
    fs::write(output, bytes).map_err(|source| ProfileOutputError::Write {
        path: output.display().to_string(),
        source,
    })
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
) -> Result<PathBuf, VmRuntimeError> {
    match command.profile.boot_artifact {
        VmBootArtifactKind::KernelBinary => Ok(command.kernel.clone()),
        VmBootArtifactKind::LimineUefiDiskImage => prepare_limine_uefi_image(command, runtime_dir),
    }
}

fn prepare_limine_uefi_image(
    command: &ResolvedVmCommand,
    runtime_dir: Option<&Path>,
) -> Result<PathBuf, VmRuntimeError> {
    let kernel =
        fs::canonicalize(&command.kernel).map_err(|source| VmRuntimeError::CanonicalizeKernel {
            path: command.kernel.display().to_string(),
            source,
        })?;
    let image = match runtime_dir {
        Some(dir) => dir.join("kernel.uefi.img"),
        None => kernel.with_extension("uefi.img"),
    };
    let spinner = spinner(&format!(
        "building {} Limine UEFI disk image",
        arch_label(command.profile.arch)
    ));
    let cli = discover_helios_cli(command.build.kind)?;
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
        .map_err(|source| BuildStepError::Spawn {
            label: format!("{}", cli.display()),
            source,
        })?;
    if !status.success() {
        spinner.finish_and_clear();
        return Err(BuildStepError::Exited {
            label: "helios-cli limine-uefi-image".to_owned(),
            status,
        }
        .into());
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

/// The `helios-cli` a build of `kind` drives.
///
/// `HELIOS_CLI_BIN` first: a paired benchmark run pins one harness for two
/// guest checkouts and says so there.
///
/// Then the binary [`build_vm`] builds for this kind. An optimised kernel
/// build compiles `helios-cli` `--release`, and the inspector asking for
/// one need not be a release binary itself — `just build-instrumented` and
/// `just kernel-pgo-use` run it through `cargo run`, out of
/// `target/debug/`. Looking beside the running executable therefore finds
/// nothing on a clean checkout, which is why `profile-generate` had never
/// produced an artifact (#217). The kind names the profile, so the lookup
/// asks for that one rather than for whatever shares a directory with the
/// inspector.
///
/// The directory beside the inspector, and then `PATH`, still answer for
/// an inspector run from somewhere other than a workspace.
fn discover_helios_cli(kind: KernelBuildProfile) -> Result<PathBuf, ToolDiscoveryError> {
    if let Some(path) = std::env::var_os("HELIOS_CLI_BIN").map(PathBuf::from) {
        if path.is_file() {
            return Ok(path);
        }
        return Err(ToolDiscoveryError::CliBinNotAFile {
            path: path.display().to_string(),
        });
    }
    if let Ok(root) = repo_root() {
        let candidate = workspace_helios_cli(&root, kind);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    let current_exe =
        std::env::current_exe().map_err(|source| ToolDiscoveryError::CurrentExe { source })?;
    if let Some(candidate) = current_exe.parent().map(|dir| dir.join("helios-cli"))
        && candidate.is_file()
    {
        return Ok(candidate);
    }
    if let Some(candidate) = find_executable_in_path("helios-cli") {
        return Ok(candidate);
    }
    Err(ToolDiscoveryError::CliMissing)
}

/// Where [`build_vm`] leaves the `helios-cli` a build of `kind` needs.
///
/// One expression, so the build and the lookup cannot disagree about the
/// profile: `cargo_build_command` compiles it under `kind.host()`, and
/// this names the directory that profile writes into.
fn workspace_helios_cli(root: &Path, kind: KernelBuildProfile) -> PathBuf {
    root.join("target")
        .join(kind.host().directory())
        .join("helios-cli")
}

fn arch_label(arch: VmArch) -> &'static str {
    match arch {
        VmArch::Aarch64 => "aarch64",
        VmArch::Riscv64 => "riscv64",
        VmArch::X86_64 => "x86_64",
    }
}

fn run_step(label: &str, command: &mut Command) -> Result<(), BuildStepError> {
    let spinner = spinner(label);
    let status = command.status().map_err(|source| BuildStepError::Spawn {
        label: label.to_owned(),
        source,
    })?;
    if status.success() {
        spinner.finish_with_message(format!("{} {}", style("built").green(), label));
        return Ok(());
    }
    spinner.finish_and_clear();
    Err(BuildStepError::Exited {
        label: label.to_owned(),
        status,
    })
}

struct VmRuntime {
    transport: Option<VmTransport>,
    _serial_pty_slave: Option<fs::File>,
    /// The QMP socket the inspector created, when it owns one.
    qmp_socket: Option<PathBuf>,
    runtime_dir: VmRuntimeDir,
    /// Held for the life of the VM: dropping it takes the sockets with
    /// it, and QEMU is still bound to them until it exits.
    _socket_dir: VmSocketDir,
    qemu_log: PathBuf,
    child: Child,
}

enum VmTransport {
    SerialSocket(PathBuf),
    SerialIo(crate::serial::SerialIo),
    /// RPC over vsock, with the guest console left on the serial socket.
    ///
    /// The boot markers are printed before any RPC transport exists, so
    /// the serial socket is still what says when the guest is up; the
    /// vsock connection is opened once it has.
    VsockAfterSerial {
        serial_socket: PathBuf,
        guest_cid: u32,
    },
}

enum VmRuntimeDir {
    Temporary(TempDir),
    Persistent(PathBuf),
}

impl VmRuntimeDir {
    fn create(command: &ResolvedVmCommand) -> Result<Self, VmRuntimeError> {
        let create = |path: &Path| {
            fs::create_dir_all(path).map_err(|source| VmRuntimeError::CreateRuntimeDir {
                path: path.display().to_string(),
                source,
            })
        };
        if let Some(path) = &command.runtime_dir {
            create(path)?;
            return Ok(Self::Persistent(path.clone()));
        }
        if command.keep_runtime_dir {
            let path = default_persistent_runtime_dir()?;
            create(&path)?;
            return Ok(Self::Persistent(path));
        }
        tempfile::Builder::new()
            .prefix("helios-inspector-vm.")
            .tempdir()
            .map(Self::Temporary)
            .map_err(|source| VmRuntimeError::CreateTempRuntimeDir { source })
    }

    fn path(&self) -> &Path {
        match self {
            Self::Temporary(tempdir) => tempdir.path(),
            Self::Persistent(path) => path,
        }
    }

    fn is_persistent(&self) -> bool {
        matches!(self, Self::Persistent(_))
    }
}

/// A unix socket path the kernel will not accept.
///
/// The inspector owns the paths it hands QEMU, so it is the inspector
/// that refuses one that cannot fit, naming the path and the limit.
/// QEMU's own refusal comes after the guest image is built and reads as
/// a QEMU argument error rather than as what it is: run 33993027470 lost
/// a whole benchmark lane to `-monitor unix:…/monitor.sock` at 116
/// bytes.
#[derive(Debug, thiserror::Error)]
#[error("unix socket path {} is {length} bytes, and at most {limit} fit", .path.display())]
pub(crate) struct SocketPathTooLong {
    path: PathBuf,
    length: usize,
    limit: usize,
}

fn check_socket_path(path: &Path) -> Result<(), SocketPathTooLong> {
    let length = path.as_os_str().len();
    if length > UNIX_SOCKET_PATH_MAX {
        return Err(SocketPathTooLong {
            path: path.to_path_buf(),
            length,
            limit: UNIX_SOCKET_PATH_MAX,
        });
    }
    Ok(())
}

/// Where the sockets of one VM live.
///
/// Not in the runtime directory. That path belongs to the caller, names
/// the run for a human, and nests as deeply as the caller likes: a
/// benchmark that boots one guest per workload per image pushed it past
/// `sun_path` and QEMU refused the monitor socket with every guest image
/// already built (#173). The sockets go in a short directory of the
/// inspector's own, and the runtime directory carries a `sockets` link
/// to it so that a monitor, a QMP client or a raw serial reader still
/// finds them from the directory a lane retained.
enum VmSocketDir {
    Temporary(TempDir),
    Persistent(PathBuf),
}

impl VmSocketDir {
    /// `$XDG_RUNTIME_DIR` when the session has one, and the system
    /// temporary directory otherwise. Both are short by construction,
    /// and the first is already the place a session's sockets belong.
    fn base() -> PathBuf {
        match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
            Some(path) if path.is_dir() => path,
            _ => std::env::temp_dir(),
        }
    }

    fn create(runtime_dir: &VmRuntimeDir) -> Result<Self, VmRuntimeError> {
        let base = Self::base();
        let dir = tempfile::Builder::new()
            .prefix("helios-")
            .tempdir_in(&base)
            .map_err(|source| VmRuntimeError::CreateSocketDirectory {
                path: base.display().to_string(),
                source,
            })?;
        // The longest name any of the sockets takes, checked once: the
        // three of them are created at different points of the spawn and
        // the run must fail before the first, not between them.
        check_socket_path(&dir.path().join(MONITOR_SOCKET_NAME))?;
        let link = runtime_dir.path().join(SOCKET_DIR_LINK_NAME);
        if fs::symlink_metadata(&link).is_ok() {
            fs::remove_file(&link).map_err(|source| VmRuntimeError::ReplaceSocketLink {
                path: link.display().to_string(),
                source,
            })?;
        }
        symlink(dir.path(), &link).map_err(|source| VmRuntimeError::RecordSocketLink {
            target: dir.path().display().to_string(),
            link: link.display().to_string(),
            source,
        })?;
        if runtime_dir.is_persistent() {
            // A retained runtime directory is retained to be read later,
            // and a link into a directory that removed itself reads as
            // nothing at all.
            return Ok(Self::Persistent(dir.keep()));
        }
        Ok(Self::Temporary(dir))
    }

    fn path(&self) -> &Path {
        match self {
            Self::Temporary(tempdir) => tempdir.path(),
            Self::Persistent(path) => path,
        }
    }
}

impl VmRuntime {
    fn spawn(command: &ResolvedVmCommand) -> Result<Self, VmRuntimeError> {
        let runtime_dir = VmRuntimeDir::create(command)?;
        let socket_dir = VmSocketDir::create(&runtime_dir)?;
        let socket_path = match &command.socket {
            // A path the caller named is the caller's; it is still
            // checked, because the kernel will refuse it either way and
            // the inspector can say so before QEMU is started.
            Some(path) => {
                check_socket_path(path)?;
                path.clone()
            }
            None => socket_dir.path().join(DEBUG_SOCKET_NAME),
        };
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
        qemu.arg("-display").arg(command.display.token());
        if let Some(monitor) = monitor_endpoint(command, socket_dir.path())? {
            qemu.arg("-monitor").arg(monitor);
        } else {
            qemu.arg("-monitor").arg("none");
        }
        if let Some(qmp) = qmp_endpoint(command, socket_dir.path())? {
            qemu.arg("-qmp").arg(qmp);
        }
        qemu.arg("-machine").arg(machine(command));
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
            // The debug serial is a named chardev rather than the
            // `-serial unix:` shorthand so the host side can keep a raw
            // copy of the line: `logfile=` records every byte the
            // chardev accepts, before anything on the host frames it.
            // That is what tells a byte QEMU's 16550 model discarded
            // into a socket it could not write apart from one the
            // inspector's reader lost, and both are otherwise invisible
            // — the guest sees a successful transmit either way.
            let debug_serial_log = command
                .debug_serial_log
                .clone()
                .unwrap_or_else(|| runtime_dir.path().join(DEBUG_SERIAL_LOG_NAME));
            prepare_log_path(&debug_serial_log)?;
            qemu.arg("-chardev").arg(format!(
                "socket,id={DEBUG_SERIAL_CHARDEV},path={},server=on,wait=on,\
                 logfile={},logappend=off",
                qemu_option_value(&socket_path)?,
                qemu_option_value(&debug_serial_log)?,
            ));
            qemu.arg("-serial")
                .arg(format!("chardev:{DEBUG_SERIAL_CHARDEV}"));
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
        // QEMU sits in its own process group, so a signal aimed at the
        // inspector's group (a terminal's SIGINT, a driver's timeout kill)
        // never reaches it and the shutdown in `VmRuntime::drop` is the one
        // path that ends it. An inspector that dies by a signal runs no
        // `Drop`, so on Linux the kernel ends QEMU with the thread that
        // spawned it instead; `spawn` runs on the main thread, which lives
        // as long as the process. Bench run 34443906698 left nineteen
        // guests running that way, one per timed-out boot, under every
        // boot that followed.
        qemu.process_group(0);
        #[cfg(target_os = "linux")]
        {
            // SAFETY: the closure runs in the child between fork and exec;
            // `prctl` sets one flag on the calling task and allocates
            // nothing, takes no lock and touches no memory the parent
            // shares.
            unsafe {
                qemu.pre_exec(|| {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == 0 {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                });
            }
        }
        if command.serial_stdio {
            qemu.stdin(Stdio::piped());
            qemu.stdout(Stdio::piped());
        } else {
            qemu.stdin(Stdio::null());
            qemu.stdout(Stdio::from(fs::File::create(&qemu_log).map_err(
                |source| VmRuntimeError::QemuLog {
                    step: "create",
                    path: qemu_log.display().to_string(),
                    source,
                },
            )?));
        }
        qemu.stderr(Stdio::from(
            fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&qemu_log)
                .map_err(|source| VmRuntimeError::QemuLog {
                    step: "open",
                    path: format!("{} for append", qemu_log.display()),
                    source,
                })?,
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
        if let Some(host_share) = command.profile.host_share
            && let Some(shared_dir) = &command.shared_dir
        {
            configure_host_share(&mut qemu, host_share, shared_dir, command.virtio_devices);
        }
        if let Some(watchdog) = command.profile.watchdog {
            configure_watchdog(&mut qemu, watchdog);
        }
        configure_entropy_device(&mut qemu, command.profile.entropy, command.virtio_devices);
        configure_balloon(&mut qemu, command.profile.balloon, command.virtio_devices);
        if command.desktop {
            configure_display(&mut qemu, command.profile.display, command.virtio_devices);
            configure_input(&mut qemu, command.profile.input, command.virtio_devices);
        }
        configure_sound(
            &mut qemu,
            command.profile.sound,
            &command.audiodev,
            command.virtio_devices,
        );
        if command.rpc_transport == VmRpcTransport::Vsock {
            configure_vsock_device(
                &mut qemu,
                command.profile.vsock,
                command.vsock_cid,
                command.virtio_devices,
            );
        }

        let mut child = qemu.spawn().map_err(|source| VmRuntimeError::SpawnQemu {
            path: command.qemu_bin.display().to_string(),
            source,
        })?;
        let mut serial_pty_slave = None;
        let transport = if command.serial_stdio {
            let stdout = child
                .stdout
                .take()
                .ok_or(VmRuntimeError::ChildPipeMissing { pipe: "stdout" })?;
            let stdin = child
                .stdin
                .take()
                .ok_or(VmRuntimeError::ChildPipeMissing { pipe: "stdin" })?;
            VmTransport::SerialIo(crate::serial::open_child_stdio(stdout, stdin)?)
        } else if let Some(serial_pty) = serial_pty {
            serial_pty_slave = Some(serial_pty.slave);
            VmTransport::SerialIo(serial_pty.io)
        } else {
            wait_for_socket(&socket_path, &qemu_log, &mut child)?;
            match command.rpc_transport {
                VmRpcTransport::Serial => VmTransport::SerialSocket(socket_path),
                VmRpcTransport::Vsock => VmTransport::VsockAfterSerial {
                    serial_socket: socket_path,
                    guest_cid: command.vsock_cid,
                },
            }
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
            qmp_socket: qmp_socket_path(command, socket_dir.path())?,
            qemu_log,
            runtime_dir,
            _socket_dir: socket_dir,
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
            VmTransport::VsockAfterSerial { serial_socket, .. } => serial_socket,
            VmTransport::SerialIo(_) => {
                panic!("QEMU stdio serial transport does not have a socket path")
            }
        }
    }

    fn take_transport(&mut self) -> Result<VmTransport, VmRuntimeError> {
        self.transport
            .take()
            .ok_or(VmRuntimeError::TransportAlreadyTaken)
    }

    fn runtime_dir_path(&self) -> &Path {
        self.runtime_dir.path()
    }

    /// What QEMU has to say about a session that failed.
    ///
    /// QEMU writes its own diagnostics to the runtime directory's log,
    /// and they are the only account of two failures the RPC path
    /// cannot see: a machine QEMU refused to build, and a device whose
    /// host backend never started — a `vhost` backend that gave up
    /// leaves the device in place, answering configuration reads and
    /// carrying no traffic at all. The exit status joins the report when
    /// QEMU is already gone.
    fn qemu_report(&mut self) -> String {
        let log = fs::read(&self.qemu_log).unwrap_or_default();
        let start = log.len().saturating_sub(QEMU_REPORT_TAIL_BYTES);
        let tail = String::from_utf8_lossy(&log[start..]);
        let epitaph = match self.child.try_wait().ok().flatten() {
            Some(status) => format!("QEMU exited with {status}"),
            None => "QEMU was still running".to_owned(),
        };
        format!(
            "{epitaph}; {} says:\n{}",
            self.qemu_log.display(),
            tail.trim()
        )
    }

    fn shutdown(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn default_persistent_runtime_dir() -> Result<PathBuf, VmRuntimeError> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|source| VmRuntimeError::SystemTimeBeforeEpoch { source })?
        .as_millis();
    Ok(repo_root()?
        .join("target")
        .join("inspector-vm")
        .join(format!("run-{}-{timestamp}", std::process::id())))
}

fn monitor_endpoint(
    command: &ResolvedVmCommand,
    socket_dir: &Path,
) -> Result<Option<String>, SocketPathTooLong> {
    match &command.monitor {
        Some(monitor) => Ok(Some(monitor.clone())),
        None if command.keep_runtime_dir => {
            Ok(Some(unix_endpoint(socket_dir, MONITOR_SOCKET_NAME)?))
        }
        None => Ok(None),
    }
}

fn qmp_endpoint(
    command: &ResolvedVmCommand,
    socket_dir: &Path,
) -> Result<Option<String>, SocketPathTooLong> {
    match &command.qmp {
        Some(qmp) => Ok(Some(qmp.clone())),
        None if command.keep_runtime_dir || command.needs_qmp => {
            Ok(Some(unix_endpoint(socket_dir, QMP_SOCKET_NAME)?))
        }
        None => Ok(None),
    }
}

/// The QMP socket the inspector can talk to, when there is one it owns
/// the path of.
///
/// A hand-written `--qmp` endpoint is passed to QEMU verbatim and may
/// name a TCP port or a socket QEMU connects out to, so only the
/// `unix:<path>` form the inspector understands is offered back to the
/// commands that drive QMP themselves.
fn qmp_socket_path(
    command: &ResolvedVmCommand,
    socket_dir: &Path,
) -> Result<Option<PathBuf>, SocketPathTooLong> {
    let Some(endpoint) = qmp_endpoint(command, socket_dir)? else {
        return Ok(None);
    };
    let Some(path) = endpoint.strip_prefix("unix:") else {
        return Ok(None);
    };
    let path = path.split(',').next().unwrap_or(path);
    Ok(Some(PathBuf::from(path)))
}

fn unix_endpoint(socket_dir: &Path, name: &str) -> Result<String, SocketPathTooLong> {
    let path = socket_dir.join(name);
    check_socket_path(&path)?;
    Ok(format!("unix:{},server=on,wait=off", path.display()))
}

impl Drop for VmRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn prepare_socket_path(socket_path: &Path) -> Result<(), VmRuntimeError> {
    if let Some(parent) = socket_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|source| VmRuntimeError::CreateSocketDir {
            path: parent.display().to_string(),
            source,
        })?;
    }
    if !socket_path.exists() {
        return Ok(());
    }

    let metadata =
        fs::symlink_metadata(socket_path).map_err(|source| VmRuntimeError::InspectSocketPath {
            path: socket_path.display().to_string(),
            source,
        })?;
    if !metadata.file_type().is_socket() {
        return Err(VmRuntimeError::SocketPathNotASocket {
            path: socket_path.display().to_string(),
        });
    }
    fs::remove_file(socket_path).map_err(|source| VmRuntimeError::RemoveStaleSocket {
        path: socket_path.display().to_string(),
        source,
    })?;
    Ok(())
}

fn prepare_log_path(log_path: &Path) -> Result<(), VmRuntimeError> {
    if let Some(parent) = log_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|source| VmRuntimeError::CreateLogDir {
            path: parent.display().to_string(),
            source,
        })?;
    }
    Ok(())
}

/// Renders a path as one QEMU option value.
///
/// QEMU splits an option list on commas and reads a doubled comma as a
/// literal one, so a path that carries a comma has to be doubled or the
/// rest of it is parsed as another option.
fn qemu_option_value(path: &Path) -> Result<String, VmRuntimeError> {
    let text = path
        .to_str()
        .ok_or_else(|| VmRuntimeError::OptionPathNotUtf8 {
            path: path.display().to_string(),
        })?;
    Ok(text.replace(',', ",,"))
}

fn wait_for_socket(
    socket_path: &Path,
    qemu_log: &Path,
    child: &mut Child,
) -> Result<(), VmRuntimeError> {
    let started = std::time::Instant::now();
    while started.elapsed() < DEFAULT_SOCKET_WAIT {
        if socket_path.exists() {
            return Ok(());
        }
        if child
            .try_wait()
            .map_err(|source| VmRuntimeError::PollQemu { source })?
            .is_some()
        {
            let log =
                fs::read_to_string(qemu_log).map_err(|source| VmRuntimeError::ReadQemuLog {
                    path: qemu_log.display().to_string(),
                    source,
                })?;
            return Err(VmRuntimeError::QemuExitedBeforeSocket {
                socket: socket_path.display().to_string(),
                log,
            });
        }
        std::thread::sleep(SOCKET_POLL_INTERVAL);
    }
    Err(VmRuntimeError::SocketTimedOut {
        socket: socket_path.display().to_string(),
    })
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

/// The checkout this inspector run operates on.
///
/// Resolved from the current directory (or `HELIOS_WORKSPACE_ROOT`) every
/// time, never from the manifest directory the binary was compiled in: an
/// inspector reused from another worktree would otherwise build and boot that
/// worktree's kernel.
fn repo_root() -> Result<PathBuf, WorkspaceRootError> {
    Ok(WorkspaceRoot::resolve(None)?.path().to_path_buf())
}

fn default_kernel_path(arch: VmArch, profile_dir: &str) -> Result<PathBuf, WorkspaceRootError> {
    let profile = arch.profile();
    Ok(repo_root()?
        .join("target")
        .join(profile.cargo_target)
        .join(profile_dir)
        .join(profile.kernel_artifact_name))
}

/// Creates the scratch disk image this VM's guest kernel will own.
///
/// It lives in the runtime directory, so every VM gets a disk of its own
/// and a retained runtime directory keeps whatever the guest wrote.
fn prepare_data_disk(
    command: &ResolvedVmCommand,
    runtime_dir: &Path,
) -> Result<PathBuf, VmRuntimeError> {
    let image = runtime_dir.join("data.img");
    let file = fs::File::create(&image).map_err(|source| VmRuntimeError::CreateDataDisk {
        path: image.display().to_string(),
        source,
    })?;
    file.set_len(command.data_disk_bytes)
        .map_err(|source| VmRuntimeError::SizeDataDisk {
            path: image.display().to_string(),
            source,
        })?;
    Ok(image)
}

fn configure_firmware(
    qemu: &mut Command,
    command: &ResolvedVmCommand,
    runtime_dir: &Path,
) -> Result<(), VmRuntimeError> {
    if let Some(bios) = &command.bios {
        qemu.arg("-bios").arg(bios);
        return Ok(());
    }
    if command.profile.boot_artifact == VmBootArtifactKind::LimineUefiDiskImage {
        let code = discover_qemu_edk2_code(command.profile.arch, &command.qemu_bin)?;
        let vars_template = discover_qemu_edk2_vars(command.profile.arch, &code)?;
        let vars = runtime_dir.join(format!("edk2-{}-vars.fd", arch_label(command.profile.arch)));
        fs::copy(&vars_template, &vars).map_err(|source| VmRuntimeError::Edk2VarsCopy {
            vars: vars.display().to_string(),
            template: vars_template.display().to_string(),
            source,
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

fn discover_qemu_edk2_code(arch: VmArch, qemu_bin: &Path) -> Result<PathBuf, VmRuntimeError> {
    let env_var = match arch {
        VmArch::Aarch64 => "HELIOS_EDK2_AARCH64_CODE",
        VmArch::X86_64 => "HELIOS_EDK2_X86_64_CODE",
        VmArch::Riscv64 => panic!("riscv64 does not use EDK2 firmware"),
    };
    if let Some(path) = std::env::var_os(env_var).map(PathBuf::from) {
        if path.is_file() {
            return Ok(path);
        }
        return Err(VmRuntimeError::Edk2EnvNotAFile {
            env_var,
            path: path.display().to_string(),
        });
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
        .ok_or(VmRuntimeError::Edk2CodeMissing {
            arch: arch_label(arch),
            env_var,
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

fn discover_qemu_edk2_vars(arch: VmArch, code: &Path) -> Result<PathBuf, VmRuntimeError> {
    for vars in edk2_vars_filenames(arch).map(|name| code.with_file_name(name)) {
        if vars.is_file() {
            return Ok(vars);
        }
    }
    Err(VmRuntimeError::Edk2VarsMissing {
        arch: arch_label(arch),
        code: code.display().to_string(),
    })
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
/// The `-machine` option list this session boots, which is the ACPI
/// variant only when `--acpi` asked for it and the profile has one.
fn machine(command: &ResolvedVmCommand) -> &'static str {
    if command.acpi {
        return command.profile.acpi_machine.unwrap_or_else(|| {
            unreachable!("--acpi is refused on a machine profile that publishes no ACPI tables")
        });
    }
    command.profile.machine
}

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

/// Gives the guest a vsock device on the host's `vhost-vsock` backend.
///
/// There is no user-space vsock backend in QEMU: the host kernel carries
/// the packets, which is why this is reachable only after
/// [`crate::vsock::preflight`] has confirmed the host can.
fn configure_vsock_device(
    qemu: &mut Command,
    vsock: VmVsockProfile,
    guest_cid: u32,
    queues: VirtioDeviceProfile,
) {
    let mut device = QemuOptions::new(match vsock {
        VmVsockProfile::VhostVsockMmio => "vhost-vsock-device",
        VmVsockProfile::VhostVsockPci => "vhost-vsock-pci",
    });
    device.set("guest-cid", guest_cid);
    queues.apply(&mut device);
    qemu.arg("-device").arg(device.to_string());
}

/// Gives the guest a virtio-GPU, and makes it the only display adapter
/// the machine has.
///
/// The PCI machine creates a VGA adapter of its own unless it is told
/// not to, and QEMU's console 0 — the one `screendump` captures — would
/// then be that adapter's rather than the guest's. A capture of a
/// display device the guest never drove is worse than no capture: it
/// looks exactly like one the guest failed to draw into.
fn configure_display(qemu: &mut Command, display: VmDisplayProfile, queues: VirtioDeviceProfile) {
    if display == VmDisplayProfile::VirtioGpuPci {
        qemu.arg("-vga").arg("none");
    }
    let mut device = QemuOptions::new(match display {
        VmDisplayProfile::VirtioGpuMmio => "virtio-gpu-device",
        VmDisplayProfile::VirtioGpuPci => "virtio-gpu-pci",
    });
    apply_transport(
        display == VmDisplayProfile::VirtioGpuPci,
        queues,
        &mut device,
    );
    qemu.arg("-device").arg(device.to_string());
}

/// Gives the guest the three input devices a desktop is driven through.
///
/// The tablet carries absolute positions and the mouse relative ones,
/// which are different virtio-input devices rather than two modes of
/// one, so a session that can send both kinds of event needs both.
fn configure_input(qemu: &mut Command, input: VmInputProfile, queues: VirtioDeviceProfile) {
    let pci = input == VmInputProfile::VirtioInputPci;
    let devices: [&str; 3] = match input {
        VmInputProfile::VirtioInputMmio => [
            "virtio-keyboard-device",
            "virtio-tablet-device",
            "virtio-mouse-device",
        ],
        VmInputProfile::VirtioInputPci => [
            "virtio-keyboard-pci",
            "virtio-tablet-pci",
            "virtio-mouse-pci",
        ],
    };
    for name in devices {
        let mut device = QemuOptions::new(name);
        apply_transport(pci, queues, &mut device);
        qemu.arg("-device").arg(device.to_string());
    }
}

/// Gives the guest a virtio-sound device playing into the host backend
/// the session named, and nothing at all when it named none.
///
/// The backend is created first and the device names it: QEMU refuses a
/// virtio-sound device whose `audiodev` points at nothing, which is why
/// the two are one step rather than two.
fn configure_sound(
    qemu: &mut Command,
    sound: VmSoundProfile,
    audiodev: &VmAudioDev,
    queues: VirtioDeviceProfile,
) {
    let Some(backend) = audiodev.options() else {
        return;
    };
    qemu.arg("-audiodev").arg(backend.to_string());
    let mut device = QemuOptions::new(match sound {
        VmSoundProfile::VirtioSoundMmio => "virtio-sound-device",
        VmSoundProfile::VirtioSoundPci => "virtio-sound-pci",
    });
    device.set("audiodev", VmAudioDev::ID);
    apply_transport(sound == VmSoundProfile::VirtioSoundPci, queues, &mut device);
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
            VmSessionCommand::Screendump(command) => Self::Screendump(command),
            VmSessionCommand::Input(command) => Self::Input(command),
            VmSessionCommand::Profile(command) => Self::Profile(command),
            VmSessionCommand::Build
            | VmSessionCommand::KernelPath
            | VmSessionCommand::NetSetup(_)
            | VmSessionCommand::NetTeardown(_) => {
                unreachable!(
                    "the build, the artifact query and the network helpers never reach a guest session"
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{ErrorKind, Read};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    use helios_inspector_protocol::debugger::programs as debugger_programs;
    use helios_inspector_protocol::system::programs::ExecError;

    use super::*;

    /// Why one of the `#[ignore]`d guest integration tests failed.
    ///
    /// The tests drive the whole stack — build, boot, connect, run — so
    /// the failure keeps the typed error of whichever layer stopped,
    /// and adds only what a test can say that production code cannot:
    /// which arch, which program, and how long it waited.
    #[derive(Debug, thiserror::Error)]
    enum GuestTestFailure {
        #[error("{arch} watchdog self-test failed: {source}")]
        Watchdog {
            arch: &'static str,
            #[source]
            source: Box<Self>,
        },
        #[error("{0}")]
        Build(#[from] VmBuildError),
        #[error("{0}")]
        Runtime(#[from] VmRuntimeError),
        #[error("{0}")]
        Step(#[from] BuildStepError),
        #[error("{0}")]
        WorkspaceRoot(#[from] WorkspaceRootError),
        #[error("failed to spawn cargo for watchdog self-test kernel build: {source}")]
        SpawnCargo {
            #[source]
            source: io::Error,
        },
        #[error("watchdog self-test kernel build exited with status {status}")]
        CargoExited { status: std::process::ExitStatus },
        #[error("socket path must be valid UTF-8")]
        SocketPathNotUtf8,
        #[error("failed to connect {purpose} RPC client: {source}")]
        Connect {
            purpose: &'static str,
            #[source]
            source: ConnectError,
        },
        #[error("timed out waiting for {what}")]
        TimedOut { what: &'static str },
        #[error("{what} failed: {source}")]
        Rpc {
            what: &'static str,
            #[source]
            source: helios_inspector_protocol::RpcError,
        },
        #[error("{what} failed: {kind:?}: {detail}")]
        Refused {
            what: &'static str,
            kind: system_programs::ExecErrorKind,
            detail: String,
        },
        #[error("{0}")]
        Program(#[from] crate::programs::ProgramError),
        #[error("failed to configure serial socket read timeout: {source}")]
        SerialReadTimeout {
            #[source]
            source: io::Error,
        },
        #[error("failed to connect to QEMU debug serial socket {path}: {source}")]
        SerialConnect {
            path: String,
            #[source]
            source: io::Error,
        },
        #[error("failed while reading QEMU debug serial socket: {source}")]
        SerialRead {
            #[source]
            source: io::Error,
        },
        #[error(
            "timed out waiting for {expected} debugger run markers; observed {seen}; \
             reconnects: {reconnects}; recent stages: {stages}; recent lines: {lines}"
        )]
        MarkersTimedOut {
            expected: usize,
            seen: usize,
            reconnects: usize,
            stages: String,
            lines: String,
        },
    }

    impl GuestTestFailure {
        /// The guest's typed refusal of an `exec-path`, named by what was
        /// being run.
        fn refused(what: &'static str, error: ExecError) -> Self {
            Self::Refused {
                what,
                kind: error.kind,
                detail: error.detail,
            }
        }
    }

    const WATCHDOG_SELF_TEST_DELAY_MS: &str = "5000";

    const WATCHDOG_TIMEOUT_SECS: &str = "10";
    const WATCHDOG_STAGE_TIMEOUT: Duration = Duration::from_secs(120);
    const DIRECT_EXEC_TIMEOUT: Duration = Duration::from_secs(900);
    const SERIAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    const SERIAL_READ_TIMEOUT: Duration = Duration::from_millis(500);

    /// Renders the `-device` argument `configure_vsock_device` would
    /// pass QEMU, without spawning one.
    fn vsock_device_argument(vsock: VmVsockProfile, guest_cid: u32) -> String {
        let mut qemu = Command::new("true");
        configure_vsock_device(&mut qemu, vsock, guest_cid, VirtioDeviceProfile::default());
        qemu.get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn the_vsock_device_carries_its_context_id_on_each_bus() {
        assert_eq!(
            vsock_device_argument(VmVsockProfile::VhostVsockMmio, 42),
            "-device vhost-vsock-device,guest-cid=42"
        );
        assert_eq!(
            vsock_device_argument(VmVsockProfile::VhostVsockPci, 7),
            "-device vhost-vsock-pci,guest-cid=7"
        );
    }

    #[test]
    fn every_profile_names_the_vsock_bus_its_other_devices_use() {
        // A profile that attached a PCI vsock device to an MMIO-only
        // machine would fail at QEMU startup rather than at review.
        assert_eq!(
            AARCH64_VIRT_HVF_PROFILE.vsock,
            VmVsockProfile::VhostVsockMmio
        );
        assert_eq!(RISCV64_VM_PROFILE.vsock, VmVsockProfile::VhostVsockMmio);
        assert_eq!(X86_64_VM_PROFILE.vsock, VmVsockProfile::VhostVsockPci);
    }

    /// Renders the arguments one `configure_*` call would pass QEMU,
    /// without spawning one.
    fn rendered(configure: impl FnOnce(&mut Command)) -> Vec<String> {
        let mut qemu = Command::new("true");
        configure(&mut qemu);
        qemu.get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    /// A capture is only evidence if it is a capture of the guest's own
    /// display, so the PCI machine's default VGA adapter goes away with
    /// the same call that attaches the virtio-GPU.
    #[test]
    fn the_desktop_display_is_the_only_adapter_the_machine_has() {
        assert_eq!(
            rendered(|qemu| configure_display(
                qemu,
                VmDisplayProfile::VirtioGpuMmio,
                VirtioDeviceProfile::default()
            )),
            ["-device", "virtio-gpu-device"]
        );
        assert_eq!(
            rendered(|qemu| configure_display(
                qemu,
                VmDisplayProfile::VirtioGpuPci,
                VirtioDeviceProfile::default()
            )),
            ["-vga", "none", "-device", "virtio-gpu-pci"]
        );
    }

    /// Absolute and relative pointer events go to different devices, so
    /// a desktop that can be driven by both carries both.
    #[test]
    fn the_desktop_input_devices_cover_keys_and_both_pointers() {
        assert_eq!(
            rendered(|qemu| configure_input(
                qemu,
                VmInputProfile::VirtioInputMmio,
                VirtioDeviceProfile::default()
            )),
            [
                "-device",
                "virtio-keyboard-device",
                "-device",
                "virtio-tablet-device",
                "-device",
                "virtio-mouse-device"
            ]
        );
        assert_eq!(
            rendered(|qemu| configure_input(
                qemu,
                VmInputProfile::VirtioInputPci,
                VirtioDeviceProfile::default()
            )),
            [
                "-device",
                "virtio-keyboard-pci",
                "-device",
                "virtio-tablet-pci",
                "-device",
                "virtio-mouse-pci"
            ]
        );
    }

    /// The sound device is created against a backend, so it appears
    /// exactly when a session names one.
    #[test]
    fn a_sound_device_arrives_with_the_backend_it_plays_into() {
        assert_eq!(
            rendered(|qemu| configure_sound(
                qemu,
                VmSoundProfile::VirtioSoundPci,
                &VmAudioDev::None,
                VirtioDeviceProfile::default()
            )),
            Vec::<String>::new()
        );
        assert_eq!(
            rendered(|qemu| configure_sound(
                qemu,
                VmSoundProfile::VirtioSoundPci,
                &VmAudioDev::Wav {
                    path: PathBuf::from("/tmp/guest.wav")
                },
                VirtioDeviceProfile::default()
            )),
            [
                "-audiodev",
                "wav,path=/tmp/guest.wav,id=snd0",
                "-device",
                "virtio-sound-pci,audiodev=snd0"
            ]
        );
        assert_eq!(
            rendered(|qemu| configure_sound(
                qemu,
                VmSoundProfile::VirtioSoundMmio,
                &VmAudioDev::Host {
                    backend: "coreaudio".to_owned()
                },
                VirtioDeviceProfile::default()
            )),
            [
                "-audiodev",
                "coreaudio,id=snd0",
                "-device",
                "virtio-sound-device,audiodev=snd0"
            ]
        );
    }

    /// A profile that attached a PCI display to an MMIO-only machine
    /// would fail at QEMU startup rather than at review.
    #[test]
    fn every_profile_names_the_desktop_bus_its_other_devices_use() {
        for profile in [&AARCH64_VIRT_HVF_PROFILE, &RISCV64_VM_PROFILE] {
            assert_eq!(profile.display, VmDisplayProfile::VirtioGpuMmio);
            assert_eq!(profile.input, VmInputProfile::VirtioInputMmio);
            assert_eq!(profile.sound, VmSoundProfile::VirtioSoundMmio);
        }
        assert_eq!(X86_64_VM_PROFILE.display, VmDisplayProfile::VirtioGpuPci);
        assert_eq!(X86_64_VM_PROFILE.input, VmInputProfile::VirtioInputPci);
        assert_eq!(X86_64_VM_PROFILE.sound, VmSoundProfile::VirtioSoundPci);
    }

    /// Every lane that does not ask for a window boots the machine it
    /// booted before, so the default stays the headless one.
    #[test]
    fn the_display_backend_defaults_to_none() {
        assert_eq!(VmDisplayBackend::default(), VmDisplayBackend::None);
        assert_eq!(VmDisplayBackend::None.token(), "none");
        assert_eq!(VmDisplayBackend::Cocoa.token(), "cocoa");
        assert_eq!(VmDisplayBackend::Gtk.token(), "gtk");
        assert_eq!(VmDisplayBackend::Sdl.token(), "sdl");
    }

    #[test]
    fn an_audio_backend_is_named_as_qemu_names_it() {
        assert_eq!(
            VmAudioDev::parse("none").expect("no sound"),
            VmAudioDev::None
        );
        assert_eq!(
            VmAudioDev::parse("wav:/tmp/guest.wav").expect("the file sink"),
            VmAudioDev::Wav {
                path: PathBuf::from("/tmp/guest.wav")
            }
        );
        assert_eq!(
            VmAudioDev::parse("coreaudio").expect("a host backend"),
            VmAudioDev::Host {
                backend: "coreaudio".to_owned()
            }
        );
        VmAudioDev::parse("wav:").expect_err("the wav sink needs a file to write");
        VmAudioDev::parse("").expect_err("an empty backend names nothing");
        // A name carrying an option separator would smuggle further
        // options into the `-audiodev` list.
        VmAudioDev::parse("coreaudio,id=other").expect_err("a backend name is one word");
    }

    #[test]
    fn the_rpc_transport_defaults_to_the_serial_line() {
        // vsock needs a host that can provide the device, so it is opted
        // into rather than guessed at.
        assert_eq!(VmRpcTransport::default(), VmRpcTransport::Serial);
    }

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
            pcap: None,
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

    /// Every action that speaks to QEMU rather than to the guest has to
    /// say so, or it reaches a session with no socket to speak over.
    #[test]
    fn every_action_that_drives_the_monitor_asks_for_a_socket() {
        assert_eq!(
            ResolvedVmSessionCommand::Balloon(BalloonCommand {
                targets: Vec::new(),
                settle_seconds: 1,
                hold_seconds: 0,
            })
            .qmp_action(),
            Some("balloon")
        );
        assert_eq!(
            ResolvedVmSessionCommand::Screendump(ScreendumpCommand {
                paths: vec![PathBuf::from("desktop.png")],
                settle_seconds: 0,
                run: None,
                run_args: Vec::new(),
                run_wait_seconds: 0,
                input: None,
                input_interval_ms: 0,
            })
            .qmp_action(),
            Some("screendump")
        );
        assert_eq!(
            ResolvedVmSessionCommand::Input(InputCommand {
                script: PathBuf::from("desktop.input"),
                interval_ms: 0,
            })
            .qmp_action(),
            Some("input")
        );
        assert_eq!(
            ResolvedVmSessionCommand::Session(SessionCommand::Stats).qmp_action(),
            None
        );
    }

    /// A balloon session needs a socket to speak QMP over, whether or
    /// not the runtime directory is being kept.
    #[test]
    fn a_balloon_session_gets_a_qmp_socket_of_its_own() {
        let mut command = watchdog_test_command(VmArch::Aarch64);
        command.needs_qmp = true;
        let socket_dir = Path::new("/tmp/helios-vm");
        assert_eq!(
            qmp_socket_path(&command, socket_dir).unwrap(),
            Some(PathBuf::from("/tmp/helios-vm/qmp.sock"))
        );

        command.needs_qmp = false;
        assert_eq!(qmp_socket_path(&command, socket_dir).unwrap(), None);
    }

    /// The path QEMU is handed is the inspector's to refuse.
    ///
    /// A caller's runtime directory nests as deeply as the caller likes,
    /// and the benchmark suite's paired mode nests it per image and per
    /// workload. QEMU refuses a `sun_path` over the limit after the
    /// guest image is built, so the run has to fail before that, naming
    /// the path and the limit (#173).
    #[test]
    fn a_socket_path_over_the_unix_limit_is_refused_by_name() {
        let short = Path::new("/run/user/1000/helios-ab12cd/monitor.sock");
        check_socket_path(short).expect("a short path is handed to QEMU");

        let long = PathBuf::from("/home/runner/work/helios/helios")
            .join("a".repeat(UNIX_SOCKET_PATH_MAX))
            .join(MONITOR_SOCKET_NAME);
        let error = check_socket_path(&long).expect_err("a path over the limit is refused");
        let message = error.to_string();
        assert!(message.contains(&long.display().to_string()), "{message}");
        assert!(
            message.contains(&UNIX_SOCKET_PATH_MAX.to_string()),
            "{message}"
        );
    }

    /// The sockets never sit under the caller's runtime directory, so a
    /// deep one cannot push them over the limit.
    #[test]
    fn the_socket_directory_is_short_whatever_the_runtime_directory_is() {
        let deep = std::env::temp_dir()
            .join("helios-socket-dir-test")
            .join("bench-runtime/helios-baseline/helios-control-before/helios-control-before");
        fs::create_dir_all(&deep).expect("the deep runtime directory is created");
        let runtime_dir = VmRuntimeDir::Persistent(deep.clone());
        let socket_dir =
            VmSocketDir::create(&runtime_dir).expect("the socket directory is created");

        check_socket_path(&socket_dir.path().join(MONITOR_SOCKET_NAME))
            .expect("the socket path fits");
        let link = deep.join(SOCKET_DIR_LINK_NAME);
        assert_eq!(
            fs::read_link(&link).expect("the runtime directory records the socket directory"),
            socket_dir.path(),
        );
        fs::remove_dir_all(socket_dir.path()).expect("the socket directory is removed");
        fs::remove_dir_all(
            deep.parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .unwrap(),
        )
        .expect("the runtime directory is removed");
    }

    #[test]
    fn aarch64_boots_a_device_tree_unless_acpi_is_asked_for() {
        let mut command = watchdog_test_command(VmArch::Aarch64);
        assert_eq!(machine(&command), "virt,gic-version=3,acpi=off");
        command.acpi = true;
        assert_eq!(machine(&command), "virt,gic-version=3,acpi=on");
    }

    #[test]
    fn only_the_aarch64_machines_can_publish_acpi_tables() {
        assert_eq!(
            AARCH64_VIRT_HVF_PROFILE.acpi_machine,
            Some("virt,gic-version=3,acpi=on")
        );
        assert_eq!(
            AARCH64_VIRT_TCG_PROFILE.acpi_machine,
            AARCH64_VIRT_HVF_PROFILE.acpi_machine
        );
        // The riscv64 `virt` machine describes itself with a device
        // tree only, and q35 with ACPI only; neither has a second
        // description for `--acpi` to select.
        assert_eq!(RISCV64_VM_PROFILE.acpi_machine, None);
        assert_eq!(X86_64_VM_PROFILE.acpi_machine, None);
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

    /// Emulation is a choice, never a fallback. When the caller names no
    /// accelerator and the host provides none of the profile's, the
    /// error has to name every check that refused and how to ask for the
    /// emulator on purpose — a lane that silently dropped to TCG
    /// reported HVF numbers that were TCG numbers for weeks (#118).
    #[test]
    fn no_native_accelerator_names_every_failed_check() {
        let error = NoNativeAccelerator {
            arch: "aarch64",
            checks: vec![
                AcceleratorUnavailable::HvfHostOs { host_os: "linux" },
                AcceleratorUnavailable::KvmNodeMissing,
            ],
        }
        .to_string();
        assert!(error.contains("aarch64 profile"), "{error}");
        assert!(
            error.contains("needs a macOS host and this one runs linux"),
            "{error}"
        );
        assert!(
            error.contains("needs /dev/kvm and this host has no such node"),
            "{error}"
        );
        assert!(error.contains("pass `--accel tcg`"), "{error}");
    }

    /// A profile that names no native accelerator leaves the choice to
    /// QEMU instead of failing: nothing runs riscv64 natively here, so
    /// that profile never asked for an accelerator in the first place.
    #[test]
    fn a_profile_without_a_native_accelerator_resolves_to_nothing() {
        assert_eq!(
            default_accel(&RISCV64_VM_PROFILE).expect("riscv64 names no native accelerator"),
            Vec::<String>::new()
        );
    }

    /// The host-architecture check runs ahead of any device probe, so an
    /// accelerator asked for on the wrong host says so rather than
    /// blaming a node that could never have helped.
    #[test]
    fn a_foreign_host_architecture_is_named_before_any_device_probe() {
        let error = probe_accel(VmArch::Riscv64, "kvm")
            .expect_err("no host these tests run on executes riscv64 natively");
        assert!(
            matches!(
                error,
                AcceleratorUnavailable::HostArchitecture {
                    accelerator: "kvm",
                    required: "riscv64",
                    ..
                }
            ),
            "unexpected check: {error}"
        );
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
    #[test]
    fn a_build_resolves_without_an_accelerator() {
        // A build boots nothing, so it must resolve on a host that has no
        // accelerator for the target (#179). Pick whichever architecture
        // this host cannot accelerate; a host that accelerates all three
        // has nothing to prove here.
        let Some(arch) = [VmArch::X86_64, VmArch::Aarch64, VmArch::Riscv64]
            .into_iter()
            .find(|arch| default_accel(arch.profile()).is_err())
        else {
            return;
        };
        let mut command = minimal_command();
        command.arch = arch;
        let spec = resolve_build(&command, &VmConfigFile::default(), None)
            .expect("a build needs no accelerator");
        assert_eq!(spec.profile.arch, arch);
        assert_eq!(spec.kind, KernelBuildProfile::Debug);
    }

    /// A merged profile of the pinned toolchain's format, as
    /// `llvm-profdata merge` writes one: the magic, the version word, and
    /// nothing the header check reads past.
    fn pinned_profile(directory: &Path) -> PathBuf {
        let path = directory.join("helios-kernel.profdata");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x8169_666f_7270_6cffu64.to_le_bytes());
        bytes.extend_from_slice(&(13u64 | 1 << 56).to_le_bytes());
        fs::write(&path, bytes).expect("writing the profile header");
        path
    }

    #[test]
    fn a_profile_use_build_is_release_reading_that_profile() {
        let directory = tempfile::tempdir().expect("a temporary directory for the profile");
        let profile = pinned_profile(directory.path());
        let mut command = minimal_command();
        command.profile_use = Some(profile.clone());
        let spec = resolve_build(&command, &VmConfigFile::default(), None)
            .expect("a profile of the pinned format builds");
        assert_eq!(spec.kind, KernelBuildProfile::ProfileUse);
        assert_eq!(spec.kind.directory(), "profile-use");
        assert!(spec.kind.optimised(), "PGO is an optimised build");
        assert!(
            !spec.kind.instrumented(),
            "it consumes counters, it does not emit them"
        );
        assert_eq!(
            spec.profile_use.as_deref(),
            Some(
                profile
                    .canonicalize()
                    .expect("the profile exists")
                    .as_path()
            ),
        );
    }

    #[test]
    fn a_release_build_of_the_measured_target_refuses_an_empty_store() {
        let directory = tempfile::tempdir().expect("a temporary checkout");
        let store = KernelProfileStore::new(directory.path());
        let error = release_kernel_profile(&X86_64_VM_PROFILE, &store, false)
            .expect_err("a release kernel of the measured target is built against a profile");
        assert!(
            matches!(error, VmConfigError::ReleaseKernelProfile { .. }),
            "{error}"
        );
        assert!(
            error.to_string().contains(helios_profdata::FETCH_COMMAND),
            "the refusal says how to fill the store: {error}"
        );
    }

    #[test]
    fn the_plain_control_reads_no_profile_and_needs_no_store() {
        let directory = tempfile::tempdir().expect("a temporary checkout");
        let store = KernelProfileStore::new(directory.path());
        assert_eq!(
            release_kernel_profile(&X86_64_VM_PROFILE, &store, true)
                .expect("the control is built without a profile, whatever the store holds"),
            None,
        );
    }

    #[test]
    fn the_plain_control_is_refused_where_the_release_kernel_is_already_plain() {
        let directory = tempfile::tempdir().expect("a temporary checkout");
        let store = KernelProfileStore::new(directory.path());
        let error = release_kernel_profile(&RISCV64_VM_PROFILE, &store, true)
            .expect_err("a target whose release build reads no profile has no separate control");
        assert!(
            matches!(
                error,
                VmConfigError::WithoutKernelProfileOnPlainTarget { .. }
            ),
            "{error}"
        );
    }

    #[test]
    fn a_release_build_of_another_target_reads_no_profile() {
        let directory = tempfile::tempdir().expect("a temporary checkout");
        let store = KernelProfileStore::new(directory.path());
        for profile in [&RISCV64_VM_PROFILE, &AARCH64_VIRT_HVF_PROFILE] {
            assert_eq!(
                release_kernel_profile(profile, &store, false)
                    .expect("a target whose releases carry no profile needs no store"),
                None,
            );
        }
    }

    #[test]
    fn a_release_build_of_the_measured_target_reads_the_fetched_profile() {
        let directory = tempfile::tempdir().expect("a temporary checkout");
        let store = KernelProfileStore::new(directory.path());
        let tag = "helios-v0.1.0";
        let stored = store
            .profile_path(tag)
            .expect("a tag that keys a directory");
        fs::create_dir_all(stored.parent().expect("the tag's directory"))
            .expect("creating the tag's directory");
        pinned_profile(stored.parent().expect("the tag's directory"));
        store
            .publish(&helios_profdata::FetchedProfile::Release {
                repository: helios_profdata::RELEASE_REPOSITORY.to_owned(),
                tag: tag.to_owned(),
            })
            .expect("recording the profile in force");
        let profile = release_kernel_profile(&X86_64_VM_PROFILE, &store, false)
            .expect("the store holds a profile of the pinned format")
            .expect("the measured target reads it");
        assert_eq!(
            profile,
            stored.canonicalize().expect("the stored profile exists")
        );
    }

    #[test]
    fn the_profile_use_flags_name_the_profile_and_keep_a_gap_a_warning() {
        let directory = tempfile::tempdir().expect("a temporary directory for the profile");
        let profile = pinned_profile(directory.path());
        let flags = profile_use_rustflags(&X86_64_VM_PROFILE, &profile);
        assert!(
            flags.starts_with("target.\"x86_64-unknown-none\".rustflags="),
            "{flags}"
        );
        assert!(
            flags.contains(&format!("\"profile-use={}\"", profile.display())),
            "{flags}"
        );
        assert!(flags.contains("-pgo-warn-missing-function"), "{flags}");
        assert!(
            flags.contains("-disable-vp=true"),
            "the collection turns value profiling off and the use side has to agree: {flags}"
        );
    }

    #[test]
    fn a_profile_from_another_toolchain_is_refused_before_the_build() {
        let directory = tempfile::tempdir().expect("a temporary directory for the profile");
        let path = directory.path().join("stale.profdata");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x8169_666f_7270_6cffu64.to_le_bytes());
        bytes.extend_from_slice(&(9u64 | 1 << 56).to_le_bytes());
        fs::write(&path, bytes).expect("writing the stale profile header");
        let mut command = minimal_command();
        command.profile_use = Some(path);
        let error = resolve_build(&command, &VmConfigFile::default(), None)
            .expect_err("a profile this toolchain cannot read never reaches cargo");
        assert!(matches!(error, VmConfigError::ProfileUse(_)), "{error}");
        assert!(error.to_string().contains("version 9"), "{error}");
        assert!(error.to_string().contains("version 13"), "{error}");
    }

    #[test]
    fn profile_use_and_profile_generate_are_the_two_halves_and_never_one_build() {
        let directory = tempfile::tempdir().expect("a temporary directory for the profile");
        let mut command = minimal_command();
        command.profile_use = Some(pinned_profile(directory.path()));
        command.profile_generate = true;
        let error = resolve_build(&command, &VmConfigFile::default(), None)
            .expect_err("one build cannot both collect a profile and read one");
        assert!(
            matches!(error, VmConfigError::ProfileUseWithOtherProfile),
            "{error}"
        );
    }

    #[test]
    fn a_config_file_profile_use_is_refused_beside_kernel_debug() {
        let mut command = minimal_command();
        command.kernel_debug = true;
        let directory = tempfile::tempdir().expect("a temporary directory for the profile");
        let file = VmConfigFile {
            profile_use: Some(pinned_profile(directory.path())),
            ..VmConfigFile::default()
        };
        let error = resolve_build(&command, &file, None)
            .expect_err("the config file's profile is refused the way the flag's is");
        assert!(
            matches!(error, VmConfigError::ProfileUseWithOtherProfile),
            "{error}"
        );
    }

    #[test]
    fn the_cli_a_build_needs_is_the_one_that_build_compiles() {
        // An optimised kernel build compiles helios-cli --release, and the
        // inspector driving it can be the debug binary `cargo run`
        // produces, so the lookup asks for the profile rather than for
        // whatever shares a directory with it (#217).
        let root = Path::new("/workspace");
        for kind in [
            KernelBuildProfile::Release,
            KernelBuildProfile::ProfileGenerate,
            KernelBuildProfile::ProfileUse,
        ] {
            assert_eq!(
                workspace_helios_cli(root, kind),
                Path::new("/workspace/target/release/helios-cli"),
                "{kind:?}"
            );
        }
        for kind in [KernelBuildProfile::Debug, KernelBuildProfile::KernelDebug] {
            assert_eq!(
                workspace_helios_cli(root, kind),
                Path::new("/workspace/target/debug/helios-cli"),
                "{kind:?}"
            );
        }
    }

    fn minimal_command() -> VmCommand {
        VmCommand {
            rpc_transport: None,
            vsock_cid: None,
            arch: VmArch::X86_64,
            debug: false,
            release: false,
            profile_generate: false,
            profile_use: None,
            without_kernel_profile: false,
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
            // Every caller states its accelerator; these resolve on any
            // host, so they ask for the emulator on purpose.
            accel: vec!["tcg".to_owned()],
            shared_dir: None,
            data_disk_size: None,
            gdb: None,
            gdb_wait: false,
            monitor: None,
            qmp: None,
            qemu_log: None,
            debug_serial_log: None,
            qemu_trace: Vec::new(),
            qemu_trace_log: None,
            qemu_arg: Vec::new(),
            boot_programs: Vec::new(),
            no_compiler_plugin: false,
            runtime_dir: None,
            keep_runtime_dir: false,
            acpi: false,
            iommu: false,
            virtio_packed: false,
            virtio_in_order: false,
            desktop: false,
            display: None,
            audiodev: None,
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
            rpc_transport: None,
            vsock_cid: None,
            arch: VmArch::X86_64,
            debug: false,
            release: false,
            profile_generate: false,
            profile_use: None,
            without_kernel_profile: false,
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
            // Every caller states its accelerator; these resolve on any
            // host, so they ask for the emulator on purpose.
            accel: vec!["tcg".to_owned()],
            shared_dir: None,
            data_disk_size: None,
            gdb: None,
            gdb_wait: false,
            monitor: None,
            qmp: None,
            qemu_log: None,
            debug_serial_log: None,
            qemu_trace: Vec::new(),
            qemu_trace_log: None,
            qemu_arg: Vec::new(),
            boot_programs: Vec::new(),
            no_compiler_plugin: false,
            runtime_dir: None,
            keep_runtime_dir: false,
            acpi: false,
            iommu: false,
            virtio_packed: false,
            virtio_in_order: false,
            desktop: false,
            display: None,
            audiodev: None,
            network: default_network_args(),
            command: None,
        };

        let resolved = resolve(command).expect("VM command resolution must succeed");
        assert_eq!(resolved.profile, &X86_64_VM_PROFILE);
        assert_eq!(resolved.build.kind, KernelBuildProfile::Debug);
        assert_eq!(resolved.smp, DEFAULT_X86_SMP);
        assert_eq!(resolved.memory, DEFAULT_X86_MEMORY);
        assert_eq!(resolved.bios, None);
        assert_eq!(
            resolved.kernel,
            repo_root()
                .expect("workspace root must resolve")
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
            rpc_transport: None,
            vsock_cid: None,
            arch: VmArch::X86_64,
            debug: true,
            release: false,
            profile_generate: false,
            profile_use: None,
            without_kernel_profile: false,
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
            // Every caller states its accelerator; these resolve on any
            // host, so they ask for the emulator on purpose.
            accel: vec!["tcg".to_owned()],
            shared_dir: None,
            data_disk_size: None,
            gdb: None,
            gdb_wait: false,
            monitor: None,
            qmp: None,
            qemu_log: None,
            debug_serial_log: None,
            qemu_trace: Vec::new(),
            qemu_trace_log: None,
            qemu_arg: Vec::new(),
            boot_programs: Vec::new(),
            no_compiler_plugin: false,
            runtime_dir: None,
            keep_runtime_dir: false,
            acpi: false,
            iommu: false,
            virtio_packed: false,
            virtio_in_order: false,
            desktop: false,
            display: None,
            audiodev: None,
            network: default_network_args(),
            command: None,
        };

        let resolved = resolve(command).expect("VM debug command resolution must succeed");
        assert_eq!(resolved.build.kind, KernelBuildProfile::KernelDebug);
        assert_eq!(resolved.gdb.as_deref(), Some(DEFAULT_GDB_ENDPOINT));
        assert!(resolved.gdb_wait);
        assert!(resolved.keep_runtime_dir);
        assert_eq!(
            resolved.kernel,
            repo_root()
                .expect("workspace root must resolve")
                .join("target")
                .join(X86_64_VM_PROFILE.cargo_target)
                .join("kernel-debug")
                .join(X86_64_VM_PROFILE.kernel_artifact_name)
        );
    }

    #[test]
    #[ignore = "requires qemu and cross-compiled kernels"]
    fn watchdog_self_test_resets_x86_and_riscv() -> Result<(), GuestTestFailure> {
        for arch in [VmArch::X86_64, VmArch::Riscv64] {
            assert_watchdog_reset(arch).map_err(|source| GuestTestFailure::Watchdog {
                arch: arch_label(arch),
                source: Box::new(source),
            })?;
        }
        Ok(())
    }

    fn assert_watchdog_reset(arch: VmArch) -> Result<(), GuestTestFailure> {
        build_watchdog_test_kernel(arch)?;
        let command = watchdog_test_command(arch);
        let mut runtime = VmRuntime::spawn(&command)?;
        wait_for_stage_occurrences(runtime.socket_path(), DEBUGGER_RUN_STAGE_MARKER, 2)?;
        runtime.shutdown();
        Ok(())
    }

    fn build_watchdog_test_kernel(arch: VmArch) -> Result<(), GuestTestFailure> {
        let command = watchdog_test_command(arch);
        run_step(
            "building helios-cli",
            cargo_build_command(&repo_root()?, command.build.kind.host())
                .arg("-p")
                .arg("helios-cli"),
        )?;
        let prebuild_manifest = run_kernel_prebuild(&command.build)?;
        let status = std::process::Command::new("cargo")
            .current_dir(repo_root()?)
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
            .map_err(|source| GuestTestFailure::SpawnCargo { source })?;
        if status.success() {
            return Ok(());
        }
        Err(GuestTestFailure::CargoExited { status })
    }

    fn watchdog_test_command(arch: VmArch) -> ResolvedVmCommand {
        let profile = arch.profile();
        ResolvedVmCommand {
            profile,
            build: KernelBuildSpec {
                profile,
                kind: KernelBuildProfile::Debug,
                profile_use: None,
                boot_programs: vec!["debugger".to_owned()],
                no_compiler_plugin: true,
            },
            qemu_bin: PathBuf::from(profile.qemu_bin),
            kernel: default_kernel_path(arch, KernelBuildProfile::Debug.directory())
                .expect("workspace root must resolve"),
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
            debug_serial_log: None,
            qemu_trace: Vec::new(),
            qemu_trace_log: None,
            qemu_arg: Vec::new(),
            runtime_dir: None,
            keep_runtime_dir: false,
            acpi: false,
            iommu: false,
            desktop: false,
            display: VmDisplayBackend::None,
            audiodev: VmAudioDev::None,
            needs_qmp: false,
            virtio_devices: VirtioDeviceProfile::default(),
            rpc_transport: VmRpcTransport::Serial,
            vsock_cid: crate::vsock::default_guest_cid(),
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
    fn exec_path_runs_host_curl_in_riscv_release_vm() -> Result<(), GuestTestFailure> {
        let command = direct_exec_command(VmArch::Riscv64);
        build_vm(&command.build)?;
        let mut runtime = VmRuntime::spawn(&command)?;
        let socket = runtime
            .socket_path()
            .to_str()
            .ok_or(GuestTestFailure::SocketPathNotUtf8)?;
        let client = connect_client(socket, DEFAULT_BAUD, true).map_err(|source| {
            GuestTestFailure::Connect {
                purpose: "direct-exec",
                source,
            }
        })?;

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
        .ok_or(GuestTestFailure::TimedOut {
            what: "direct curl exec-path result",
        })?
        .map_err(|source| GuestTestFailure::Rpc {
            what: "direct curl exec-path",
            source,
        })?
        .map_err(|error| GuestTestFailure::refused("direct curl exec-path", error))?;
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
    fn exec_path_runs_host_cpython_in_riscv_release_vm() -> Result<(), GuestTestFailure> {
        let command = direct_exec_command(VmArch::Riscv64);
        build_vm(&command.build)?;
        let mut runtime = VmRuntime::spawn(&command)?;
        let socket = runtime
            .socket_path()
            .to_str()
            .ok_or(GuestTestFailure::SocketPathNotUtf8)?;
        let client = connect_client(socket, DEFAULT_BAUD, true).map_err(|source| {
            GuestTestFailure::Connect {
                purpose: "direct-exec",
                source,
            }
        })?;

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
        .ok_or(GuestTestFailure::TimedOut {
            what: "direct CPython exec-path result",
        })?
        .map_err(|source| GuestTestFailure::Rpc {
            what: "direct CPython exec-path",
            source,
        })?
        .map_err(|error| GuestTestFailure::refused("direct CPython exec-path", error))?;
        assert_eq!(python.exit_code, 0, "CPython exited non-zero: {python:?}");
        let python_stdout = String::from_utf8_lossy(&python.output.stdout);
        assert_eq!(python_stdout.trim(), "42", "unexpected CPython stdout");

        runtime.shutdown();
        Ok(())
    }

    #[test]
    #[ignore = "requires qemu, a release riscv guest build, and staged host artifacts"]
    fn shell_runs_host_cpython_in_riscv_release_vm() -> Result<(), GuestTestFailure> {
        let command = direct_exec_command(VmArch::Riscv64);
        build_vm(&command.build)?;
        let mut runtime = VmRuntime::spawn(&command)?;
        let socket = runtime
            .socket_path()
            .to_str()
            .ok_or(GuestTestFailure::SocketPathNotUtf8)?;
        let mut client = connect_client(socket, DEFAULT_BAUD, true).map_err(|source| {
            GuestTestFailure::Connect {
                purpose: "shell",
                source,
            }
        })?;

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
        .ok_or(GuestTestFailure::TimedOut {
            what: "shell CPython result",
        })??;
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
            build: KernelBuildSpec {
                profile,
                kind: KernelBuildProfile::Release,
                profile_use: None,
                boot_programs: Vec::new(),
                no_compiler_plugin: false,
            },
            qemu_bin: PathBuf::from(profile.qemu_bin),
            kernel: default_kernel_path(arch, KernelBuildProfile::Release.directory())
                .expect("workspace root must resolve"),
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
            shared_dir: Some(repo_root().expect("workspace root must resolve")),
            data_disk_bytes: DEFAULT_DATA_DISK_BYTES,
            gdb: None,
            gdb_wait: false,
            monitor: None,
            qmp: None,
            qemu_log: None,
            debug_serial_log: None,
            qemu_trace: Vec::new(),
            qemu_trace_log: None,
            qemu_arg: Vec::new(),
            runtime_dir: None,
            keep_runtime_dir: false,
            acpi: false,
            iommu: false,
            desktop: false,
            display: VmDisplayBackend::None,
            audiodev: VmAudioDev::None,
            needs_qmp: false,
            virtio_devices: VirtioDeviceProfile::default(),
            rpc_transport: VmRpcTransport::Serial,
            vsock_cid: crate::vsock::default_guest_cid(),
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

    fn connect_serial_socket(socket_path: &Path) -> Result<UnixStream, GuestTestFailure> {
        let started = Instant::now();
        loop {
            match UnixStream::connect(socket_path) {
                Ok(stream) => {
                    stream
                        .set_read_timeout(Some(SERIAL_READ_TIMEOUT))
                        .map_err(|source| GuestTestFailure::SerialReadTimeout { source })?;
                    return Ok(stream);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::ConnectionRefused | ErrorKind::NotFound
                    ) && started.elapsed() < SERIAL_CONNECT_TIMEOUT => {}
                Err(source) => {
                    return Err(GuestTestFailure::SerialConnect {
                        path: socket_path.display().to_string(),
                        source,
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
    ) -> Result<(), GuestTestFailure> {
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
                Err(source) => {
                    return Err(GuestTestFailure::SerialRead { source });
                }
            }
        }

        Err(GuestTestFailure::MarkersTimedOut {
            expected,
            seen,
            reconnects,
            stages: observed_stages.join(" | "),
            lines: recent_lines.join(" | "),
        })
    }
}
