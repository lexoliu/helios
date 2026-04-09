pub const HOST_SHARE_GUEST_MOUNT_PATH: &str = "/host";

pub fn guest_host_share_path(path: &str) -> Option<&str> {
    if path == HOST_SHARE_GUEST_MOUNT_PATH {
        return Some("/");
    }

    path.strip_prefix("/host/")
        .map(|suffix| if suffix.is_empty() { "/" } else { suffix })
}
