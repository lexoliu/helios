//! A TPM 2.0 for the guest: `swtpm` on the host, and QEMU's TPM front
//! end in the machine, joined by QEMU's `emulator` TPM backend.
//!
//! QEMU drives `swtpm` over the control socket alone: it connects to
//! the socket `--ctrl type=unixio` names and hands the emulator its data
//! channel through `CMD_SET_DATAFD`, so the inspector starts `swtpm`
//! before QEMU and waits for that socket (QEMU `docs/specs/tpm.rst`,
//! "The QEMU TPM emulator device").
//!
//! The TPM's non-volatile state is what a sealed secret survives a
//! reboot in, so where it lives is the session's choice: a directory the
//! session names (`--tpm-state`) outlives every boot that uses it, and
//! without one it lives and goes with the runtime directory.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use super::qemu::QemuOptions;

/// The id QEMU's chardev to the emulator's control socket takes.
const CHARDEV_ID: &str = "chrtpm";
/// The id QEMU's TPM backend takes, which the front end names.
const TPMDEV_ID: &str = "tpm0";
/// The control socket's name in the VM's socket directory; shorter than
/// the monitor's, which bounds every socket name there.
pub(super) const SOCKET_NAME: &str = "swtpm.sock";
/// The emulator's state directory inside the runtime directory, when the
/// session names none.
const RUNTIME_STATE_DIR: &str = "tpm";
const LOG_NAME: &str = "swtpm.log";
const PROGRAM: &str = "swtpm";

/// The host emulator behind the guest's TPM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum VmTpmBackend {
    /// `swtpm` in socket mode.
    Swtpm,
}

/// The TPM front end a machine profile attaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VmTpmProfile {
    /// `tpm-crb`: the Command Response Buffer interface at the PC Client
    /// address, which `q35` describes in its ACPI TPM2 table.
    Crb,
    /// `tpm-tis-device`: the FIFO interface on the `virt` machine's
    /// platform bus, which a `tcg,tpm-tis-mmio` node describes, or with
    /// ACPI the TPM2 table and an `MSFT0101` device.
    TisMmio,
}

impl VmTpmProfile {
    const fn device(self) -> &'static str {
        match self {
            Self::Crb => "tpm-crb",
            Self::TisMmio => "tpm-tis-device",
        }
    }
}

/// Why a session's TPM request cannot be met.
#[derive(Debug, thiserror::Error)]
pub(crate) enum VmTpmError {
    #[error(
        "--tpm is not available on {arch}: its kernel takes no measured-boot device, and its \
         machine profile attaches no TPM front end"
    )]
    Unavailable { arch: &'static str },
    #[error("--tpm-state requires --tpm (or `tpm` in the config file)")]
    StateWithoutTpm,
    #[error(
        "--tpm swtpm needs the `swtpm` TPM emulator on PATH; Homebrew and Debian/Ubuntu both \
         package it as `swtpm`"
    )]
    SwtpmMissing,
}

/// Why the emulator did not come up.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SwtpmError {
    #[error("failed to create the TPM state directory {path}: {source}")]
    CreateStateDir {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to create the swtpm log {path}: {source}")]
    Log {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to start swtpm: {source}")]
    Spawn {
        #[source]
        source: io::Error,
    },
    #[error("failed to poll swtpm: {source}")]
    Poll {
        #[source]
        source: io::Error,
    },
    #[error("swtpm exited before opening its control socket {socket}; {log} says:\n{report}")]
    ExitedBeforeSocket {
        socket: String,
        log: String,
        report: String,
    },
    #[error("timed out waiting for swtpm to open its control socket {socket}")]
    SocketTimedOut { socket: String },
}

/// The guest's TPM, as a session asked for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VmTpm {
    backend: VmTpmBackend,
    device: VmTpmProfile,
    /// The directory the session named for the TPM's state, which the
    /// inspector creates when missing and never removes.
    state_dir: Option<PathBuf>,
}

impl VmTpm {
    /// The TPM the flags and the config file ask for, attached the way
    /// `device` says, or `None` when they ask for none.
    pub(crate) fn resolve(
        backend: Option<VmTpmBackend>,
        state_dir: Option<PathBuf>,
        device: Option<VmTpmProfile>,
        arch: &'static str,
    ) -> Result<Option<Self>, VmTpmError> {
        let Some(backend) = backend else {
            return match state_dir {
                Some(_) => Err(VmTpmError::StateWithoutTpm),
                None => Ok(None),
            };
        };
        let device = device.ok_or(VmTpmError::Unavailable { arch })?;
        Ok(Some(Self {
            backend,
            device,
            state_dir,
        }))
    }

    /// Checks that the emulator can be started, before the kernel build,
    /// so a missing `swtpm` costs seconds rather than a rebuild.
    pub(crate) fn preflight(&self) -> Result<(), VmTpmError> {
        match self.backend {
            VmTpmBackend::Swtpm => {
                super::find_executable_in_path(PROGRAM).ok_or(VmTpmError::SwtpmMissing)?;
                Ok(())
            }
        }
    }

    /// Where the emulator keeps the TPM's state for a VM whose runtime
    /// directory is `runtime_dir`.
    fn state_dir(&self, runtime_dir: &Path) -> PathBuf {
        self.state_dir
            .clone()
            .unwrap_or_else(|| runtime_dir.join(RUNTIME_STATE_DIR))
    }

    /// The emulator's command line: socket mode, a TPM 2.0, the state
    /// directory, the control socket QEMU connects to, and an exit once
    /// QEMU closes it (`swtpm(8)`).
    pub(crate) fn emulator_command(&self, state_dir: &Path, socket: &Path, log: &Path) -> Command {
        let mut tpmstate = QemuOptions::keyed();
        tpmstate.set("dir", state_dir.display());
        let mut ctrl = QemuOptions::keyed();
        ctrl.set("type", "unixio").set("path", socket.display());
        let mut log_option = QemuOptions::keyed();
        log_option.set("file", log.display());
        let mut command = Command::new(PROGRAM);
        command
            .arg("socket")
            .arg("--tpm2")
            .arg("--tpmstate")
            .arg(tpmstate.to_string())
            .arg("--ctrl")
            .arg(ctrl.to_string())
            .arg("--log")
            .arg(log_option.to_string())
            .arg("--terminate");
        command
    }

    /// Adds the backend and the front end to QEMU's command line;
    /// `socket` is the control socket, already in QEMU's option syntax.
    pub(crate) fn apply(&self, qemu: &mut Command, socket: &str) {
        let mut chardev = QemuOptions::new("socket");
        chardev.set("id", CHARDEV_ID).set("path", socket);
        let mut tpmdev = QemuOptions::new("emulator");
        tpmdev.set("id", TPMDEV_ID).set("chardev", CHARDEV_ID);
        let mut device = QemuOptions::new(self.device.device());
        device.set("tpmdev", TPMDEV_ID);
        qemu.arg("-chardev").arg(chardev.to_string());
        qemu.arg("-tpmdev").arg(tpmdev.to_string());
        qemu.arg("-device").arg(device.to_string());
    }

    /// Starts the emulator and waits for its control socket.
    pub(crate) fn start(&self, runtime_dir: &Path, socket: &Path) -> Result<Swtpm, SwtpmError> {
        let state_dir = self.state_dir(runtime_dir);
        fs::create_dir_all(&state_dir).map_err(|source| SwtpmError::CreateStateDir {
            path: state_dir.display().to_string(),
            source,
        })?;
        let log = runtime_dir.join(LOG_NAME);
        // What swtpm says before its own log is open goes to the same
        // file, the way QEMU's stderr joins its log.
        let stderr = fs::File::create(&log).map_err(|source| SwtpmError::Log {
            path: log.display().to_string(),
            source,
        })?;
        let mut command = self.emulator_command(&state_dir, socket, &log);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));
        super::keep_off_the_terminal(&mut command);
        let mut child = command
            .spawn()
            .map_err(|source| SwtpmError::Spawn { source })?;
        match super::poll_for_socket(socket, &mut child) {
            Ok(super::SocketWait::Ready) => Ok(Swtpm {
                child,
                socket: socket.to_path_buf(),
            }),
            Ok(super::SocketWait::Exited) => Err(SwtpmError::ExitedBeforeSocket {
                socket: socket.display().to_string(),
                log: log.display().to_string(),
                report: fs::read_to_string(&log).unwrap_or_default(),
            }),
            Ok(super::SocketWait::TimedOut) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(SwtpmError::SocketTimedOut {
                    socket: socket.display().to_string(),
                })
            }
            Err(source) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(SwtpmError::Poll { source })
            }
        }
    }
}

/// The emulator serving one VM's TPM, for as long as the VM runs.
pub(crate) struct Swtpm {
    child: Child,
    socket: PathBuf,
}

impl Swtpm {
    /// The control socket QEMU's `emulator` backend connects to.
    pub(crate) fn socket(&self) -> &Path {
        &self.socket
    }

    /// Ends the emulator. `--terminate` already ends it once QEMU closes
    /// the control channel; this covers a QEMU that never connected, and
    /// a spawn that failed after the emulator started.
    pub(crate) fn shutdown(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl Drop for Swtpm {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn no_backend_and_no_state_is_no_tpm() {
        assert_eq!(
            VmTpm::resolve(None, None, Some(VmTpmProfile::Crb), "x86-64").unwrap(),
            None
        );
    }

    #[test]
    fn a_state_directory_without_a_backend_is_refused() {
        let error = VmTpm::resolve(
            None,
            Some(PathBuf::from("/var/lib/helios-tpm")),
            Some(VmTpmProfile::Crb),
            "x86-64",
        )
        .unwrap_err();
        assert!(matches!(error, VmTpmError::StateWithoutTpm), "{error}");
    }

    #[test]
    fn a_profile_without_a_front_end_refuses_the_tpm() {
        let error = VmTpm::resolve(Some(VmTpmBackend::Swtpm), None, None, "riscv64").unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("--tpm is not available on riscv64"),
            "{error}"
        );
    }

    #[test]
    fn the_state_lives_in_the_runtime_directory_unless_named() {
        let runtime = Path::new("/run/helios-vm");
        let transient =
            VmTpm::resolve(Some(VmTpmBackend::Swtpm), None, Some(VmTpmProfile::Crb), "")
                .unwrap()
                .unwrap();
        assert_eq!(transient.state_dir(runtime), runtime.join("tpm"));
        let retained = VmTpm::resolve(
            Some(VmTpmBackend::Swtpm),
            Some(PathBuf::from("/var/lib/helios-tpm")),
            Some(VmTpmProfile::Crb),
            "",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            retained.state_dir(runtime),
            PathBuf::from("/var/lib/helios-tpm")
        );
    }

    #[test]
    fn swtpm_runs_in_socket_mode_on_the_control_socket_qemu_connects_to() {
        let tpm = VmTpm::resolve(Some(VmTpmBackend::Swtpm), None, Some(VmTpmProfile::Crb), "")
            .unwrap()
            .unwrap();
        let command = tpm.emulator_command(
            Path::new("/run/helios-vm/tpm"),
            Path::new("/tmp/helios-x/swtpm.sock"),
            Path::new("/run/helios-vm/swtpm.log"),
        );
        assert_eq!(command.get_program(), "swtpm");
        assert_eq!(
            arguments(&command),
            [
                "socket",
                "--tpm2",
                "--tpmstate",
                "dir=/run/helios-vm/tpm",
                "--ctrl",
                "type=unixio,path=/tmp/helios-x/swtpm.sock",
                "--log",
                "file=/run/helios-vm/swtpm.log",
                "--terminate",
            ]
        );
    }

    #[test]
    fn each_profile_attaches_its_own_front_end_to_the_emulator_backend() {
        for (profile, device) in [
            (VmTpmProfile::Crb, "tpm-crb,tpmdev=tpm0"),
            (VmTpmProfile::TisMmio, "tpm-tis-device,tpmdev=tpm0"),
        ] {
            let tpm = VmTpm::resolve(Some(VmTpmBackend::Swtpm), None, Some(profile), "")
                .unwrap()
                .unwrap();
            let mut qemu = Command::new("qemu");
            tpm.apply(&mut qemu, "/tmp/helios-x/swtpm.sock");
            assert_eq!(
                arguments(&qemu),
                [
                    "-chardev",
                    "socket,id=chrtpm,path=/tmp/helios-x/swtpm.sock",
                    "-tpmdev",
                    "emulator,id=tpm0,chardev=chrtpm",
                    "-device",
                    device,
                ]
            );
        }
    }
}
