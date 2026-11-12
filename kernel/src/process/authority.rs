extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use arrayvec::ArrayVec;
use bitflags::bitflags;
use thiserror::Error;

use crate::device::{DeviceName, MAX_GRANTS};
use crate::{ComponentFsPathError, resolve_absolute_path};

bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct DirectoryAuthorityRights: u16 {
        const READ = 1 << 0;
        const WRITE = 1 << 1;
        const MUTATE_DIRECTORY = 1 << 2;
        const EXECUTE = 1 << 3;
    }
}

bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct NetworkAuthorityRights: u16 {
        const TCP = 1 << 0;
        const UDP = 1 << 1;
        const DNS = 1 << 2;
        const PRIVILEGED_BIND = 1 << 3;
        const MULTICAST = 1 << 4;
        const ADMIN = 1 << 5;
    }
}

bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct ClockAuthorityRights: u8 {
        const SET_WALL_CLOCK = 1 << 0;
    }
}

bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct TerminalAuthorityRights: u8 {
        const INPUT = 1 << 0;
        const OUTPUT = 1 << 1;
        const CONTROL = 1 << 2;
    }
}

bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct ProcessAuthorityRights: u8 {
        const SPAWN = 1 << 0;
        const EXEC = 1 << 1;
        const FORK = 1 << 2;
        const JOIN = 1 << 3;
        const SIGNAL = 1 << 4;
    }
}

bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct LinkAuthorityRights: u8 {
        const SOURCE = 1 << 0;
        const TARGET_DIRECTORY = 1 << 1;
        const SYMLINK_CREATE = 1 << 2;
        const SYMLINK_READ = 1 << 3;
    }
}

bitflags! {
    /// Rights over the kernel's mount table.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct MountAuthorityRights: u8 {
        /// Attach a filesystem at a directory the holder may change.
        const MOUNT = 1 << 0;
        /// Detach a filesystem from a directory the holder may change.
        const UNMOUNT = 1 << 1;
    }
}

bitflags! {
    /// The machine-wide device classes an instance may claim.
    ///
    /// Each class is one resource with one owner role, so authority names
    /// the class. A raw device is named individually instead, in
    /// [`ProcessAuthority::granted_devices`].
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct DeviceClassRights: u8 {
        const DISPLAY = 1 << 0;
        const INPUT = 1 << 1;
        const GPU = 1 << 2;
        const AUDIO = 1 << 3;
    }
}

bitflags! {
    /// What an instance may do to the machine's diagnostics, beyond
    /// reading its own.
    ///
    /// `OBSERVE_MACHINE` covers the tracing and profiling controls and
    /// reading any instance's records. Every observer in the tree uses
    /// both, so one right covers them.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct DiagnosticAuthorityRights: u8 {
        const OBSERVE_MACHINE = 1 << 0;
    }
}

bitflags! {
    /// What an instance may do to the machine-wide settings. Reading a
    /// setting needs no right.
    ///
    /// A class of its own beside the diagnostics rights: observing the
    /// machine and configuring it are separate powers, and an observer
    /// has no reason to hold the second.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct SettingsAuthorityRights: u8 {
        const WRITE = 1 << 0;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryPreopen {
    source_path: String,
    guest_name: String,
    rights: DirectoryAuthorityRights,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryCap {
    preopen: DirectoryPreopen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetworkCap {
    rights: NetworkAuthorityRights,
}

macro_rules! typed_network_cap {
    ($name:ident, $rights:expr) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct $name(NetworkCap);

        impl $name {
            pub const fn required_rights() -> NetworkAuthorityRights {
                $rights
            }

            pub const fn network_cap(self) -> NetworkCap {
                self.0
            }
        }
    };
}

typed_network_cap!(TcpCap, NetworkAuthorityRights::TCP);
typed_network_cap!(UdpCap, NetworkAuthorityRights::UDP);
typed_network_cap!(DnsCap, NetworkAuthorityRights::DNS);
typed_network_cap!(PrivilegedBindCap, NetworkAuthorityRights::PRIVILEGED_BIND);
typed_network_cap!(MulticastCap, NetworkAuthorityRights::MULTICAST);
typed_network_cap!(NetworkAdminCap, NetworkAuthorityRights::ADMIN);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetWallClockCap(ClockAuthorityRights);

macro_rules! typed_device_class_cap {
    ($name:ident, $rights:expr) => {
        /// Proof, checked on the owning task, that the instance holds
        /// this device class.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct $name(DeviceClassRights);

        impl $name {
            pub const fn required_rights() -> DeviceClassRights {
                $rights
            }

            pub const fn rights(self) -> DeviceClassRights {
                self.0
            }
        }
    };
}

typed_device_class_cap!(DisplayClaimCap, DeviceClassRights::DISPLAY);
typed_device_class_cap!(InputClaimCap, DeviceClassRights::INPUT);
typed_device_class_cap!(GpuOpenCap, DeviceClassRights::GPU);
typed_device_class_cap!(AudioClaimCap, DeviceClassRights::AUDIO);

/// Proof that the instance was granted one named device. The name is
/// the proof: a registry claim looks the device up by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceGrantClaimCap(DeviceName);

impl DeviceGrantClaimCap {
    pub const fn name(&self) -> &DeviceName {
        &self.0
    }
}

/// Proof, checked on the owning task, that the instance may control the
/// machine's tracing and profiling and read every instance's records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObserveMachineCap(DiagnosticAuthorityRights);

impl ObserveMachineCap {
    pub const fn required_rights() -> DiagnosticAuthorityRights {
        DiagnosticAuthorityRights::OBSERVE_MACHINE
    }

    pub const fn rights(self) -> DiagnosticAuthorityRights {
        self.0
    }
}

/// Proof, checked on the owning task, that the instance may change the
/// machine-wide settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteSettingsCap(SettingsAuthorityRights);

impl WriteSettingsCap {
    pub const fn required_rights() -> SettingsAuthorityRights {
        SettingsAuthorityRights::WRITE
    }

    pub const fn rights(self) -> SettingsAuthorityRights {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalInputCap(TerminalAuthorityRights);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalOutputCap(TerminalAuthorityRights);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TtyControlCap(TerminalAuthorityRights);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpawnAuthority(ProcessAuthorityRights);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecAuthority(ProcessAuthorityRights);

/// Proof that the holder derived a process right that may start a
/// compile: SPAWN or EXEC. It is built only from one of those two
/// tokens, so every route into the in-kernel compiler has to have
/// derived a process right first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompileAuthority(ProcessAuthorityRights);

impl From<&SpawnAuthority> for CompileAuthority {
    fn from(authority: &SpawnAuthority) -> Self {
        Self(authority.rights())
    }
}

impl From<&ExecAuthority> for CompileAuthority {
    fn from(authority: &ExecAuthority) -> Self {
        Self(authority.rights())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ForkAuthority(ProcessAuthorityRights);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JoinAuthority(ProcessAuthorityRights);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignalAuthority(ProcessAuthorityRights);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkSourceCap(LinkAuthorityRights);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkTargetDirectoryCap(LinkAuthorityRights);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SymlinkCreateCap(LinkAuthorityRights);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SymlinkReadCap(LinkAuthorityRights);

/// Authority to attach a filesystem at one mount point: the holder has the
/// mount right and may change the directory at `point`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountCap {
    point: String,
}

/// Authority to detach the filesystem at one mount point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnmountCap {
    point: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessAuthority {
    directory_preopens: Vec<DirectoryPreopen>,
    cwd: Option<DirectoryPreopen>,
    network: NetworkAuthorityRights,
    clock: ClockAuthorityRights,
    terminal: TerminalAuthorityRights,
    process: ProcessAuthorityRights,
    link: LinkAuthorityRights,
    mount: MountAuthorityRights,
    device_classes: DeviceClassRights,
    /// Raw devices this instance may claim by name. Bounded by the
    /// registry's own capacity, because the registry cannot publish more.
    devices: ArrayVec<DeviceName, MAX_GRANTS>,
    diagnostics: DiagnosticAuthorityRights,
    settings: SettingsAuthorityRights,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ProcessAuthorityError {
    #[error("directory grant rights must not be empty")]
    EmptyRights,
    #[error("network grant rights must not be empty")]
    EmptyNetworkRights,
    #[error("clock grant rights must not be empty")]
    EmptyClockRights,
    #[error("terminal grant rights must not be empty")]
    EmptyTerminalRights,
    #[error("process grant rights must not be empty")]
    EmptyProcessRights,
    #[error("link grant rights must not be empty")]
    EmptyLinkRights,
    #[error("directory grant path is invalid: {0}")]
    InvalidPath(ComponentFsPathError),
    #[error("directory grant exceeds caller authority: rights={0:?}")]
    DirectoryGrantExceedsAuthority(DirectoryAuthorityRights),
    #[error("network grant exceeds caller authority: {0:?}")]
    NetworkGrantExceedsAuthority(NetworkAuthorityRights),
    #[error("clock grant exceeds caller authority: {0:?}")]
    ClockGrantExceedsAuthority(ClockAuthorityRights),
    #[error("terminal grant exceeds caller authority: {0:?}")]
    TerminalGrantExceedsAuthority(TerminalAuthorityRights),
    #[error("process grant exceeds caller authority: {0:?}")]
    ProcessGrantExceedsAuthority(ProcessAuthorityRights),
    #[error("link grant exceeds caller authority: {0:?}")]
    LinkGrantExceedsAuthority(LinkAuthorityRights),
    #[error("mount grant rights must not be empty")]
    EmptyMountRights,
    #[error("mount grant exceeds caller authority: {0:?}")]
    MountGrantExceedsAuthority(MountAuthorityRights),
    #[error("mount point {0} is outside the caller's mutate-directory authority")]
    MountPointOutsideAuthority(String),
    #[error("device class grant rights must not be empty")]
    EmptyDeviceClassRights,
    #[error("device class grant exceeds caller authority: {0:?}")]
    DeviceClassGrantExceedsAuthority(DeviceClassRights),
    #[error("device grant exceeds caller authority: {0}")]
    DeviceGrantExceedsAuthority(DeviceName),
    #[error("device name is longer than the registry stores")]
    DeviceNameTooLong,
    #[error("an instance holds at most {} named devices", MAX_GRANTS)]
    TooManyDeviceGrants,
    #[error("diagnostic grant rights must not be empty")]
    EmptyDiagnosticRights,
    #[error("diagnostic grant exceeds caller authority: {0:?}")]
    DiagnosticGrantExceedsAuthority(DiagnosticAuthorityRights),
    #[error("settings grant rights must not be empty")]
    EmptySettingsRights,
    #[error("settings grant exceeds caller authority: {0:?}")]
    SettingsGrantExceedsAuthority(SettingsAuthorityRights),
}

impl DirectoryPreopen {
    pub fn new(
        source_path: impl AsRef<str>,
        guest_name: impl AsRef<str>,
        rights: DirectoryAuthorityRights,
    ) -> Result<Self, ProcessAuthorityError> {
        if rights.is_empty() {
            return Err(ProcessAuthorityError::EmptyRights);
        }
        Ok(Self {
            source_path: normalize_authority_path(source_path.as_ref())?,
            guest_name: normalize_authority_path(guest_name.as_ref())?,
            rights,
        })
    }

    pub fn source_path(&self) -> &str {
        &self.source_path
    }

    pub fn guest_name(&self) -> &str {
        &self.guest_name
    }

    pub fn rights(&self) -> DirectoryAuthorityRights {
        self.rights
    }
}

impl DirectoryCap {
    pub fn source_path(&self) -> &str {
        self.preopen.source_path()
    }

    pub fn guest_name(&self) -> &str {
        self.preopen.guest_name()
    }

    pub fn rights(&self) -> DirectoryAuthorityRights {
        self.preopen.rights()
    }

    pub fn into_preopen(self) -> DirectoryPreopen {
        self.preopen
    }
}

impl NetworkCap {
    pub const fn new(rights: NetworkAuthorityRights) -> Self {
        Self { rights }
    }

    pub const fn rights(self) -> NetworkAuthorityRights {
        self.rights
    }
}

impl SetWallClockCap {
    pub const fn rights(self) -> ClockAuthorityRights {
        self.0
    }
}

impl TerminalInputCap {
    pub const fn rights(self) -> TerminalAuthorityRights {
        self.0
    }
}

impl TerminalOutputCap {
    pub const fn rights(self) -> TerminalAuthorityRights {
        self.0
    }
}

impl TtyControlCap {
    pub const fn rights(self) -> TerminalAuthorityRights {
        self.0
    }
}

macro_rules! impl_process_cap {
    ($name:ident) => {
        impl $name {
            pub const fn rights(self) -> ProcessAuthorityRights {
                self.0
            }
        }
    };
}

impl_process_cap!(SpawnAuthority);
impl_process_cap!(ExecAuthority);
impl_process_cap!(CompileAuthority);
impl_process_cap!(ForkAuthority);
impl_process_cap!(JoinAuthority);
impl_process_cap!(SignalAuthority);

macro_rules! impl_link_cap {
    ($name:ident) => {
        impl $name {
            pub const fn rights(self) -> LinkAuthorityRights {
                self.0
            }
        }
    };
}

impl_link_cap!(LinkSourceCap);
impl_link_cap!(LinkTargetDirectoryCap);
impl_link_cap!(SymlinkCreateCap);
impl_link_cap!(SymlinkReadCap);

impl MountCap {
    /// The normalized guest path the filesystem is attached at.
    pub fn point(&self) -> &str {
        &self.point
    }
}

impl UnmountCap {
    /// The normalized guest path the filesystem is detached from.
    pub fn point(&self) -> &str {
        &self.point
    }
}

impl ProcessAuthority {
    pub const fn empty() -> Self {
        Self {
            directory_preopens: Vec::new(),
            cwd: None,
            network: NetworkAuthorityRights::empty(),
            clock: ClockAuthorityRights::empty(),
            terminal: TerminalAuthorityRights::empty(),
            process: ProcessAuthorityRights::empty(),
            link: LinkAuthorityRights::empty(),
            mount: MountAuthorityRights::empty(),
            device_classes: DeviceClassRights::empty(),
            devices: ArrayVec::new_const(),
            diagnostics: DiagnosticAuthorityRights::empty(),
            settings: SettingsAuthorityRights::empty(),
        }
    }

    /// Every right except the device, diagnostic and settings ones. The
    /// kernel adds the device classes, named devices, diagnostic and
    /// settings rights a component needs where it starts that component,
    /// so no program inherits a claim, the machine's diagnostics or its
    /// settings through this value.
    pub fn root() -> Self {
        let mut authority = Self::empty();
        authority.insert_directory_preopen(
            DirectoryPreopen::new(
                "/",
                "/",
                DirectoryAuthorityRights::READ
                    | DirectoryAuthorityRights::WRITE
                    | DirectoryAuthorityRights::MUTATE_DIRECTORY
                    | DirectoryAuthorityRights::EXECUTE,
            )
            .expect("root process authority must be valid"),
        );
        authority.chdir(
            authority
                .derive_directory_cap("/", "/", DirectoryAuthorityRights::READ)
                .expect("root cwd cap must be valid"),
        );
        authority.grant_network_rights(
            NetworkAuthorityRights::TCP
                | NetworkAuthorityRights::UDP
                | NetworkAuthorityRights::DNS
                | NetworkAuthorityRights::PRIVILEGED_BIND
                | NetworkAuthorityRights::MULTICAST
                | NetworkAuthorityRights::ADMIN,
        );
        authority.grant_clock_rights(ClockAuthorityRights::SET_WALL_CLOCK);
        authority.grant_terminal_rights(
            TerminalAuthorityRights::INPUT
                | TerminalAuthorityRights::OUTPUT
                | TerminalAuthorityRights::CONTROL,
        );
        authority.grant_process_rights(
            ProcessAuthorityRights::SPAWN
                | ProcessAuthorityRights::EXEC
                | ProcessAuthorityRights::FORK
                | ProcessAuthorityRights::JOIN
                | ProcessAuthorityRights::SIGNAL,
        );
        authority.grant_link_rights(
            LinkAuthorityRights::SOURCE
                | LinkAuthorityRights::TARGET_DIRECTORY
                | LinkAuthorityRights::SYMLINK_CREATE
                | LinkAuthorityRights::SYMLINK_READ,
        );
        authority.grant_mount_rights(MountAuthorityRights::MOUNT | MountAuthorityRights::UNMOUNT);
        authority
    }

    pub fn fork_inherited(&self) -> Self {
        self.clone()
    }

    pub fn exec_preserved(&self) -> Self {
        self.clone()
    }

    pub fn directory_preopens(&self) -> &[DirectoryPreopen] {
        &self.directory_preopens
    }

    pub fn cwd(&self) -> Option<&DirectoryPreopen> {
        self.cwd.as_ref()
    }

    pub fn chdir(&mut self, directory: DirectoryCap) {
        self.cwd = Some(directory.into_preopen());
    }

    pub fn insert_directory_preopen(&mut self, preopen: DirectoryPreopen) {
        self.directory_preopens.push(preopen);
    }

    pub fn network_rights(&self) -> NetworkAuthorityRights {
        self.network
    }

    pub fn clock_rights(&self) -> ClockAuthorityRights {
        self.clock
    }

    pub fn terminal_rights(&self) -> TerminalAuthorityRights {
        self.terminal
    }

    pub fn process_rights(&self) -> ProcessAuthorityRights {
        self.process
    }

    pub fn link_rights(&self) -> LinkAuthorityRights {
        self.link
    }

    pub fn mount_rights(&self) -> MountAuthorityRights {
        self.mount
    }

    pub fn device_class_rights(&self) -> DeviceClassRights {
        self.device_classes
    }

    pub fn granted_devices(&self) -> &[DeviceName] {
        &self.devices
    }

    pub fn diagnostic_rights(&self) -> DiagnosticAuthorityRights {
        self.diagnostics
    }

    pub fn grant_diagnostic_rights(&mut self, rights: DiagnosticAuthorityRights) {
        self.diagnostics |= rights;
    }

    pub fn settings_rights(&self) -> SettingsAuthorityRights {
        self.settings
    }

    pub fn grant_settings_rights(&mut self, rights: SettingsAuthorityRights) {
        self.settings |= rights;
    }

    pub fn grant_device_classes(&mut self, rights: DeviceClassRights) {
        self.device_classes |= rights;
    }

    /// Name `name` as a device this instance may claim. Granting a name
    /// twice is the same grant.
    pub fn grant_device(&mut self, name: DeviceName) -> Result<(), ProcessAuthorityError> {
        if self.devices.contains(&name) {
            return Ok(());
        }
        self.devices
            .try_push(name)
            .map_err(|_| ProcessAuthorityError::TooManyDeviceGrants)
    }

    pub fn grant_network_rights(&mut self, rights: NetworkAuthorityRights) {
        self.network |= rights;
    }

    pub fn grant_clock_rights(&mut self, rights: ClockAuthorityRights) {
        self.clock |= rights;
    }

    pub fn grant_terminal_rights(&mut self, rights: TerminalAuthorityRights) {
        self.terminal |= rights;
    }

    pub fn grant_process_rights(&mut self, rights: ProcessAuthorityRights) {
        self.process |= rights;
    }

    pub fn grant_link_rights(&mut self, rights: LinkAuthorityRights) {
        self.link |= rights;
    }

    pub fn grant_mount_rights(&mut self, rights: MountAuthorityRights) {
        self.mount |= rights;
    }

    pub fn derive_directory_preopen(
        &self,
        source_path: impl AsRef<str>,
        guest_name: impl AsRef<str>,
        rights: DirectoryAuthorityRights,
    ) -> Result<DirectoryPreopen, ProcessAuthorityError> {
        let source_path = normalize_authority_path(source_path.as_ref())?;
        if rights.is_empty() {
            return Err(ProcessAuthorityError::EmptyRights);
        }
        if !self.has_directory_authority(&source_path, rights) {
            return Err(ProcessAuthorityError::DirectoryGrantExceedsAuthority(
                rights,
            ));
        }
        DirectoryPreopen::new(source_path, guest_name, rights)
    }

    pub fn derive_directory_cap(
        &self,
        source_path: impl AsRef<str>,
        guest_name: impl AsRef<str>,
        rights: DirectoryAuthorityRights,
    ) -> Result<DirectoryCap, ProcessAuthorityError> {
        Ok(DirectoryCap {
            preopen: self.derive_directory_preopen(source_path, guest_name, rights)?,
        })
    }

    pub fn derive_network_rights(
        &self,
        rights: NetworkAuthorityRights,
    ) -> Result<NetworkAuthorityRights, ProcessAuthorityError> {
        if rights.is_empty() {
            return Err(ProcessAuthorityError::EmptyNetworkRights);
        }
        if !self.network.contains(rights) {
            return Err(ProcessAuthorityError::NetworkGrantExceedsAuthority(rights));
        }
        Ok(rights)
    }

    pub fn derive_network_cap(
        &self,
        rights: NetworkAuthorityRights,
    ) -> Result<NetworkCap, ProcessAuthorityError> {
        self.derive_network_rights(rights).map(NetworkCap::new)
    }

    pub fn derive_tcp_cap(&self) -> Result<TcpCap, ProcessAuthorityError> {
        self.derive_network_cap(TcpCap::required_rights())
            .map(TcpCap)
    }

    pub fn derive_udp_cap(&self) -> Result<UdpCap, ProcessAuthorityError> {
        self.derive_network_cap(UdpCap::required_rights())
            .map(UdpCap)
    }

    pub fn derive_dns_cap(&self) -> Result<DnsCap, ProcessAuthorityError> {
        self.derive_network_cap(DnsCap::required_rights())
            .map(DnsCap)
    }

    pub fn derive_privileged_bind_cap(&self) -> Result<PrivilegedBindCap, ProcessAuthorityError> {
        self.derive_network_cap(PrivilegedBindCap::required_rights())
            .map(PrivilegedBindCap)
    }

    pub fn derive_multicast_cap(&self) -> Result<MulticastCap, ProcessAuthorityError> {
        self.derive_network_cap(MulticastCap::required_rights())
            .map(MulticastCap)
    }

    pub fn derive_network_admin_cap(&self) -> Result<NetworkAdminCap, ProcessAuthorityError> {
        self.derive_network_cap(NetworkAdminCap::required_rights())
            .map(NetworkAdminCap)
    }

    pub fn derive_clock_rights(
        &self,
        rights: ClockAuthorityRights,
    ) -> Result<ClockAuthorityRights, ProcessAuthorityError> {
        if rights.is_empty() {
            return Err(ProcessAuthorityError::EmptyClockRights);
        }
        if !self.clock.contains(rights) {
            return Err(ProcessAuthorityError::ClockGrantExceedsAuthority(rights));
        }
        Ok(rights)
    }

    pub fn derive_set_wall_clock_cap(&self) -> Result<SetWallClockCap, ProcessAuthorityError> {
        self.derive_clock_rights(ClockAuthorityRights::SET_WALL_CLOCK)
            .map(SetWallClockCap)
    }

    pub fn derive_terminal_rights(
        &self,
        rights: TerminalAuthorityRights,
    ) -> Result<TerminalAuthorityRights, ProcessAuthorityError> {
        if rights.is_empty() {
            return Err(ProcessAuthorityError::EmptyTerminalRights);
        }
        if !self.terminal.contains(rights) {
            return Err(ProcessAuthorityError::TerminalGrantExceedsAuthority(rights));
        }
        Ok(rights)
    }

    pub fn derive_terminal_input_cap(&self) -> Result<TerminalInputCap, ProcessAuthorityError> {
        self.derive_terminal_rights(TerminalAuthorityRights::INPUT)
            .map(TerminalInputCap)
    }

    pub fn derive_terminal_output_cap(&self) -> Result<TerminalOutputCap, ProcessAuthorityError> {
        self.derive_terminal_rights(TerminalAuthorityRights::OUTPUT)
            .map(TerminalOutputCap)
    }

    pub fn derive_tty_control_cap(&self) -> Result<TtyControlCap, ProcessAuthorityError> {
        self.derive_terminal_rights(TerminalAuthorityRights::CONTROL)
            .map(TtyControlCap)
    }

    pub fn derive_process_rights(
        &self,
        rights: ProcessAuthorityRights,
    ) -> Result<ProcessAuthorityRights, ProcessAuthorityError> {
        if rights.is_empty() {
            return Err(ProcessAuthorityError::EmptyProcessRights);
        }
        if !self.process.contains(rights) {
            return Err(ProcessAuthorityError::ProcessGrantExceedsAuthority(rights));
        }
        Ok(rights)
    }

    pub fn derive_spawn_authority(&self) -> Result<SpawnAuthority, ProcessAuthorityError> {
        self.derive_process_rights(ProcessAuthorityRights::SPAWN)
            .map(SpawnAuthority)
    }

    pub fn derive_exec_authority(&self) -> Result<ExecAuthority, ProcessAuthorityError> {
        self.derive_process_rights(ProcessAuthorityRights::EXEC)
            .map(ExecAuthority)
    }

    pub fn derive_fork_authority(&self) -> Result<ForkAuthority, ProcessAuthorityError> {
        self.derive_process_rights(ProcessAuthorityRights::FORK)
            .map(ForkAuthority)
    }

    pub fn derive_join_authority(&self) -> Result<JoinAuthority, ProcessAuthorityError> {
        self.derive_process_rights(ProcessAuthorityRights::JOIN)
            .map(JoinAuthority)
    }

    pub fn derive_signal_authority(&self) -> Result<SignalAuthority, ProcessAuthorityError> {
        self.derive_process_rights(ProcessAuthorityRights::SIGNAL)
            .map(SignalAuthority)
    }

    pub fn derive_link_rights(
        &self,
        rights: LinkAuthorityRights,
    ) -> Result<LinkAuthorityRights, ProcessAuthorityError> {
        if rights.is_empty() {
            return Err(ProcessAuthorityError::EmptyLinkRights);
        }
        if !self.link.contains(rights) {
            return Err(ProcessAuthorityError::LinkGrantExceedsAuthority(rights));
        }
        Ok(rights)
    }

    pub fn derive_link_source_cap(&self) -> Result<LinkSourceCap, ProcessAuthorityError> {
        self.derive_link_rights(LinkAuthorityRights::SOURCE)
            .map(LinkSourceCap)
    }

    pub fn derive_link_target_directory_cap(
        &self,
    ) -> Result<LinkTargetDirectoryCap, ProcessAuthorityError> {
        self.derive_link_rights(LinkAuthorityRights::TARGET_DIRECTORY)
            .map(LinkTargetDirectoryCap)
    }

    pub fn derive_symlink_create_cap(&self) -> Result<SymlinkCreateCap, ProcessAuthorityError> {
        self.derive_link_rights(LinkAuthorityRights::SYMLINK_CREATE)
            .map(SymlinkCreateCap)
    }

    pub fn derive_symlink_read_cap(&self) -> Result<SymlinkReadCap, ProcessAuthorityError> {
        self.derive_link_rights(LinkAuthorityRights::SYMLINK_READ)
            .map(SymlinkReadCap)
    }

    pub fn derive_mount_rights(
        &self,
        rights: MountAuthorityRights,
    ) -> Result<MountAuthorityRights, ProcessAuthorityError> {
        if rights.is_empty() {
            return Err(ProcessAuthorityError::EmptyMountRights);
        }
        if !self.mount.contains(rights) {
            return Err(ProcessAuthorityError::MountGrantExceedsAuthority(rights));
        }
        Ok(rights)
    }

    pub fn derive_diagnostic_rights(
        &self,
        rights: DiagnosticAuthorityRights,
    ) -> Result<DiagnosticAuthorityRights, ProcessAuthorityError> {
        if rights.is_empty() {
            return Err(ProcessAuthorityError::EmptyDiagnosticRights);
        }
        if !self.diagnostics.contains(rights) {
            return Err(ProcessAuthorityError::DiagnosticGrantExceedsAuthority(
                rights,
            ));
        }
        Ok(rights)
    }

    pub fn derive_observe_machine_cap(&self) -> Result<ObserveMachineCap, ProcessAuthorityError> {
        self.derive_diagnostic_rights(ObserveMachineCap::required_rights())
            .map(ObserveMachineCap)
    }

    pub fn derive_settings_rights(
        &self,
        rights: SettingsAuthorityRights,
    ) -> Result<SettingsAuthorityRights, ProcessAuthorityError> {
        if rights.is_empty() {
            return Err(ProcessAuthorityError::EmptySettingsRights);
        }
        if !self.settings.contains(rights) {
            return Err(ProcessAuthorityError::SettingsGrantExceedsAuthority(rights));
        }
        Ok(rights)
    }

    pub fn derive_write_settings_cap(&self) -> Result<WriteSettingsCap, ProcessAuthorityError> {
        self.derive_settings_rights(WriteSettingsCap::required_rights())
            .map(WriteSettingsCap)
    }

    pub fn derive_device_class_rights(
        &self,
        rights: DeviceClassRights,
    ) -> Result<DeviceClassRights, ProcessAuthorityError> {
        if rights.is_empty() {
            return Err(ProcessAuthorityError::EmptyDeviceClassRights);
        }
        if !self.device_classes.contains(rights) {
            return Err(ProcessAuthorityError::DeviceClassGrantExceedsAuthority(
                rights,
            ));
        }
        Ok(rights)
    }

    /// Authority to attach a filesystem at `point`: the mount right, and
    /// mutate-directory authority over the point, since a mount replaces
    /// what the directory shows.
    pub fn derive_mount_cap(&self, point: &str) -> Result<MountCap, ProcessAuthorityError> {
        self.derive_mount_rights(MountAuthorityRights::MOUNT)?;
        Ok(MountCap {
            point: self.mutable_mount_point(point)?,
        })
    }

    /// Authority to detach the filesystem at `point`, under the same
    /// directory authority attaching it needed.
    pub fn derive_unmount_cap(&self, point: &str) -> Result<UnmountCap, ProcessAuthorityError> {
        self.derive_mount_rights(MountAuthorityRights::UNMOUNT)?;
        Ok(UnmountCap {
            point: self.mutable_mount_point(point)?,
        })
    }

    fn mutable_mount_point(&self, point: &str) -> Result<String, ProcessAuthorityError> {
        let point = normalize_authority_path(point)?;
        if !self.has_directory_authority(&point, DirectoryAuthorityRights::MUTATE_DIRECTORY) {
            return Err(ProcessAuthorityError::MountPointOutsideAuthority(point));
        }
        Ok(point)
    }

    pub fn derive_display_claim_cap(&self) -> Result<DisplayClaimCap, ProcessAuthorityError> {
        self.derive_device_class_rights(DisplayClaimCap::required_rights())
            .map(DisplayClaimCap)
    }

    pub fn derive_input_claim_cap(&self) -> Result<InputClaimCap, ProcessAuthorityError> {
        self.derive_device_class_rights(InputClaimCap::required_rights())
            .map(InputClaimCap)
    }

    pub fn derive_gpu_open_cap(&self) -> Result<GpuOpenCap, ProcessAuthorityError> {
        self.derive_device_class_rights(GpuOpenCap::required_rights())
            .map(GpuOpenCap)
    }

    pub fn derive_audio_claim_cap(&self) -> Result<AudioClaimCap, ProcessAuthorityError> {
        self.derive_device_class_rights(AudioClaimCap::required_rights())
            .map(AudioClaimCap)
    }

    /// The device `name` is granted to this instance. A name the registry
    /// could not store is as ungranted as one nobody published.
    pub fn derive_device_grant_cap(
        &self,
        name: &str,
    ) -> Result<DeviceGrantClaimCap, ProcessAuthorityError> {
        let name = DeviceName::new(name).map_err(|_| ProcessAuthorityError::DeviceNameTooLong)?;
        if !self.devices.contains(&name) {
            return Err(ProcessAuthorityError::DeviceGrantExceedsAuthority(name));
        }
        Ok(DeviceGrantClaimCap(name))
    }

    pub fn can_load_program(&self, path: &str) -> bool {
        self.program_load_floor(path).is_some()
    }

    /// The directory whose grant lets `path` be loaded as a program: the
    /// widest such grant, which is the floor a symbolic link the load
    /// follows may not leave.
    pub fn program_load_floor(&self, path: &str) -> Option<&str> {
        let loads = |rights: DirectoryAuthorityRights| {
            rights.intersects(DirectoryAuthorityRights::READ | DirectoryAuthorityRights::EXECUTE)
        };
        self.directory_preopens
            .iter()
            .map(|preopen| (preopen.source_path(), preopen.rights()))
            .chain(self.cwd.iter().map(|cwd| (cwd.source_path(), cwd.rights())))
            .filter(|(directory, rights)| directory_contains(directory, path) && loads(*rights))
            .map(|(directory, _)| directory)
            .min_by_key(|directory| directory.len())
    }

    pub fn can_write_path(&self, path: &str) -> bool {
        self.has_directory_authority(path, DirectoryAuthorityRights::WRITE)
    }

    pub fn can_create_or_replace_path(&self, path: &str) -> bool {
        self.has_directory_authority(
            path,
            DirectoryAuthorityRights::WRITE | DirectoryAuthorityRights::MUTATE_DIRECTORY,
        )
    }

    pub fn contains_authority(&self, child: &Self) -> bool {
        child
            .directory_preopens
            .iter()
            .all(|preopen| self.has_directory_authority(preopen.source_path(), preopen.rights()))
            && child
                .cwd
                .as_ref()
                .is_none_or(|cwd| self.has_directory_authority(cwd.source_path(), cwd.rights()))
            && self.network.contains(child.network)
            && self.clock.contains(child.clock)
            && self.terminal.contains(child.terminal)
            && self.process.contains(child.process)
            && self.link.contains(child.link)
            && self.mount.contains(child.mount)
            && self.device_classes.contains(child.device_classes)
            && self.diagnostics.contains(child.diagnostics)
            && self.settings.contains(child.settings)
            && child
                .devices
                .iter()
                .all(|device| self.devices.contains(device))
    }

    fn has_directory_authority(&self, path: &str, rights: DirectoryAuthorityRights) -> bool {
        self.directory_preopens.iter().any(|preopen| {
            directory_contains(preopen.source_path(), path) && preopen.rights().contains(rights)
        }) || self.cwd.as_ref().is_some_and(|cwd| {
            directory_contains(cwd.source_path(), path) && cwd.rights().contains(rights)
        })
    }
}

/// Caps for tests that exercise a claim below the authority check, where
/// the check itself is not the thing under test.
#[cfg(test)]
pub(crate) mod test_caps {
    use super::{
        AudioClaimCap, DeviceClassRights, DeviceGrantClaimCap, DisplayClaimCap, GpuOpenCap,
        InputClaimCap, ProcessAuthority,
    };
    use crate::device::DeviceName;

    fn holding(classes: DeviceClassRights) -> ProcessAuthority {
        let mut authority = ProcessAuthority::empty();
        authority.grant_device_classes(classes);
        authority
    }

    pub(crate) fn display() -> DisplayClaimCap {
        holding(DeviceClassRights::DISPLAY)
            .derive_display_claim_cap()
            .expect("the authority holds the display class")
    }

    pub(crate) fn input() -> InputClaimCap {
        holding(DeviceClassRights::INPUT)
            .derive_input_claim_cap()
            .expect("the authority holds the input class")
    }

    pub(crate) fn gpu() -> GpuOpenCap {
        holding(DeviceClassRights::GPU)
            .derive_gpu_open_cap()
            .expect("the authority holds the GPU class")
    }

    pub(crate) fn audio() -> AudioClaimCap {
        holding(DeviceClassRights::AUDIO)
            .derive_audio_claim_cap()
            .expect("the authority holds the audio class")
    }

    pub(crate) fn device(name: &str) -> DeviceGrantClaimCap {
        let mut authority = ProcessAuthority::empty();
        authority
            .grant_device(DeviceName::new(name).expect("the test names are short"))
            .expect("one named device fits");
        authority
            .derive_device_grant_cap(name)
            .expect("the authority names the device")
    }
}

fn normalize_authority_path(path: &str) -> Result<String, ProcessAuthorityError> {
    resolve_absolute_path(path).map_err(ProcessAuthorityError::InvalidPath)
}

fn directory_contains(directory: &str, path: &str) -> bool {
    if directory == "/" {
        return path.starts_with('/');
    }
    path.strip_prefix(directory)
        .is_some_and(|remainder| remainder.starts_with('/'))
        || path == directory
}

#[cfg(test)]
mod tests {
    use super::{
        ClockAuthorityRights, DeviceClassRights, DiagnosticAuthorityRights,
        DirectoryAuthorityRights, LinkAuthorityRights, MountAuthorityRights,
        NetworkAuthorityRights, ProcessAuthority, ProcessAuthorityError, ProcessAuthorityRights,
        SettingsAuthorityRights,
        TerminalAuthorityRights,
    };
    use crate::device::{DeviceName, MAX_GRANTS};

    fn device_name(text: &str) -> DeviceName {
        DeviceName::new(text).expect("the test names are short")
    }

    #[test]
    fn child_grant_must_be_subset_of_caller_authority() {
        let parent = ProcessAuthority::root();
        let child = parent
            .derive_directory_preopen(
                "/bin",
                "/",
                DirectoryAuthorityRights::READ | DirectoryAuthorityRights::EXECUTE,
            )
            .expect("root may grant a narrower executable directory");
        assert_eq!(child.source_path(), "/bin");
        assert_eq!(child.guest_name(), "/");
        assert_eq!(
            child.rights(),
            DirectoryAuthorityRights::READ | DirectoryAuthorityRights::EXECUTE
        );
    }

    #[test]
    fn child_grant_rejects_right_widening() {
        let mut parent = ProcessAuthority::empty();
        parent.insert_directory_preopen(
            super::DirectoryPreopen::new("/bin", "/", DirectoryAuthorityRights::READ)
                .expect("test preopen should be valid"),
        );
        let error = parent
            .derive_directory_preopen(
                "/bin",
                "/",
                DirectoryAuthorityRights::READ | DirectoryAuthorityRights::WRITE,
            )
            .expect_err("write must not be created from a read-only parent");
        assert!(matches!(
            error,
            ProcessAuthorityError::DirectoryGrantExceedsAuthority(_)
        ));
    }

    #[test]
    fn child_grant_rejects_path_escape() {
        let parent = ProcessAuthority::root();
        let error = parent
            .derive_directory_preopen("../bin", "/", DirectoryAuthorityRights::READ)
            .expect_err("relative escape must not normalize into authority");
        assert!(matches!(error, ProcessAuthorityError::InvalidPath(_)));
    }

    #[test]
    fn child_network_grant_must_be_subset_of_caller_authority() {
        let mut parent = ProcessAuthority::empty();
        parent.grant_network_rights(NetworkAuthorityRights::TCP | NetworkAuthorityRights::DNS);

        let rights = parent
            .derive_network_rights(NetworkAuthorityRights::TCP)
            .expect("caller may derive a subset of network rights");
        assert_eq!(rights, NetworkAuthorityRights::TCP);

        let error = parent
            .derive_network_rights(NetworkAuthorityRights::UDP)
            .expect_err("UDP must not be created from TCP/DNS authority");
        assert!(matches!(
            error,
            ProcessAuthorityError::NetworkGrantExceedsAuthority(_)
        ));
    }

    #[test]
    fn child_clock_grant_must_be_subset_of_caller_authority() {
        let parent = ProcessAuthority::empty();
        let error = parent
            .derive_clock_rights(ClockAuthorityRights::SET_WALL_CLOCK)
            .expect_err("wall-clock mutation authority must be explicit");
        assert!(matches!(
            error,
            ProcessAuthorityError::ClockGrantExceedsAuthority(_)
        ));
    }

    #[test]
    fn child_terminal_grant_must_be_subset_of_caller_authority() {
        let mut parent = ProcessAuthority::empty();
        parent.grant_terminal_rights(TerminalAuthorityRights::OUTPUT);

        let rights = parent
            .derive_terminal_rights(TerminalAuthorityRights::OUTPUT)
            .expect("caller may derive terminal output authority");
        assert_eq!(rights, TerminalAuthorityRights::OUTPUT);

        let error = parent
            .derive_terminal_rights(TerminalAuthorityRights::CONTROL)
            .expect_err("TTY control must not be created from output authority");
        assert!(matches!(
            error,
            ProcessAuthorityError::TerminalGrantExceedsAuthority(_)
        ));
    }

    #[test]
    fn typed_network_caps_can_only_derive_from_matching_authority() {
        let mut authority = ProcessAuthority::empty();
        authority.grant_network_rights(NetworkAuthorityRights::TCP | NetworkAuthorityRights::DNS);

        assert_eq!(
            authority
                .derive_tcp_cap()
                .expect("TCP authority should derive TcpCap")
                .network_cap()
                .rights(),
            NetworkAuthorityRights::TCP
        );
        assert_eq!(
            authority
                .derive_dns_cap()
                .expect("DNS authority should derive DnsCap")
                .network_cap()
                .rights(),
            NetworkAuthorityRights::DNS
        );
        assert!(matches!(
            authority
                .derive_udp_cap()
                .expect_err("UDP cap must not derive from TCP/DNS authority"),
            ProcessAuthorityError::NetworkGrantExceedsAuthority(_)
        ));
    }

    #[test]
    fn typed_clock_and_terminal_caps_require_matching_rights() {
        let mut authority = ProcessAuthority::empty();
        authority.grant_clock_rights(ClockAuthorityRights::SET_WALL_CLOCK);
        authority.grant_terminal_rights(TerminalAuthorityRights::CONTROL);

        assert_eq!(
            authority
                .derive_set_wall_clock_cap()
                .expect("wall-clock authority should derive typed cap")
                .rights(),
            ClockAuthorityRights::SET_WALL_CLOCK
        );
        assert_eq!(
            authority
                .derive_tty_control_cap()
                .expect("TTY control authority should derive typed cap")
                .rights(),
            TerminalAuthorityRights::CONTROL
        );
        assert!(matches!(
            authority
                .derive_terminal_input_cap()
                .expect_err("terminal input cap must require input authority"),
            ProcessAuthorityError::TerminalGrantExceedsAuthority(_)
        ));
    }

    #[test]
    fn typed_process_caps_require_matching_rights() {
        let mut authority = ProcessAuthority::empty();
        authority
            .grant_process_rights(ProcessAuthorityRights::SPAWN | ProcessAuthorityRights::EXEC);

        assert_eq!(
            authority
                .derive_spawn_authority()
                .expect("spawn authority should derive SpawnAuthority")
                .rights(),
            ProcessAuthorityRights::SPAWN
        );
        assert_eq!(
            authority
                .derive_exec_authority()
                .expect("exec authority should derive ExecAuthority")
                .rights(),
            ProcessAuthorityRights::EXEC
        );
        assert!(matches!(
            authority
                .derive_fork_authority()
                .expect_err("fork authority must require FORK right"),
            ProcessAuthorityError::ProcessGrantExceedsAuthority(_)
        ));
    }

    #[test]
    fn typed_link_caps_require_matching_rights() {
        let mut authority = ProcessAuthority::empty();
        authority
            .grant_link_rights(LinkAuthorityRights::SOURCE | LinkAuthorityRights::SYMLINK_READ);

        assert_eq!(
            authority
                .derive_link_source_cap()
                .expect("source link cap should derive")
                .rights(),
            LinkAuthorityRights::SOURCE
        );
        assert_eq!(
            authority
                .derive_symlink_read_cap()
                .expect("readlink cap should derive")
                .rights(),
            LinkAuthorityRights::SYMLINK_READ
        );
        assert!(matches!(
            authority
                .derive_symlink_create_cap()
                .expect_err("symlink create cap must require symlink-create right"),
            ProcessAuthorityError::LinkGrantExceedsAuthority(_)
        ));
    }

    #[test]
    fn mount_requires_the_mount_right() {
        let mut authority = ProcessAuthority::empty();
        authority.insert_directory_preopen(
            super::DirectoryPreopen::new(
                "/",
                "/",
                DirectoryAuthorityRights::READ | DirectoryAuthorityRights::MUTATE_DIRECTORY,
            )
            .expect("test preopen should be valid"),
        );

        assert!(matches!(
            authority
                .derive_mount_cap("/mnt")
                .expect_err("a mount without the mount right must be refused"),
            ProcessAuthorityError::MountGrantExceedsAuthority(MountAuthorityRights::MOUNT)
        ));
        assert!(matches!(
            authority
                .derive_unmount_cap("/mnt")
                .expect_err("an unmount without the unmount right must be refused"),
            ProcessAuthorityError::MountGrantExceedsAuthority(MountAuthorityRights::UNMOUNT)
        ));
    }

    #[test]
    fn mount_point_must_lie_under_mutate_directory_authority() {
        let mut authority = ProcessAuthority::empty();
        authority.grant_mount_rights(MountAuthorityRights::MOUNT);
        authority.insert_directory_preopen(
            super::DirectoryPreopen::new("/", "/", DirectoryAuthorityRights::READ)
                .expect("test preopen should be valid"),
        );
        authority.insert_directory_preopen(
            super::DirectoryPreopen::new(
                "/data",
                "/data",
                DirectoryAuthorityRights::READ | DirectoryAuthorityRights::MUTATE_DIRECTORY,
            )
            .expect("test preopen should be valid"),
        );

        assert!(matches!(
            authority
                .derive_mount_cap("/mnt")
                .expect_err("a read-only directory must not take a mount"),
            ProcessAuthorityError::MountPointOutsideAuthority(_)
        ));
        assert!(matches!(
            authority
                .derive_mount_cap("/data/../data/disk")
                .expect_err("an absolute mount point never climbs with `..`"),
            ProcessAuthorityError::InvalidPath(_)
        ));
        let cap = authority
            .derive_mount_cap("/data/disk")
            .expect("a directory the caller may change takes a mount");
        assert_eq!(cap.point(), "/data/disk");
    }

    #[test]
    fn root_holds_mount_authority_and_delegates_a_subset() {
        let root = ProcessAuthority::root();
        assert_eq!(
            root.derive_mount_cap("/mnt")
                .expect("root may mount anywhere")
                .point(),
            "/mnt"
        );

        let mut child = ProcessAuthority::empty();
        child.grant_mount_rights(
            root.derive_mount_rights(MountAuthorityRights::MOUNT)
                .expect("root may delegate the mount right"),
        );
        assert!(root.contains_authority(&child));
        assert!(!child.contains_authority(&root));
        assert!(matches!(
            child.derive_mount_rights(MountAuthorityRights::UNMOUNT),
            Err(ProcessAuthorityError::MountGrantExceedsAuthority(_))
        ));
    }

    #[test]
    fn cwd_is_set_only_from_derived_directory_capability() {
        let mut authority = ProcessAuthority::empty();
        authority.insert_directory_preopen(
            super::DirectoryPreopen::new("/workspace", "/", DirectoryAuthorityRights::READ)
                .expect("test preopen should be valid"),
        );

        let cwd = authority
            .derive_directory_cap("/workspace", "/", DirectoryAuthorityRights::READ)
            .expect("authorized directory cap should derive");
        authority.chdir(cwd);
        assert_eq!(
            authority.cwd().expect("cwd should be set").source_path(),
            "/workspace"
        );

        assert!(matches!(
            authority
                .derive_directory_cap("/private", "/", DirectoryAuthorityRights::READ)
                .expect_err("cwd cap must not come from an unauthorized path"),
            ProcessAuthorityError::DirectoryGrantExceedsAuthority(_)
        ));
    }

    #[test]
    fn root_has_explicit_cwd_but_empty_authority_has_none() {
        assert!(ProcessAuthority::empty().cwd().is_none());
        assert_eq!(
            ProcessAuthority::root()
                .cwd()
                .expect("root authority should carry cwd")
                .guest_name(),
            "/"
        );
    }

    #[test]
    fn fork_and_exec_never_widen_authority() {
        let mut authority = ProcessAuthority::empty();
        authority.insert_directory_preopen(
            super::DirectoryPreopen::new("/bin", "/", DirectoryAuthorityRights::READ)
                .expect("test preopen should be valid"),
        );
        authority.grant_network_rights(NetworkAuthorityRights::TCP);
        authority.grant_process_rights(ProcessAuthorityRights::FORK | ProcessAuthorityRights::EXEC);

        let forked = authority.fork_inherited();
        let execed = authority.exec_preserved();

        assert_eq!(forked, authority);
        assert_eq!(execed, authority);
    }

    #[test]
    fn path_create_or_replace_requires_write_and_directory_mutation() {
        let mut write_only = ProcessAuthority::empty();
        write_only.insert_directory_preopen(
            super::DirectoryPreopen::new("/out", "/", DirectoryAuthorityRights::WRITE)
                .expect("test preopen should be valid"),
        );

        let mut writable_directory = ProcessAuthority::empty();
        writable_directory.insert_directory_preopen(
            super::DirectoryPreopen::new(
                "/out",
                "/",
                DirectoryAuthorityRights::WRITE | DirectoryAuthorityRights::MUTATE_DIRECTORY,
            )
            .expect("test preopen should be valid"),
        );

        assert!(write_only.can_write_path("/out/program"));
        assert!(!write_only.can_create_or_replace_path("/out/program"));
        assert!(writable_directory.can_create_or_replace_path("/out/program"));
    }

    #[test]
    fn authority_subset_check_covers_all_authority_kinds() {
        let parent = ProcessAuthority::root();
        let mut child = ProcessAuthority::empty();
        child.insert_directory_preopen(
            super::DirectoryPreopen::new("/bin", "/", DirectoryAuthorityRights::READ)
                .expect("test preopen should be valid"),
        );
        child.grant_network_rights(NetworkAuthorityRights::TCP);
        child.grant_clock_rights(ClockAuthorityRights::SET_WALL_CLOCK);
        child.grant_terminal_rights(TerminalAuthorityRights::OUTPUT);
        child.grant_process_rights(ProcessAuthorityRights::SPAWN);
        child.grant_link_rights(LinkAuthorityRights::SOURCE);
        child.grant_device_classes(DeviceClassRights::DISPLAY);
        child.grant_diagnostic_rights(DiagnosticAuthorityRights::OBSERVE_MACHINE);
        child.grant_settings_rights(SettingsAuthorityRights::WRITE);
        child
            .grant_device(device_name("pci:0000:00:04.0"))
            .expect("one named device fits");

        let mut parent = parent;
        parent.grant_device_classes(DeviceClassRights::all());
        parent.grant_diagnostic_rights(DiagnosticAuthorityRights::OBSERVE_MACHINE);
        parent.grant_settings_rights(SettingsAuthorityRights::WRITE);
        parent
            .grant_device(device_name("pci:0000:00:04.0"))
            .expect("one named device fits");
        assert!(parent.contains_authority(&child));

        let mut overgrant = child.clone();
        overgrant.grant_link_rights(LinkAuthorityRights::SYMLINK_CREATE);
        assert!(parent.contains_authority(&overgrant));

        let mut narrow_parent = ProcessAuthority::empty();
        narrow_parent.grant_link_rights(LinkAuthorityRights::SOURCE);
        assert!(!narrow_parent.contains_authority(&child));

        let mut class_only_parent = ProcessAuthority::root();
        class_only_parent.grant_device_classes(DeviceClassRights::DISPLAY);
        assert!(!class_only_parent.contains_authority(&child));

        let mut device_only_parent = ProcessAuthority::root();
        device_only_parent
            .grant_device(device_name("pci:0000:00:04.0"))
            .expect("one named device fits");
        assert!(!device_only_parent.contains_authority(&child));

        let mut diagnostics_less_parent = ProcessAuthority::root();
        diagnostics_less_parent.grant_device_classes(DeviceClassRights::all());
        diagnostics_less_parent
            .grant_device(device_name("pci:0000:00:04.0"))
            .expect("one named device fits");
        diagnostics_less_parent.grant_settings_rights(SettingsAuthorityRights::WRITE);
        assert!(!diagnostics_less_parent.contains_authority(&child));

        let mut settings_less_parent = ProcessAuthority::root();
        settings_less_parent.grant_device_classes(DeviceClassRights::all());
        settings_less_parent.grant_diagnostic_rights(DiagnosticAuthorityRights::OBSERVE_MACHINE);
        settings_less_parent
            .grant_device(device_name("pci:0000:00:04.0"))
            .expect("one named device fits");
        assert!(!settings_less_parent.contains_authority(&child));
    }

    #[test]
    fn settings_grant_must_be_subset_of_caller_authority() {
        let mut parent = ProcessAuthority::empty();
        parent.grant_settings_rights(SettingsAuthorityRights::WRITE);

        assert_eq!(
            parent
                .derive_settings_rights(SettingsAuthorityRights::WRITE)
                .expect("caller may derive a right it holds"),
            SettingsAuthorityRights::WRITE
        );
        assert_eq!(
            parent
                .derive_settings_rights(SettingsAuthorityRights::empty())
                .expect_err("an empty settings grant is a mistake"),
            ProcessAuthorityError::EmptySettingsRights
        );
        assert!(parent.derive_write_settings_cap().is_ok());

        let holder_less = ProcessAuthority::empty();
        assert_eq!(
            holder_less
                .derive_settings_rights(SettingsAuthorityRights::WRITE)
                .expect_err("a caller without the right must not hand it on"),
            ProcessAuthorityError::SettingsGrantExceedsAuthority(SettingsAuthorityRights::WRITE)
        );
        assert!(matches!(
            holder_less.derive_write_settings_cap(),
            Err(ProcessAuthorityError::SettingsGrantExceedsAuthority(_))
        ));

        // The observe right is a separate power: holding it grants no
        // write.
        let mut observer = ProcessAuthority::empty();
        observer.grant_diagnostic_rights(DiagnosticAuthorityRights::OBSERVE_MACHINE);
        assert!(observer.derive_write_settings_cap().is_err());
    }

    #[test]
    fn root_authority_holds_no_settings_rights() {
        let root = ProcessAuthority::root();
        assert_eq!(root.settings_rights(), SettingsAuthorityRights::empty());
        assert!(root.derive_write_settings_cap().is_err());
    }

    #[test]
    fn diagnostic_grant_must_be_subset_of_caller_authority() {
        let mut parent = ProcessAuthority::empty();
        parent.grant_diagnostic_rights(DiagnosticAuthorityRights::OBSERVE_MACHINE);

        assert_eq!(
            parent
                .derive_diagnostic_rights(DiagnosticAuthorityRights::OBSERVE_MACHINE)
                .expect("caller may derive a right it holds"),
            DiagnosticAuthorityRights::OBSERVE_MACHINE
        );
        assert_eq!(
            parent
                .derive_diagnostic_rights(DiagnosticAuthorityRights::empty())
                .expect_err("an empty diagnostic grant is a mistake"),
            ProcessAuthorityError::EmptyDiagnosticRights
        );
        assert!(parent.derive_observe_machine_cap().is_ok());

        let holder_less = ProcessAuthority::empty();
        assert_eq!(
            holder_less
                .derive_diagnostic_rights(DiagnosticAuthorityRights::OBSERVE_MACHINE)
                .expect_err("a caller without the right must not hand it on"),
            ProcessAuthorityError::DiagnosticGrantExceedsAuthority(
                DiagnosticAuthorityRights::OBSERVE_MACHINE
            )
        );
        assert!(matches!(
            holder_less.derive_observe_machine_cap(),
            Err(ProcessAuthorityError::DiagnosticGrantExceedsAuthority(_))
        ));
    }

    #[test]
    fn root_authority_holds_no_diagnostic_rights() {
        let root = ProcessAuthority::root();
        assert_eq!(root.diagnostic_rights(), DiagnosticAuthorityRights::empty());
        assert!(root.derive_observe_machine_cap().is_err());
    }

    #[test]
    fn device_class_grant_must_be_subset_of_caller_authority() {
        let mut parent = ProcessAuthority::empty();
        parent.grant_device_classes(DeviceClassRights::DISPLAY | DeviceClassRights::INPUT);

        assert_eq!(
            parent
                .derive_device_class_rights(DeviceClassRights::INPUT)
                .expect("caller may derive a subset of device classes"),
            DeviceClassRights::INPUT
        );
        assert_eq!(
            parent
                .derive_device_class_rights(DeviceClassRights::empty())
                .expect_err("an empty class grant is a mistake"),
            ProcessAuthorityError::EmptyDeviceClassRights
        );
        assert_eq!(
            parent
                .derive_device_class_rights(DeviceClassRights::DISPLAY | DeviceClassRights::GPU)
                .expect_err("GPU must not be created from display and input authority"),
            ProcessAuthorityError::DeviceClassGrantExceedsAuthority(
                DeviceClassRights::DISPLAY | DeviceClassRights::GPU
            )
        );

        assert!(parent.derive_display_claim_cap().is_ok());
        assert!(parent.derive_input_claim_cap().is_ok());
        assert!(matches!(
            parent.derive_gpu_open_cap(),
            Err(ProcessAuthorityError::DeviceClassGrantExceedsAuthority(_))
        ));
        assert!(matches!(
            parent.derive_audio_claim_cap(),
            Err(ProcessAuthorityError::DeviceClassGrantExceedsAuthority(_))
        ));
    }

    #[test]
    fn named_device_cap_derives_only_for_a_granted_name() {
        let granted = device_name("pci:0000:00:04.0");
        let mut authority = ProcessAuthority::empty();
        authority
            .grant_device(granted)
            .expect("one named device fits");
        authority
            .grant_device(granted)
            .expect("granting a name twice is the same grant");
        assert_eq!(authority.granted_devices(), &[granted]);

        let cap = authority
            .derive_device_grant_cap("pci:0000:00:04.0")
            .expect("the granted name derives a cap");
        assert_eq!(cap.name(), &granted);

        assert_eq!(
            authority
                .derive_device_grant_cap("pci:0000:00:05.0")
                .expect_err("an ungranted name must not derive a cap"),
            ProcessAuthorityError::DeviceGrantExceedsAuthority(device_name("pci:0000:00:05.0"))
        );
        let too_long = "x".repeat(crate::device::MAX_DEVICE_NAME + 1);
        assert_eq!(
            authority
                .derive_device_grant_cap(&too_long)
                .expect_err("a name the registry cannot store is not granted"),
            ProcessAuthorityError::DeviceNameTooLong
        );
    }

    #[test]
    fn named_device_grants_are_bounded_by_the_registry() {
        let mut authority = ProcessAuthority::empty();
        for index in 0..MAX_GRANTS {
            authority
                .grant_device(device_name(&alloc::format!("dev{index}")))
                .expect("the registry's capacity fits");
        }
        assert_eq!(
            authority
                .grant_device(device_name("one-too-many"))
                .expect_err("a ninth named device has no registry slot"),
            ProcessAuthorityError::TooManyDeviceGrants
        );
    }

    #[test]
    fn root_authority_holds_no_device_rights() {
        let root = ProcessAuthority::root();
        assert_eq!(root.device_class_rights(), DeviceClassRights::empty());
        assert!(root.granted_devices().is_empty());
        assert!(root.derive_display_claim_cap().is_err());
        assert!(root.derive_input_claim_cap().is_err());
        assert!(root.derive_gpu_open_cap().is_err());
        assert!(root.derive_audio_claim_cap().is_err());
    }
}
