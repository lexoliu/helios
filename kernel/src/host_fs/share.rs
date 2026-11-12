/// Virtio 9p mount tag used by backends to identify the host share device.
pub const HOST_SHARE_MOUNT_TAG: &str = helios_hal::fs::HOST_SHARE_MOUNT_TAG;

/// Where the kernel mounts the host share in every namespace.
pub const HOST_SHARE_GUEST_MOUNT_PATH: &str = "/host";
