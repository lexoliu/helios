//! Host-side packet paths the inspector can give a Helios guest.
//!
//! The portable default is QEMU's built-in `user` (slirp) backend: it
//! needs no privileges and no host provisioning, but it emulates a single
//! queue pair inside the QEMU process and offers neither segmentation nor
//! checksum offload. Nothing the guest's virtio-net driver negotiates for
//! performance — `VIRTIO_NET_F_MQ`, `HOST_TSO4/6`, `CSUM` — is observable
//! through it, so a measurement taken over slirp says nothing about the
//! driver's multi-queue or offload paths.
//!
//! The other backends exist so those paths can be measured. On Linux a
//! multi-queue `tap` device with `vhost=on` gives the guest real per-CPU
//! queue pairs served by the host kernel's vhost-net threads, with
//! checksum and TSO offload negotiated end to end. On macOS the `vmnet`
//! framework backends give a real host packet path (still single queue),
//! and `socket-vmnet` reaches the same framework through an unprivileged
//! daemon.
//!
//! Every backend states which host it needs, how many queue pairs it can
//! serve, and which options it consumes. A combination a backend cannot
//! satisfy is a typed error from [`VmNetwork::render`] — the inspector
//! never quietly degrades a requested backend into a weaker one, because
//! a silently downgraded packet path turns a performance measurement into
//! a lie.

use std::fmt;
use std::fs;
use std::net::Ipv4Addr;
use std::os::unix::fs::FileTypeExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use clap::{Args as ClapArgs, ValueEnum};
use console::style;
use ipnet::Ipv4Net;
use serde::{Deserialize, Serialize};

use super::VirtioDeviceProfile;
use super::qemu::QemuOptions;

/// QEMU netdev identifier shared by the backend and the device.
const NET_ID: &str = "net0";

/// File descriptor `socket_vmnet_client` hands the QEMU it execs.
const SOCKET_VMNET_FD: u8 = 3;

/// Default `socket_vmnet` endpoint created by the upstream launchd job.
const DEFAULT_SOCKET_VMNET_PATH: &str = "/opt/socket_vmnet/var/run/socket_vmnet";

/// Default `socket_vmnet` launcher, resolved through `PATH`.
const DEFAULT_SOCKET_VMNET_CLIENT: &str = "socket_vmnet_client";

/// `IFNAMSIZ - 1`: the longest interface name the Linux kernel accepts.
const MAX_INTERFACE_NAME_LEN: usize = 15;

/// `IFF_MULTI_QUEUE` from `linux/if_tun.h`.
const IFF_MULTI_QUEUE: u32 = 0x0100;

/// Where sysfs publishes per-interface state.
const SYSFS_NET_ROOT: &str = "/sys/class/net";

/// nftables table the tap helper owns end to end.
const NAT_TABLE: &str = "helios-nat";

/// Largest DHCP pool the setup helper carves out of the bridge subnet.
const DHCP_POOL_SIZE: usize = 128;

/// DHCP lease time handed to guests by the optional dnsmasq responder.
const DHCP_LEASE_TIME: &str = "12h";

/// Documentation every diagnostic points at.
const NETWORKING_DOC: &str = "docs/networking.md";

/// The host operating system a backend runs on.
///
/// Carried explicitly rather than read from [`std::env::consts::OS`] at
/// the point of use so backend selection is a pure function the unit
/// tests can drive for both hosts from either host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HostPlatform(&'static str);

impl HostPlatform {
    pub(crate) const LINUX: Self = Self("linux");
    pub(crate) const MACOS: Self = Self("macos");

    /// The host this inspector process is running on.
    pub(crate) fn current() -> Self {
        Self(std::env::consts::OS)
    }
}

impl fmt::Display for HostPlatform {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

/// How a machine profile attaches its virtio-net device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VmNetworkProfile {
    /// Device-tree/MMIO transport, used by the `virt` machines.
    VirtioMmio,
    /// PCI transport, used by `q35`.
    VirtioPci,
}

impl VmNetworkProfile {
    /// QEMU device model implementing this transport.
    fn device_model(self) -> &'static str {
        match self {
            Self::VirtioMmio => "virtio-net-device",
            Self::VirtioPci => "virtio-net-pci",
        }
    }
}

/// Host-side packet path QEMU gives the guest's virtio-net device.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Serialize, Deserialize, PartialOrd, Ord,
)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum VmNetworkBackend {
    /// QEMU's built-in slirp stack. Portable, unprivileged, single queue,
    /// no offload.
    #[default]
    User,
    /// A pre-provisioned Linux tap device driven by vhost-net. The only
    /// backend that can serve more than one queue pair.
    Tap,
    /// macOS `vmnet` in shared (NAT) mode. Needs root or the
    /// `com.apple.vm.networking` entitlement.
    VmnetShared,
    /// macOS `vmnet` bridged onto a host interface. Same privileges as
    /// `vmnet-shared`.
    VmnetBridged,
    /// macOS `vmnet` reached through the unprivileged `socket_vmnet`
    /// daemon, which QEMU is exec'd under.
    SocketVmnet,
}

/// Whether a backend consumes an interface name, and what it means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterfaceNameUse {
    Required(&'static str),
    Rejected,
}

impl VmNetworkBackend {
    /// Stable spelling used by the CLI, the config file, and diagnostics.
    const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Tap => "tap",
            Self::VmnetShared => "vmnet-shared",
            Self::VmnetBridged => "vmnet-bridged",
            Self::SocketVmnet => "socket-vmnet",
        }
    }

    /// QEMU `-netdev` type implementing this backend.
    const fn netdev_kind(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Tap => "tap",
            Self::VmnetShared => "vmnet-shared",
            Self::VmnetBridged => "vmnet-bridged",
            // socket_vmnet hands QEMU an already-connected unix socket.
            Self::SocketVmnet => "socket",
        }
    }

    /// Host this backend needs, or `None` when any host can provide it.
    const fn required_host(self) -> Option<HostPlatform> {
        match self {
            Self::User => None,
            Self::Tap => Some(HostPlatform::LINUX),
            Self::VmnetShared | Self::VmnetBridged | Self::SocketVmnet => Some(HostPlatform::MACOS),
        }
    }

    /// Why this backend cannot serve more than one queue pair, or `None`
    /// when it can.
    const fn single_queue_reason(self) -> Option<&'static str> {
        match self {
            Self::User => {
                Some("QEMU's slirp stack emulates exactly one queue pair inside the QEMU process")
            }
            Self::Tap => None,
            Self::VmnetShared | Self::VmnetBridged => {
                Some("the macOS vmnet framework exposes a single packet path per interface")
            }
            Self::SocketVmnet => Some("socket_vmnet multiplexes every guest onto one unix stream"),
        }
    }

    /// Whether this backend can serve several virtio-net queue pairs.
    const fn supports_multi_queue(self) -> bool {
        self.single_queue_reason().is_none()
    }

    /// Whether `--net-queues` defaults to `--smp` on this backend.
    const fn interface_name(self) -> InterfaceNameUse {
        match self {
            Self::Tap => InterfaceNameUse::Required(
                "the multi-queue tap device created by `helios-inspector vm net-setup`",
            ),
            Self::VmnetBridged => {
                InterfaceNameUse::Required("the host interface vmnet bridges onto, such as `en0`")
            }
            Self::User | Self::VmnetShared | Self::SocketVmnet => InterfaceNameUse::Rejected,
        }
    }

    /// Whether `--net-bridge` is meaningful for this backend.
    const fn uses_bridge(self) -> bool {
        matches!(self, Self::Tap)
    }

    /// Whether the `socket_vmnet` options are meaningful for this backend.
    const fn uses_socket_vmnet(self) -> bool {
        matches!(self, Self::SocketVmnet)
    }
}

impl fmt::Display for VmNetworkBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Shape of a virtio-net device property's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VirtioNetPropertyValue {
    /// QEMU `on`/`off` toggle.
    Toggle,
    /// Non-negative integer.
    Count,
}

/// One virtio-net device property `--net-device-prop` accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VirtioNetPropertySpec {
    name: &'static str,
    value: VirtioNetPropertyValue,
    /// Inspector flag that owns this property when the inspector derives
    /// it, so a caller cannot set the same knob from two sources.
    owner: Option<&'static str>,
}

/// The virtio-net device properties the inspector understands.
///
/// Anything outside this table is rejected instead of forwarded: QEMU
/// reports an unknown device property only once the VM is already being
/// created, long after the inspector could have said which name was
/// wrong.
const VIRTIO_NET_PROPERTIES: &[VirtioNetPropertySpec] = &[
    VirtioNetPropertySpec {
        name: "csum",
        value: VirtioNetPropertyValue::Toggle,
        owner: None,
    },
    VirtioNetPropertySpec {
        name: "guest_csum",
        value: VirtioNetPropertyValue::Toggle,
        owner: None,
    },
    VirtioNetPropertySpec {
        name: "host_tso4",
        value: VirtioNetPropertyValue::Toggle,
        owner: None,
    },
    VirtioNetPropertySpec {
        name: "host_tso6",
        value: VirtioNetPropertyValue::Toggle,
        owner: None,
    },
    VirtioNetPropertySpec {
        name: "mrg_rxbuf",
        value: VirtioNetPropertyValue::Toggle,
        owner: None,
    },
    VirtioNetPropertySpec {
        name: "event_idx",
        value: VirtioNetPropertyValue::Toggle,
        owner: None,
    },
    VirtioNetPropertySpec {
        name: "indirect_desc",
        value: VirtioNetPropertyValue::Toggle,
        owner: None,
    },
    VirtioNetPropertySpec {
        name: "packed",
        value: VirtioNetPropertyValue::Toggle,
        owner: Some("--virtio-packed"),
    },
    VirtioNetPropertySpec {
        name: "mq",
        value: VirtioNetPropertyValue::Toggle,
        owner: Some("--net-queues"),
    },
    VirtioNetPropertySpec {
        name: "vectors",
        value: VirtioNetPropertyValue::Count,
        owner: Some("--net-queues"),
    },
];

fn supported_property_names() -> String {
    VIRTIO_NET_PROPERTIES
        .iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A validated `key=value` virtio-net device property.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct VirtioNetProperty {
    spec: &'static VirtioNetPropertySpec,
    value: String,
}

impl VirtioNetProperty {
    fn name(&self) -> &'static str {
        self.spec.name
    }
}

impl std::str::FromStr for VirtioNetProperty {
    type Err = VmNetworkError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let (name, value) =
            raw.split_once('=')
                .ok_or_else(|| VmNetworkError::MalformedDeviceProperty {
                    raw: raw.to_owned(),
                })?;
        let spec = VIRTIO_NET_PROPERTIES
            .iter()
            .find(|spec| spec.name == name)
            .ok_or_else(|| VmNetworkError::UnknownDeviceProperty {
                name: name.to_owned(),
                supported: supported_property_names(),
            })?;
        match spec.value {
            VirtioNetPropertyValue::Toggle if !matches!(value, "on" | "off") => {
                return Err(VmNetworkError::NonToggleDeviceProperty {
                    name: spec.name,
                    value: value.to_owned(),
                });
            }
            VirtioNetPropertyValue::Count if value.parse::<u32>().is_err() => {
                return Err(VmNetworkError::NonCountDeviceProperty {
                    name: spec.name,
                    value: value.to_owned(),
                });
            }
            _ => {}
        }
        Ok(Self {
            spec,
            value: value.to_owned(),
        })
    }
}

impl TryFrom<String> for VirtioNetProperty {
    type Error = VmNetworkError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        raw.parse()
    }
}

impl From<VirtioNetProperty> for String {
    fn from(property: VirtioNetProperty) -> Self {
        format!("{}={}", property.spec.name, property.value)
    }
}

/// Everything that can go wrong while turning a requested backend into
/// QEMU arguments.
#[derive(Debug, thiserror::Error)]
pub(crate) enum VmNetworkError {
    #[error("`--net-device-prop {raw}` is not a `key=value` pair")]
    MalformedDeviceProperty { raw: String },

    #[error("unknown virtio-net device property `{name}`; supported properties are {supported}")]
    UnknownDeviceProperty { name: String, supported: String },

    #[error("virtio-net device property `{name}` expects `on` or `off`, got `{value}`")]
    NonToggleDeviceProperty { name: &'static str, value: String },

    #[error("virtio-net device property `{name}` expects a non-negative count, got `{value}`")]
    NonCountDeviceProperty { name: &'static str, value: String },

    #[error(
        "virtio-net device property `{name}` is derived from `{owner}`; \
         pass `{owner}` instead of setting it with --net-device-prop"
    )]
    DerivedDeviceProperty {
        name: &'static str,
        owner: &'static str,
    },

    #[error("virtio-net device property `{name}` was given more than once")]
    DuplicateDeviceProperty { name: &'static str },

    #[error("the `{backend}` network backend needs a {required} host, but this host is {actual}")]
    UnsupportedHost {
        backend: VmNetworkBackend,
        required: HostPlatform,
        actual: HostPlatform,
    },

    #[error(
        "--net-queues {requested} is not available on the `{backend}` network backend: {reason}"
    )]
    UnsupportedQueueCount {
        backend: VmNetworkBackend,
        requested: u16,
        reason: &'static str,
    },

    #[error("--net-queues 0 is not a queue-pair count; every backend serves at least one pair")]
    ZeroQueues,

    #[error("the `{backend}` network backend does not use {option}")]
    UnsupportedOption {
        backend: VmNetworkBackend,
        option: &'static str,
    },

    #[error("the `{backend}` network backend requires {option}: {purpose}")]
    MissingOption {
        backend: VmNetworkBackend,
        option: &'static str,
        purpose: &'static str,
    },

    #[error("network interface name `{name}` is {length} bytes; the kernel accepts at most {max}")]
    InterfaceNameTooLong {
        name: String,
        length: usize,
        max: usize,
    },

    #[error("network interface name `{name}` must be non-empty and free of `/` and whitespace")]
    InvalidInterfaceName { name: String },

    #[error(
        "tap interface `{ifname}` does not exist; create it with \
         `helios-inspector vm net-setup --net-backend tap --net-ifname {ifname}`"
    )]
    TapInterfaceMissing { ifname: String },

    #[error("failed to read {path}")]
    SysfsUnreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path} holds `{raw}`, which is not a hexadecimal tun flag word")]
    TunFlagsUnparsable { path: PathBuf, raw: String },

    #[error(
        "tap interface `{ifname}` was created without IFF_MULTI_QUEUE (tun_flags {flags:#06x}), \
         so it cannot back {queues} queue pairs; recreate it with \
         `helios-inspector vm net-setup --net-backend tap --net-ifname {ifname}`"
    )]
    TapNotMultiQueue {
        ifname: String,
        flags: u32,
        queues: u16,
    },

    #[error(
        "tap interface `{ifname}` is enslaved to `{actual}`, not the requested bridge `{expected}`"
    )]
    TapBridgeMismatch {
        ifname: String,
        actual: String,
        expected: String,
    },

    #[error(
        "tap interface `{ifname}` is not enslaved to any bridge, but --net-bridge {expected} was requested"
    )]
    TapNotBridged { ifname: String, expected: String },

    #[error(
        "socket_vmnet endpoint {path} is not a unix socket; start the daemon (see {NETWORKING_DOC})"
    )]
    SocketVmnetEndpointMissing { path: PathBuf },

    #[error(
        "socket_vmnet launcher `{name}` was not found; pass --socket-vmnet-client (see {NETWORKING_DOC})"
    )]
    SocketVmnetClientMissing { name: String },

    #[error(
        "the `{backend}` network backend needs root, or a QEMU binary carrying the \
         com.apple.vm.networking entitlement (see {NETWORKING_DOC})"
    )]
    VmnetPrivilegeMissing { backend: VmNetworkBackend },

    #[error("failed to inspect the code-signing entitlements of {path}")]
    EntitlementProbeFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("QEMU binary `{name}` was not found")]
    QemuBinaryMissing { name: String },
}

/// Failures of the privileged `net-setup` / `net-teardown` helpers.
#[derive(Debug, thiserror::Error)]
pub(crate) enum VmNetworkSetupError {
    #[error(transparent)]
    Network(#[from] VmNetworkError),

    #[error(
        "`net-setup` provisions the `tap` backend only; the `{backend}` backend is either \
         provisioned by QEMU itself or by an out-of-band daemon (see {NETWORKING_DOC})"
    )]
    UnsupportedBackend { backend: VmNetworkBackend },

    #[error("`net-setup` needs a {required} host, but this host is {actual}")]
    UnsupportedHost {
        required: HostPlatform,
        actual: HostPlatform,
    },

    #[error("failed to run `{command}`")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },

    #[error("`{command}` exited with {status}")]
    CommandFailed { command: String, status: ExitStatus },

    #[error(
        "could not determine the uplink interface from the host default route; pass --net-uplink"
    )]
    UplinkUnknown,

    #[error("failed to decode `ip -json route show default` output")]
    RouteDecode {
        #[source]
        source: serde_json::Error,
    },

    #[error(
        "tap interface `{ifname}` already exists without IFF_MULTI_QUEUE; \
         run `helios-inspector vm net-teardown --net-ifname {ifname}` first"
    )]
    ExistingTapNotMultiQueue { ifname: String },

    #[error("failed to read the dnsmasq pid file {path}")]
    PidFileUnreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("dnsmasq pid file {path} holds `{raw}`, which is not a process id")]
    PidFileUnparsable { path: PathBuf, raw: String },

    #[error("bridge address {address} leaves no usable DHCP host range")]
    BridgeRangeEmpty { address: Ipv4Net },
}

/// Backend selection as it arrives from the command line.
#[derive(Debug, Clone, ClapArgs)]
pub(crate) struct VmNetworkArgs {
    /// Host-side packet path for the guest's virtio-net device.
    #[arg(long = "net-backend", value_enum)]
    pub(crate) backend: Option<VmNetworkBackend>,

    /// virtio-net queue pairs. Defaults to `--smp` on backends that can
    /// serve several, and to 1 on the backends that cannot.
    #[arg(long = "net-queues")]
    pub(crate) queues: Option<u16>,

    /// Host interface the backend attaches to.
    #[arg(long = "net-ifname")]
    pub(crate) ifname: Option<String>,

    /// Host bridge the tap interface is enslaved to.
    #[arg(long = "net-bridge")]
    pub(crate) bridge: Option<String>,

    /// `socket_vmnet` daemon endpoint.
    #[arg(long = "socket-vmnet-path")]
    pub(crate) socket_vmnet_path: Option<PathBuf>,

    /// `socket_vmnet` launcher QEMU is exec'd under.
    #[arg(long = "socket-vmnet-client")]
    pub(crate) socket_vmnet_client: Option<PathBuf>,

    /// virtio-net device property, `key=value`. Repeat for several.
    #[arg(long = "net-device-prop", value_name = "KEY=VALUE")]
    pub(crate) device_props: Vec<VirtioNetProperty>,
}

/// Backend selection as it can be pinned in the inspector VM config file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct VmNetworkFile {
    #[serde(default)]
    pub(crate) backend: Option<VmNetworkBackend>,
    #[serde(default)]
    pub(crate) queues: Option<u16>,
    #[serde(default)]
    pub(crate) ifname: Option<String>,
    #[serde(default)]
    pub(crate) bridge: Option<String>,
    #[serde(default)]
    pub(crate) socket_vmnet_path: Option<PathBuf>,
    #[serde(default)]
    pub(crate) socket_vmnet_client: Option<PathBuf>,
    #[serde(default)]
    pub(crate) device_props: Vec<VirtioNetProperty>,
}

/// A resolved backend request: command line over config file, with the
/// backend's own defaults applied only where the caller stayed silent.
#[derive(Debug, Clone)]
pub(crate) struct VmNetwork {
    backend: VmNetworkBackend,
    queues: Option<u16>,
    ifname: Option<String>,
    bridge: Option<String>,
    socket_vmnet_path: Option<PathBuf>,
    socket_vmnet_client: Option<PathBuf>,
    device_props: Vec<VirtioNetProperty>,
}

impl VmNetwork {
    /// Merges command-line arguments over config-file values.
    pub(crate) fn resolve(args: VmNetworkArgs, file: VmNetworkFile) -> Self {
        let device_props = if args.device_props.is_empty() {
            file.device_props
        } else {
            args.device_props
        };
        Self {
            backend: args.backend.or(file.backend).unwrap_or_default(),
            queues: args.queues.or(file.queues),
            ifname: args.ifname.or(file.ifname),
            bridge: args.bridge.or(file.bridge),
            socket_vmnet_path: args.socket_vmnet_path.or(file.socket_vmnet_path),
            socket_vmnet_client: args.socket_vmnet_client.or(file.socket_vmnet_client),
            device_props,
        }
    }

    /// Queue pairs this request resolves to, or why the backend cannot
    /// serve the requested count.
    pub(crate) fn queue_pairs(&self, smp: u16) -> Result<u16, VmNetworkError> {
        match self.queues {
            Some(0) => Err(VmNetworkError::ZeroQueues),
            Some(requested) if requested > 1 && !self.backend.supports_multi_queue() => {
                Err(VmNetworkError::UnsupportedQueueCount {
                    backend: self.backend,
                    requested,
                    reason: self
                        .backend
                        .single_queue_reason()
                        .expect("a single-queue backend always states its reason"),
                })
            }
            Some(requested) => Ok(requested),
            None if self.backend.supports_multi_queue() => Ok(smp.max(1)),
            None => Ok(1),
        }
    }

    fn socket_vmnet_path(&self) -> &Path {
        self.socket_vmnet_path
            .as_deref()
            .unwrap_or_else(|| Path::new(DEFAULT_SOCKET_VMNET_PATH))
    }

    fn socket_vmnet_client(&self) -> &Path {
        self.socket_vmnet_client
            .as_deref()
            .unwrap_or_else(|| Path::new(DEFAULT_SOCKET_VMNET_CLIENT))
    }

    fn require_ifname(&self, purpose: &'static str) -> Result<&str, VmNetworkError> {
        let ifname = self
            .ifname
            .as_deref()
            .ok_or(VmNetworkError::MissingOption {
                backend: self.backend,
                option: "--net-ifname",
                purpose,
            })?;
        validate_interface_name(ifname)?;
        Ok(ifname)
    }

    /// Rejects every option the selected backend does not consume, so a
    /// flag that would have been ignored is reported instead.
    fn reject_unused_options(&self) -> Result<(), VmNetworkError> {
        if matches!(self.backend.interface_name(), InterfaceNameUse::Rejected)
            && self.ifname.is_some()
        {
            return Err(VmNetworkError::UnsupportedOption {
                backend: self.backend,
                option: "--net-ifname",
            });
        }
        if !self.backend.uses_bridge() && self.bridge.is_some() {
            return Err(VmNetworkError::UnsupportedOption {
                backend: self.backend,
                option: "--net-bridge",
            });
        }
        if !self.backend.uses_socket_vmnet() {
            if self.socket_vmnet_path.is_some() {
                return Err(VmNetworkError::UnsupportedOption {
                    backend: self.backend,
                    option: "--socket-vmnet-path",
                });
            }
            if self.socket_vmnet_client.is_some() {
                return Err(VmNetworkError::UnsupportedOption {
                    backend: self.backend,
                    option: "--socket-vmnet-client",
                });
            }
        }
        Ok(())
    }

    fn check_host(&self, host: HostPlatform) -> Result<(), VmNetworkError> {
        match self.backend.required_host() {
            Some(required) if required != host => Err(VmNetworkError::UnsupportedHost {
                backend: self.backend,
                required,
                actual: host,
            }),
            _ => Ok(()),
        }
    }

    /// Turns the request into the QEMU arguments that realise it.
    ///
    /// Pure: every host-state check lives in [`Self::preflight`], so the
    /// rendering of every backend is unit-testable from either host.
    pub(crate) fn render(
        &self,
        profile: VmNetworkProfile,
        ring: VirtioDeviceProfile,
        smp: u16,
        host: HostPlatform,
    ) -> Result<QemuNetArgs, VmNetworkError> {
        self.check_host(host)?;
        self.reject_unused_options()?;
        let queue_pairs = self.queue_pairs(smp)?;

        let mut netdev = QemuOptions::new(self.backend.netdev_kind());
        netdev.set("id", NET_ID);
        let mut launcher = None;
        match self.backend {
            VmNetworkBackend::User => {}
            VmNetworkBackend::Tap => {
                let InterfaceNameUse::Required(purpose) = self.backend.interface_name() else {
                    unreachable!("the tap backend always requires an interface name")
                };
                netdev.set("ifname", self.require_ifname(purpose)?);
                // The inspector never lets QEMU run host scripts: the tap
                // is provisioned once by `net-setup` and outlives the VM.
                netdev.set("script", "no");
                netdev.set("downscript", "no");
                // vhost-net moves the packet copy into host kernel
                // threads; without it a multi-queue tap still funnels
                // every queue through the QEMU main loop.
                netdev.set("vhost", "on");
                if queue_pairs > 1 {
                    netdev.set("queues", queue_pairs);
                }
            }
            VmNetworkBackend::VmnetShared => {}
            VmNetworkBackend::VmnetBridged => {
                let InterfaceNameUse::Required(purpose) = self.backend.interface_name() else {
                    unreachable!("the vmnet-bridged backend always requires an interface name")
                };
                netdev.set("ifname", self.require_ifname(purpose)?);
            }
            VmNetworkBackend::SocketVmnet => {
                netdev.set("fd", SOCKET_VMNET_FD);
                launcher = Some(QemuLauncher {
                    program: self.socket_vmnet_client().to_path_buf(),
                    args: vec![self.socket_vmnet_path().to_string_lossy().into_owned()],
                });
            }
        }

        let mut device = QemuOptions::new(profile.device_model());
        device.set("netdev", NET_ID);
        if queue_pairs > 1 {
            device.set("mq", "on");
            // One MSI-X vector per queue, plus one for the control queue
            // and one for configuration changes.
            device.set("vectors", queue_pairs * 2 + 2);
        }
        for property in &self.device_props {
            if let Some(owner) = property.spec.owner {
                return Err(VmNetworkError::DerivedDeviceProperty {
                    name: property.name(),
                    owner,
                });
            }
            if device.contains(property.name()) {
                return Err(VmNetworkError::DuplicateDeviceProperty {
                    name: property.name(),
                });
            }
            device.set(property.name(), &property.value);
        }
        match profile {
            VmNetworkProfile::VirtioMmio => ring.apply(&mut device),
            VmNetworkProfile::VirtioPci => ring.apply_pci(&mut device),
        }

        Ok(QemuNetArgs {
            launcher,
            netdev: netdev.to_string(),
            device: device.to_string(),
            backend: self.backend,
            queue_pairs,
        })
    }

    /// Checks the host state the rendered arguments depend on.
    ///
    /// Runs before the kernel build so a missing tap or an unreachable
    /// `socket_vmnet` daemon is reported in seconds rather than after a
    /// full rebuild.
    pub(crate) fn preflight(
        &self,
        qemu_bin: &Path,
        queue_pairs: u16,
    ) -> Result<(), VmNetworkError> {
        match self.backend {
            VmNetworkBackend::User => Ok(()),
            VmNetworkBackend::Tap => self.preflight_tap(queue_pairs),
            VmNetworkBackend::VmnetShared | VmNetworkBackend::VmnetBridged => {
                self.preflight_vmnet(qemu_bin)
            }
            VmNetworkBackend::SocketVmnet => {
                let endpoint = self.socket_vmnet_path();
                if !fs::metadata(endpoint).is_ok_and(|meta| meta.file_type().is_socket()) {
                    return Err(VmNetworkError::SocketVmnetEndpointMissing {
                        path: endpoint.to_path_buf(),
                    });
                }
                let client = self.socket_vmnet_client();
                resolve_program(client).ok_or_else(|| {
                    VmNetworkError::SocketVmnetClientMissing {
                        name: client.display().to_string(),
                    }
                })?;
                Ok(())
            }
        }
    }

    fn preflight_tap(&self, queue_pairs: u16) -> Result<(), VmNetworkError> {
        let InterfaceNameUse::Required(purpose) = self.backend.interface_name() else {
            unreachable!("the tap backend always requires an interface name")
        };
        let ifname = self.require_ifname(purpose)?;
        if !interface_exists(ifname) {
            return Err(VmNetworkError::TapInterfaceMissing {
                ifname: ifname.to_owned(),
            });
        }
        let flags = read_tun_flags(ifname)?;
        if queue_pairs > 1 && flags & IFF_MULTI_QUEUE == 0 {
            return Err(VmNetworkError::TapNotMultiQueue {
                ifname: ifname.to_owned(),
                flags,
                queues: queue_pairs,
            });
        }
        if let Some(bridge) = &self.bridge {
            match interface_master(ifname)? {
                Some(actual) if actual == *bridge => {}
                Some(actual) => {
                    return Err(VmNetworkError::TapBridgeMismatch {
                        ifname: ifname.to_owned(),
                        actual,
                        expected: bridge.clone(),
                    });
                }
                None => {
                    return Err(VmNetworkError::TapNotBridged {
                        ifname: ifname.to_owned(),
                        expected: bridge.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    /// vmnet needs either root or an entitled QEMU. Both are checkable, so
    /// neither is left for QEMU to discover after the kernel is built.
    fn preflight_vmnet(&self, qemu_bin: &Path) -> Result<(), VmNetworkError> {
        if is_root() {
            return Ok(());
        }
        let resolved =
            resolve_program(qemu_bin).ok_or_else(|| VmNetworkError::QemuBinaryMissing {
                name: qemu_bin.display().to_string(),
            })?;
        if has_vmnet_entitlement(&resolved)? {
            return Ok(());
        }
        Err(VmNetworkError::VmnetPrivilegeMissing {
            backend: self.backend,
        })
    }
}

/// A launcher QEMU is exec'd under, used by backends that hand QEMU a
/// pre-connected file descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QemuLauncher {
    program: PathBuf,
    args: Vec<String>,
}

/// The QEMU arguments realising a resolved backend request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QemuNetArgs {
    launcher: Option<QemuLauncher>,
    netdev: String,
    device: String,
    backend: VmNetworkBackend,
    queue_pairs: u16,
}

impl QemuNetArgs {
    /// Builds the process that will become QEMU, honouring a backend that
    /// requires a launcher.
    pub(crate) fn command(&self, qemu_bin: &Path) -> Command {
        match &self.launcher {
            Some(launcher) => {
                let mut command = Command::new(&launcher.program);
                command.args(&launcher.args);
                command.arg(qemu_bin);
                command
            }
            None => Command::new(qemu_bin),
        }
    }

    /// Appends the `-netdev`/`-device` pair to a QEMU command line.
    pub(crate) fn apply(&self, qemu: &mut Command) {
        qemu.arg("-netdev").arg(&self.netdev);
        qemu.arg("-device").arg(&self.device);
    }

    pub(crate) fn queue_pairs(&self) -> u16 {
        self.queue_pairs
    }

    #[cfg(test)]
    fn netdev(&self) -> &str {
        &self.netdev
    }

    #[cfg(test)]
    fn device(&self) -> &str {
        &self.device
    }

    #[cfg(test)]
    fn launcher(&self) -> Option<&QemuLauncher> {
        self.launcher.as_ref()
    }
}

impl fmt::Display for QemuNetArgs {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} backend, {} queue pair(s): -netdev {} -device {}",
            self.backend, self.queue_pairs, self.netdev, self.device
        )
    }
}

fn validate_interface_name(name: &str) -> Result<(), VmNetworkError> {
    if name.is_empty()
        || name.contains('/')
        || name.chars().any(char::is_whitespace)
        || name == "."
        || name == ".."
    {
        return Err(VmNetworkError::InvalidInterfaceName {
            name: name.to_owned(),
        });
    }
    if name.len() > MAX_INTERFACE_NAME_LEN {
        return Err(VmNetworkError::InterfaceNameTooLong {
            name: name.to_owned(),
            length: name.len(),
            max: MAX_INTERFACE_NAME_LEN,
        });
    }
    Ok(())
}

fn interface_path(ifname: &str) -> PathBuf {
    Path::new(SYSFS_NET_ROOT).join(ifname)
}

fn interface_exists(ifname: &str) -> bool {
    interface_path(ifname).exists()
}

/// Reads `/sys/class/net/<if>/tun_flags`, the authoritative record of the
/// flags a tap device was created with.
fn read_tun_flags(ifname: &str) -> Result<u32, VmNetworkError> {
    let path = interface_path(ifname).join("tun_flags");
    let raw = fs::read_to_string(&path).map_err(|source| VmNetworkError::SysfsUnreadable {
        path: path.clone(),
        source,
    })?;
    let trimmed = raw.trim();
    let digits = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    u32::from_str_radix(digits, 16).map_err(|_| VmNetworkError::TunFlagsUnparsable {
        path,
        raw: trimmed.to_owned(),
    })
}

/// Name of the bridge an interface is enslaved to, if any.
fn interface_master(ifname: &str) -> Result<Option<String>, VmNetworkError> {
    let path = interface_path(ifname).join("master");
    match fs::read_link(&path) {
        Ok(target) => Ok(target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(VmNetworkError::SysfsUnreadable { path, source }),
    }
}

fn is_root() -> bool {
    // SAFETY: `geteuid` reads process credentials and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// The uid/gid the tap device must be owned by for an unprivileged QEMU
/// to open it: the invoking user, even when the helper itself runs under
/// `sudo`.
fn invoking_credentials() -> (u32, u32) {
    let from_env = |name: &str| std::env::var(name).ok().and_then(|v| v.parse().ok());
    // SAFETY: `getuid`/`getgid` read process credentials and cannot fail.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    (
        from_env("SUDO_UID").unwrap_or(uid),
        from_env("SUDO_GID").unwrap_or(gid),
    )
}

/// Resolves a program name through `PATH`, or checks a given path exists.
fn resolve_program(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return program.is_file().then(|| program.to_path_buf());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

/// Whether a code-signed binary carries `com.apple.vm.networking`, the
/// entitlement that lets an unprivileged process open a vmnet interface.
fn has_vmnet_entitlement(binary: &Path) -> Result<bool, VmNetworkError> {
    let output = Command::new("codesign")
        .arg("--display")
        .arg("--entitlements")
        .arg("-")
        .arg(binary)
        .output()
        .map_err(|source| VmNetworkError::EntitlementProbeFailed {
            path: binary.to_path_buf(),
            source,
        })?;
    // codesign writes the entitlement plist to stdout on newer releases
    // and to stderr on older ones; both are searched rather than guessing
    // which release is installed.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    const ENTITLEMENT: &str = "com.apple.vm.networking";
    Ok(stdout.contains(ENTITLEMENT) || stderr.contains(ENTITLEMENT))
}

/// One privileged host command the setup helpers print before running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrivilegedCommand {
    program: String,
    args: Vec<String>,
}

impl PrivilegedCommand {
    fn new<I, S>(program: &str, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            program: program.to_owned(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    /// The command as a user could retype it, including the `sudo` prefix
    /// the helper will add.
    fn display(&self, elevate: bool) -> String {
        let mut words = Vec::with_capacity(self.args.len() + 2);
        if elevate {
            words.push("sudo".to_owned());
        }
        words.push(self.program.clone());
        words.extend(self.args.iter().cloned());
        shell_words::join(words)
    }

    fn build(&self, elevate: bool) -> Command {
        if elevate {
            let mut command = Command::new("sudo");
            command.arg(&self.program).args(&self.args);
            command
        } else {
            let mut command = Command::new(&self.program);
            command.args(&self.args);
            command
        }
    }

    /// Runs the command, failing on a non-zero exit status.
    fn run(&self, elevate: bool) -> Result<(), VmNetworkSetupError> {
        let status = self.status(elevate)?;
        if status.success() {
            Ok(())
        } else {
            Err(VmNetworkSetupError::CommandFailed {
                command: self.display(elevate),
                status,
            })
        }
    }

    /// Runs the command as a probe, returning its status instead of
    /// treating a non-zero exit as a failure.
    fn status(&self, elevate: bool) -> Result<ExitStatus, VmNetworkSetupError> {
        self.build(elevate)
            .status()
            .map_err(|source| VmNetworkSetupError::Spawn {
                command: self.display(elevate),
                source,
            })
    }

    /// Runs the command and captures stdout, for probes whose output the
    /// helper parses.
    fn output(&self, elevate: bool) -> Result<Vec<u8>, VmNetworkSetupError> {
        let output = self
            .build(elevate)
            .output()
            .map_err(|source| VmNetworkSetupError::Spawn {
                command: self.display(elevate),
                source,
            })?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(VmNetworkSetupError::CommandFailed {
                command: self.display(elevate),
                status: output.status,
            })
        }
    }
}

/// Options of the privileged `net-setup` helper.
#[derive(Debug, Clone, ClapArgs)]
pub(crate) struct NetSetupCommand {
    #[command(flatten)]
    network: VmNetworkArgs,

    /// IPv4 address and prefix assigned to the bridge, and therefore the
    /// address the guest reaches the host on.
    #[arg(long = "net-bridge-address", default_value = "10.77.0.1/24")]
    bridge_address: Ipv4Net,

    /// Host interface masqueraded traffic leaves through. Defaults to the
    /// interface owning the host default route.
    #[arg(long = "net-uplink")]
    uplink: Option<String>,

    /// Skip the nftables masquerade rule and the IPv4 forwarding sysctl.
    #[arg(long = "net-no-nat", default_value_t = false)]
    no_nat: bool,

    /// Serve DHCP on the bridge with dnsmasq, so the guest's own DHCP
    /// client can lease an address the way it does under slirp.
    #[arg(long = "net-dhcp", default_value_t = false)]
    dhcp: bool,

    /// Print the privileged commands without running any of them.
    #[arg(long = "dry-run", default_value_t = false)]
    dry_run: bool,
}

/// Options of the privileged `net-teardown` helper.
#[derive(Debug, Clone, ClapArgs)]
pub(crate) struct NetTeardownCommand {
    #[command(flatten)]
    network: VmNetworkArgs,

    /// Print the privileged commands without running any of them.
    #[arg(long = "dry-run", default_value_t = false)]
    dry_run: bool,
}

/// Path of the pid file the optional dnsmasq responder writes.
fn dnsmasq_pid_file(bridge: &str) -> PathBuf {
    PathBuf::from("/var/run").join(format!("helios-dnsmasq-{bridge}.pid"))
}

/// The DHCP pool carved out of the bridge subnet: every host address
/// above the bridge's own, capped at [`DHCP_POOL_SIZE`].
fn dhcp_range(address: Ipv4Net) -> Result<(Ipv4Addr, Ipv4Addr), VmNetworkSetupError> {
    let mut hosts = address.hosts().filter(|host| *host > address.addr());
    let start = hosts
        .next()
        .ok_or(VmNetworkSetupError::BridgeRangeEmpty { address })?;
    let end = hosts.take(DHCP_POOL_SIZE - 1).last().unwrap_or(start);
    Ok((start, end))
}

/// The interface owning the host's default route.
#[derive(Debug, Deserialize)]
struct DefaultRoute {
    dev: String,
}

fn default_uplink() -> Result<String, VmNetworkSetupError> {
    let probe = PrivilegedCommand::new("ip", ["-json", "route", "show", "default"]);
    let stdout = probe.output(false)?;
    let routes: Vec<DefaultRoute> = serde_json::from_slice(&stdout)
        .map_err(|source| VmNetworkSetupError::RouteDecode { source })?;
    routes
        .into_iter()
        .map(|route| route.dev)
        .next()
        .ok_or(VmNetworkSetupError::UplinkUnknown)
}

/// The `tap` backend is the only one with host state to provision: vmnet
/// is opened by QEMU itself, and `socket_vmnet` is a launchd daemon whose
/// installation is outside this repository.
fn require_tap_backend(network: &VmNetwork) -> Result<(), VmNetworkSetupError> {
    if network.backend != VmNetworkBackend::Tap {
        return Err(VmNetworkSetupError::UnsupportedBackend {
            backend: network.backend,
        });
    }
    let host = HostPlatform::current();
    if host != HostPlatform::LINUX {
        return Err(VmNetworkSetupError::UnsupportedHost {
            required: HostPlatform::LINUX,
            actual: host,
        });
    }
    Ok(())
}

fn tap_names(network: &VmNetwork) -> Result<(String, String), VmNetworkSetupError> {
    let InterfaceNameUse::Required(purpose) = network.backend.interface_name() else {
        unreachable!("the tap backend always requires an interface name")
    };
    let ifname = network.require_ifname(purpose)?.to_owned();
    let bridge = network
        .bridge
        .clone()
        .ok_or(VmNetworkError::MissingOption {
            backend: VmNetworkBackend::Tap,
            option: "--net-bridge",
            purpose: "the host bridge the tap is enslaved to",
        })?;
    validate_interface_name(&bridge)?;
    Ok((ifname, bridge))
}

/// Builds the privileged plan that provisions a multi-queue tap.
fn tap_setup_plan(
    command: &NetSetupCommand,
    ifname: &str,
    bridge: &str,
) -> Result<Vec<PrivilegedCommand>, VmNetworkSetupError> {
    let (uid, gid) = invoking_credentials();
    let mut plan = Vec::new();
    if !interface_exists(bridge) {
        plan.push(PrivilegedCommand::new(
            "ip",
            ["link", "add", bridge, "type", "bridge"],
        ));
    }
    plan.push(PrivilegedCommand::new("ip", ["link", "set", bridge, "up"]));
    plan.push(PrivilegedCommand::new(
        "ip",
        [
            "addr",
            "replace",
            &command.bridge_address.to_string(),
            "dev",
            bridge,
        ],
    ));
    if !interface_exists(ifname) {
        plan.push(PrivilegedCommand::new(
            "ip",
            [
                "tuntap",
                "add",
                "dev",
                ifname,
                "mode",
                "tap",
                "multi_queue",
                "user",
                &uid.to_string(),
                "group",
                &gid.to_string(),
            ],
        ));
    }
    plan.push(PrivilegedCommand::new(
        "ip",
        ["link", "set", ifname, "master", bridge],
    ));
    plan.push(PrivilegedCommand::new("ip", ["link", "set", ifname, "up"]));
    if !command.no_nat {
        let uplink = match &command.uplink {
            Some(uplink) => uplink.clone(),
            None => default_uplink()?,
        };
        validate_interface_name(&uplink)?;
        plan.push(PrivilegedCommand::new(
            "sysctl",
            ["-w", "net.ipv4.ip_forward=1"],
        ));
        plan.push(PrivilegedCommand::new(
            "nft",
            ["add", "table", "inet", NAT_TABLE],
        ));
        plan.push(PrivilegedCommand::new(
            "nft",
            [
                "add",
                "chain",
                "inet",
                NAT_TABLE,
                "postrouting",
                "{ type nat hook postrouting priority srcnat ; }",
            ],
        ));
        plan.push(PrivilegedCommand::new(
            "nft",
            [
                "add",
                "rule",
                "inet",
                NAT_TABLE,
                "postrouting",
                "ip",
                "saddr",
                &command.bridge_address.trunc().to_string(),
                "oifname",
                &uplink,
                "masquerade",
            ],
        ));
    }
    if command.dhcp {
        let (start, end) = dhcp_range(command.bridge_address)?;
        let netmask = Ipv4Addr::from(u32::MAX << (32 - command.bridge_address.prefix_len()));
        plan.push(PrivilegedCommand::new(
            "dnsmasq",
            [
                "--conf-file=/dev/null".to_owned(),
                "--no-hosts".to_owned(),
                "--bind-interfaces".to_owned(),
                "--except-interface=lo".to_owned(),
                format!("--interface={bridge}"),
                "--dhcp-authoritative".to_owned(),
                format!("--dhcp-range={start},{end},{netmask},{DHCP_LEASE_TIME}"),
                format!("--pid-file={}", dnsmasq_pid_file(bridge).display()),
            ],
        ));
    }
    Ok(plan)
}

fn tap_teardown_plan(
    ifname: &str,
    bridge: &str,
    elevate: bool,
) -> Result<Vec<PrivilegedCommand>, VmNetworkSetupError> {
    let mut plan = Vec::new();
    let pid_file = dnsmasq_pid_file(bridge);
    if pid_file.exists() {
        let raw = fs::read_to_string(&pid_file).map_err(|source| {
            VmNetworkSetupError::PidFileUnreadable {
                path: pid_file.clone(),
                source,
            }
        })?;
        let pid =
            raw.trim()
                .parse::<u32>()
                .map_err(|_| VmNetworkSetupError::PidFileUnparsable {
                    path: pid_file.clone(),
                    raw: raw.trim().to_owned(),
                })?;
        plan.push(PrivilegedCommand::new("kill", [pid.to_string()]));
        plan.push(PrivilegedCommand::new(
            "rm",
            ["-f".to_owned(), pid_file.display().to_string()],
        ));
    }
    let nat_probe = PrivilegedCommand::new("nft", ["list", "table", "inet", NAT_TABLE]);
    if nat_probe.status(elevate)?.success() {
        plan.push(PrivilegedCommand::new(
            "nft",
            ["delete", "table", "inet", NAT_TABLE],
        ));
    }
    if interface_exists(ifname) {
        plan.push(PrivilegedCommand::new("ip", ["link", "del", ifname]));
    }
    if interface_exists(bridge) {
        plan.push(PrivilegedCommand::new("ip", ["link", "del", bridge]));
    }
    Ok(plan)
}

fn execute_plan(
    plan: &[PrivilegedCommand],
    elevate: bool,
    dry_run: bool,
) -> Result<(), VmNetworkSetupError> {
    for step in plan {
        println!("{} {}", style("+").cyan(), step.display(elevate));
        if !dry_run {
            step.run(elevate)?;
        }
    }
    Ok(())
}

/// Provisions the host state a backend needs.
pub(crate) fn run_setup(command: NetSetupCommand) -> Result<(), VmNetworkSetupError> {
    let network = VmNetwork::resolve(command.network.clone(), VmNetworkFile::default());
    require_tap_backend(&network)?;
    let (ifname, bridge) = tap_names(&network)?;
    if interface_exists(&ifname) && read_tun_flags(&ifname)? & IFF_MULTI_QUEUE == 0 {
        return Err(VmNetworkSetupError::ExistingTapNotMultiQueue { ifname });
    }
    let elevate = !is_root();
    let plan = tap_setup_plan(&command, &ifname, &bridge)?;
    execute_plan(&plan, elevate, command.dry_run)?;
    if command.dry_run {
        return Ok(());
    }

    // The whole point of the tap backend is multi-queue; a tap that came
    // up without IFF_MULTI_QUEUE would silently cap the guest at one
    // queue pair, so the helper proves the flag rather than assuming it.
    let flags = read_tun_flags(&ifname)?;
    if flags & IFF_MULTI_QUEUE == 0 {
        return Err(VmNetworkSetupError::ExistingTapNotMultiQueue { ifname });
    }
    println!(
        "{} tap {ifname} on bridge {bridge} is multi-queue (tun_flags {flags:#06x}); \
         guests reach the host at {}",
        style("ready").green(),
        command.bridge_address.addr(),
    );
    Ok(())
}

/// Removes the host state `net-setup` provisioned.
pub(crate) fn run_teardown(command: NetTeardownCommand) -> Result<(), VmNetworkSetupError> {
    let network = VmNetwork::resolve(command.network.clone(), VmNetworkFile::default());
    require_tap_backend(&network)?;
    let (ifname, bridge) = tap_names(&network)?;
    let elevate = !is_root();
    let plan = tap_teardown_plan(&ifname, &bridge, elevate)?;
    execute_plan(&plan, elevate, command.dry_run)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr as _;

    use super::*;
    use crate::vm::{VirtioCompletionOrder, VirtioPlatformAccess, VirtioRingLayout};

    fn request(backend: VmNetworkBackend) -> VmNetwork {
        VmNetwork {
            backend,
            queues: None,
            ifname: None,
            bridge: None,
            socket_vmnet_path: None,
            socket_vmnet_client: None,
            device_props: Vec::new(),
        }
    }

    fn split_ring() -> VirtioDeviceProfile {
        VirtioDeviceProfile::default()
    }

    #[test]
    fn user_backend_renders_a_single_queue_slirp_path() {
        let rendered = request(VmNetworkBackend::User)
            .render(
                VmNetworkProfile::VirtioPci,
                split_ring(),
                4,
                HostPlatform::LINUX,
            )
            .expect("the user backend renders on every host");
        assert_eq!(rendered.netdev(), "user,id=net0");
        assert_eq!(rendered.device(), "virtio-net-pci,netdev=net0");
        assert_eq!(rendered.queue_pairs(), 1);
        assert!(rendered.launcher().is_none());
    }

    #[test]
    fn user_backend_rejects_multi_queue_instead_of_downgrading() {
        let mut network = request(VmNetworkBackend::User);
        network.queues = Some(4);
        let error = network
            .render(
                VmNetworkProfile::VirtioPci,
                split_ring(),
                4,
                HostPlatform::LINUX,
            )
            .expect_err("slirp cannot serve four queue pairs");
        assert!(matches!(
            error,
            VmNetworkError::UnsupportedQueueCount {
                backend: VmNetworkBackend::User,
                requested: 4,
                ..
            }
        ));
    }

    #[test]
    fn tap_backend_defaults_its_queue_count_to_smp() {
        let mut network = request(VmNetworkBackend::Tap);
        network.ifname = Some("helios0".to_owned());
        let rendered = network
            .render(
                VmNetworkProfile::VirtioMmio,
                split_ring(),
                8,
                HostPlatform::LINUX,
            )
            .expect("a named tap renders on Linux");
        assert_eq!(
            rendered.netdev(),
            "tap,id=net0,ifname=helios0,script=no,downscript=no,vhost=on,queues=8"
        );
        assert_eq!(
            rendered.device(),
            "virtio-net-device,netdev=net0,mq=on,vectors=18"
        );
        assert_eq!(rendered.queue_pairs(), 8);
    }

    #[test]
    fn tap_backend_with_one_queue_omits_the_multiqueue_options() {
        let mut network = request(VmNetworkBackend::Tap);
        network.ifname = Some("helios0".to_owned());
        network.queues = Some(1);
        let rendered = network
            .render(
                VmNetworkProfile::VirtioPci,
                split_ring(),
                4,
                HostPlatform::LINUX,
            )
            .expect("a single-queue tap renders");
        assert_eq!(
            rendered.netdev(),
            "tap,id=net0,ifname=helios0,script=no,downscript=no,vhost=on"
        );
        assert_eq!(rendered.device(), "virtio-net-pci,netdev=net0");
    }

    #[test]
    fn tap_backend_requires_an_interface_name() {
        let error = request(VmNetworkBackend::Tap)
            .render(
                VmNetworkProfile::VirtioPci,
                split_ring(),
                4,
                HostPlatform::LINUX,
            )
            .expect_err("a tap without a name cannot be attached");
        assert!(matches!(
            error,
            VmNetworkError::MissingOption {
                backend: VmNetworkBackend::Tap,
                option: "--net-ifname",
                ..
            }
        ));
    }

    #[test]
    fn tap_backend_rejects_an_over_long_interface_name() {
        let mut network = request(VmNetworkBackend::Tap);
        network.ifname = Some("helios-interface-0".to_owned());
        let error = network
            .render(
                VmNetworkProfile::VirtioPci,
                split_ring(),
                4,
                HostPlatform::LINUX,
            )
            .expect_err("IFNAMSIZ bounds the interface name");
        assert!(matches!(
            error,
            VmNetworkError::InterfaceNameTooLong { max: 15, .. }
        ));
    }

    #[test]
    fn tap_backend_is_rejected_on_macos() {
        let mut network = request(VmNetworkBackend::Tap);
        network.ifname = Some("helios0".to_owned());
        let error = network
            .render(
                VmNetworkProfile::VirtioPci,
                split_ring(),
                4,
                HostPlatform::MACOS,
            )
            .expect_err("there is no tap netdev on macOS");
        assert!(matches!(
            error,
            VmNetworkError::UnsupportedHost {
                backend: VmNetworkBackend::Tap,
                ..
            }
        ));
    }

    #[test]
    fn vmnet_shared_renders_without_an_interface_name() {
        let rendered = request(VmNetworkBackend::VmnetShared)
            .render(
                VmNetworkProfile::VirtioPci,
                split_ring(),
                4,
                HostPlatform::MACOS,
            )
            .expect("vmnet-shared renders on macOS");
        assert_eq!(rendered.netdev(), "vmnet-shared,id=net0");
        assert_eq!(rendered.device(), "virtio-net-pci,netdev=net0");
        assert_eq!(rendered.queue_pairs(), 1);
    }

    #[test]
    fn vmnet_shared_rejects_an_interface_name() {
        let mut network = request(VmNetworkBackend::VmnetShared);
        network.ifname = Some("en0".to_owned());
        let error = network
            .render(
                VmNetworkProfile::VirtioPci,
                split_ring(),
                4,
                HostPlatform::MACOS,
            )
            .expect_err("vmnet-shared has no host interface to name");
        assert!(matches!(
            error,
            VmNetworkError::UnsupportedOption {
                backend: VmNetworkBackend::VmnetShared,
                option: "--net-ifname",
            }
        ));
    }

    #[test]
    fn vmnet_bridged_names_the_host_interface() {
        let mut network = request(VmNetworkBackend::VmnetBridged);
        network.ifname = Some("en0".to_owned());
        let rendered = network
            .render(
                VmNetworkProfile::VirtioPci,
                split_ring(),
                4,
                HostPlatform::MACOS,
            )
            .expect("vmnet-bridged renders on macOS");
        assert_eq!(rendered.netdev(), "vmnet-bridged,id=net0,ifname=en0");
    }

    #[test]
    fn vmnet_backends_are_rejected_on_linux() {
        for backend in [
            VmNetworkBackend::VmnetShared,
            VmNetworkBackend::VmnetBridged,
            VmNetworkBackend::SocketVmnet,
        ] {
            let mut network = request(backend);
            if backend == VmNetworkBackend::VmnetBridged {
                network.ifname = Some("en0".to_owned());
            }
            let error = network
                .render(
                    VmNetworkProfile::VirtioPci,
                    split_ring(),
                    4,
                    HostPlatform::LINUX,
                )
                .expect_err("vmnet only exists on macOS");
            assert!(matches!(error, VmNetworkError::UnsupportedHost { .. }));
        }
    }

    #[test]
    fn socket_vmnet_execs_qemu_under_its_client() {
        let rendered = request(VmNetworkBackend::SocketVmnet)
            .render(
                VmNetworkProfile::VirtioPci,
                split_ring(),
                4,
                HostPlatform::MACOS,
            )
            .expect("socket-vmnet renders on macOS");
        assert_eq!(rendered.netdev(), "socket,id=net0,fd=3");
        let launcher = rendered
            .launcher()
            .expect("socket-vmnet execs QEMU under its client");
        assert_eq!(launcher.program, Path::new(DEFAULT_SOCKET_VMNET_CLIENT));
        assert_eq!(launcher.args, vec![DEFAULT_SOCKET_VMNET_PATH.to_owned()]);
    }

    #[test]
    fn socket_vmnet_options_are_rejected_on_other_backends() {
        let mut network = request(VmNetworkBackend::User);
        network.socket_vmnet_path = Some(PathBuf::from("/tmp/socket_vmnet"));
        let error = network
            .render(
                VmNetworkProfile::VirtioPci,
                split_ring(),
                4,
                HostPlatform::LINUX,
            )
            .expect_err("slirp has no socket_vmnet endpoint");
        assert!(matches!(
            error,
            VmNetworkError::UnsupportedOption {
                option: "--socket-vmnet-path",
                ..
            }
        ));
    }

    #[test]
    fn bridge_is_rejected_outside_the_tap_backend() {
        let mut network = request(VmNetworkBackend::VmnetShared);
        network.bridge = Some("helios-br0".to_owned());
        let error = network
            .render(
                VmNetworkProfile::VirtioPci,
                split_ring(),
                4,
                HostPlatform::MACOS,
            )
            .expect_err("only the tap backend is enslaved to a bridge");
        assert!(matches!(
            error,
            VmNetworkError::UnsupportedOption {
                option: "--net-bridge",
                ..
            }
        ));
    }

    #[test]
    fn zero_queues_is_rejected() {
        let mut network = request(VmNetworkBackend::Tap);
        network.ifname = Some("helios0".to_owned());
        network.queues = Some(0);
        let error = network
            .render(
                VmNetworkProfile::VirtioPci,
                split_ring(),
                4,
                HostPlatform::LINUX,
            )
            .expect_err("a device always has at least one queue pair");
        assert!(matches!(error, VmNetworkError::ZeroQueues));
    }

    #[test]
    fn ring_layout_properties_reach_the_network_device() {
        let ring = VirtioDeviceProfile {
            ring: VirtioRingLayout::Packed,
            completion: VirtioCompletionOrder::InOrder,
            access: VirtioPlatformAccess::Direct,
        };
        let rendered = request(VmNetworkBackend::User)
            .render(VmNetworkProfile::VirtioPci, ring, 1, HostPlatform::LINUX)
            .expect("the user backend renders with any ring layout");
        assert_eq!(
            rendered.device(),
            "virtio-net-pci,netdev=net0,packed=on,in_order=on"
        );
    }

    /// A confined device has to be told to translate, and only a
    /// non-transitional function offers the feature that says so.
    #[test]
    fn a_confined_network_device_negotiates_platform_access() {
        let devices = VirtioDeviceProfile {
            ring: VirtioRingLayout::Split,
            completion: VirtioCompletionOrder::Unordered,
            access: VirtioPlatformAccess::Confined,
        };
        let rendered = request(VmNetworkBackend::User)
            .render(VmNetworkProfile::VirtioPci, devices, 1, HostPlatform::LINUX)
            .expect("the user backend renders behind an IOMMU");
        assert_eq!(
            rendered.device(),
            "virtio-net-pci,netdev=net0,disable-legacy=on,iommu_platform=on"
        );
    }

    #[test]
    fn device_properties_are_appended_in_order() {
        let mut network = request(VmNetworkBackend::User);
        network.device_props = vec![
            "csum=off".parse().expect("csum is a known property"),
            "host_tso4=on"
                .parse()
                .expect("host_tso4 is a known property"),
        ];
        let rendered = network
            .render(
                VmNetworkProfile::VirtioPci,
                split_ring(),
                1,
                HostPlatform::LINUX,
            )
            .expect("known properties render");
        assert_eq!(
            rendered.device(),
            "virtio-net-pci,netdev=net0,csum=off,host_tso4=on"
        );
    }

    #[test]
    fn unknown_device_properties_are_rejected_rather_than_forwarded() {
        let error = VirtioNetProperty::from_str("gso=on")
            .expect_err("`gso` is not a virtio-net device property");
        assert!(matches!(
            error,
            VmNetworkError::UnknownDeviceProperty { .. }
        ));
    }

    #[test]
    fn device_properties_must_be_key_value_pairs() {
        let error = VirtioNetProperty::from_str("csum").expect_err("a bare key is not a property");
        assert!(matches!(
            error,
            VmNetworkError::MalformedDeviceProperty { .. }
        ));
    }

    #[test]
    fn toggle_properties_reject_non_toggle_values() {
        let error = VirtioNetProperty::from_str("csum=1")
            .expect_err("QEMU toggles are spelled `on` and `off`");
        assert!(matches!(
            error,
            VmNetworkError::NonToggleDeviceProperty { name: "csum", .. }
        ));
    }

    #[test]
    fn count_properties_reject_non_numeric_values() {
        let error = VirtioNetProperty::from_str("vectors=many")
            .expect_err("`vectors` counts MSI-X vectors");
        assert!(matches!(
            error,
            VmNetworkError::NonCountDeviceProperty {
                name: "vectors",
                ..
            }
        ));
    }

    #[test]
    fn derived_properties_name_the_flag_that_owns_them() {
        for (raw, owner) in [
            ("mq=on", "--net-queues"),
            ("vectors=8", "--net-queues"),
            ("packed=on", "--virtio-packed"),
        ] {
            let mut network = request(VmNetworkBackend::User);
            network.device_props = vec![raw.parse().expect("the property name is known")];
            let error = network
                .render(
                    VmNetworkProfile::VirtioPci,
                    split_ring(),
                    1,
                    HostPlatform::LINUX,
                )
                .expect_err("a derived property has exactly one source");
            match error {
                VmNetworkError::DerivedDeviceProperty { owner: actual, .. } => {
                    assert_eq!(actual, owner)
                }
                other => panic!("expected a derived-property error, got {other}"),
            }
        }
    }

    #[test]
    fn a_repeated_device_property_is_rejected() {
        let mut network = request(VmNetworkBackend::User);
        network.device_props = vec![
            "csum=on".parse().expect("csum is a known property"),
            "csum=off".parse().expect("csum is a known property"),
        ];
        let error = network
            .render(
                VmNetworkProfile::VirtioPci,
                split_ring(),
                1,
                HostPlatform::LINUX,
            )
            .expect_err("a property set twice has no defined value");
        assert!(matches!(
            error,
            VmNetworkError::DuplicateDeviceProperty { name: "csum" }
        ));
    }

    #[test]
    fn device_properties_round_trip_through_the_config_file() {
        let property: VirtioNetProperty =
            serde_json::from_str("\"host_tso6=on\"").expect("a known property decodes");
        assert_eq!(property.name(), "host_tso6");
        let encoded = serde_json::to_string(&property).expect("a property encodes");
        assert_eq!(encoded, "\"host_tso6=on\"");
    }

    #[test]
    fn command_line_arguments_win_over_the_config_file() {
        let args = VmNetworkArgs {
            backend: Some(VmNetworkBackend::Tap),
            queues: None,
            ifname: Some("helios0".to_owned()),
            bridge: None,
            socket_vmnet_path: None,
            socket_vmnet_client: None,
            device_props: Vec::new(),
        };
        let file = VmNetworkFile {
            backend: Some(VmNetworkBackend::User),
            queues: Some(2),
            ifname: Some("helios1".to_owned()),
            ..VmNetworkFile::default()
        };
        let network = VmNetwork::resolve(args, file);
        assert_eq!(network.backend, VmNetworkBackend::Tap);
        assert_eq!(network.ifname.as_deref(), Some("helios0"));
        assert_eq!(network.queues, Some(2));
    }

    #[test]
    fn the_dhcp_pool_sits_above_the_bridge_address() {
        let address: Ipv4Net = "10.77.0.1/24".parse().expect("a valid CIDR");
        let (start, end) = dhcp_range(address).expect("a /24 has a usable pool");
        assert_eq!(start, Ipv4Addr::new(10, 77, 0, 2));
        assert_eq!(end, Ipv4Addr::new(10, 77, 0, 129));
    }

    #[test]
    fn the_setup_plan_creates_a_multi_queue_tap_on_a_bridge() {
        let command = NetSetupCommand {
            network: VmNetworkArgs {
                backend: Some(VmNetworkBackend::Tap),
                queues: None,
                ifname: Some("helios0".to_owned()),
                bridge: Some("helios-br0".to_owned()),
                socket_vmnet_path: None,
                socket_vmnet_client: None,
                device_props: Vec::new(),
            },
            bridge_address: "10.77.0.1/24".parse().expect("a valid CIDR"),
            uplink: Some("eth0".to_owned()),
            no_nat: false,
            dhcp: true,
            dry_run: true,
        };
        let plan = tap_setup_plan(&command, "helios0", "helios-br0")
            .expect("a fully specified tap plan builds");
        let rendered: Vec<String> = plan.iter().map(|step| step.display(true)).collect();
        assert!(rendered.iter().any(|step| {
            step.starts_with("sudo ip tuntap add dev helios0 mode tap multi_queue user ")
        }));
        assert!(rendered.contains(&"sudo ip link set helios0 master helios-br0".to_owned()));
        // `shell_words::join` quotes the `key=value` words so every
        // printed step is copy-pasteable into a shell verbatim.
        assert!(rendered.contains(&"sudo sysctl -w 'net.ipv4.ip_forward=1'".to_owned()));
        assert!(rendered.iter().any(|step| step.contains("masquerade")));
        assert!(
            rendered
                .iter()
                .any(|step| step.contains("--dhcp-range=10.77.0.2,10.77.0.129,255.255.255.0,12h"))
        );
    }

    #[test]
    fn the_setup_helper_only_provisions_the_tap_backend() {
        for backend in [
            VmNetworkBackend::User,
            VmNetworkBackend::VmnetShared,
            VmNetworkBackend::SocketVmnet,
        ] {
            let error = require_tap_backend(&request(backend))
                .expect_err("only tap has host state to provision");
            assert!(matches!(
                error,
                VmNetworkSetupError::UnsupportedBackend { .. }
            ));
        }
    }
}
