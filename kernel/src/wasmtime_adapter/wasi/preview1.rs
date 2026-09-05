//! WASI Preview1 adapter-local ABI manifest.
//!
//! Preview1 names are kept here so core authority remains capability typed
//! and does not learn syscall spellings.

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Preview1Authority {
    Args,
    Environment,
    ClockRead,
    DescriptorRead,
    DescriptorWrite,
    DescriptorMetadata,
    DescriptorMutate,
    PathRead,
    PathMutate,
    Link,
    Poll,
    ProcessExit,
    ProcessSignal,
    Random,
    Socket,
}

pub(crate) const PREVIEW1_FUNCTIONS: &[&str] = &[
    "args_get",
    "args_sizes_get",
    "environ_get",
    "environ_sizes_get",
    "clock_res_get",
    "clock_time_get",
    "fd_advise",
    "fd_allocate",
    "fd_close",
    "fd_datasync",
    "fd_fdstat_get",
    "fd_fdstat_set_flags",
    "fd_fdstat_set_rights",
    "fd_filestat_get",
    "fd_filestat_set_size",
    "fd_filestat_set_times",
    "fd_pread",
    "fd_prestat_get",
    "fd_prestat_dir_name",
    "fd_pwrite",
    "fd_read",
    "fd_readdir",
    "fd_renumber",
    "fd_seek",
    "fd_sync",
    "fd_tell",
    "fd_write",
    "path_create_directory",
    "path_filestat_get",
    "path_filestat_set_times",
    "path_link",
    "path_open",
    "path_readlink",
    "path_remove_directory",
    "path_rename",
    "path_symlink",
    "path_unlink_file",
    "poll_oneoff",
    "proc_exit",
    "proc_raise",
    "sched_yield",
    "random_get",
    "sock_accept",
    "sock_recv",
    "sock_send",
    "sock_shutdown",
];

pub(crate) mod errno {
    pub(crate) const SUCCESS: i32 = 0;
    pub(crate) const ACCES: i32 = 2;
    /// `__WASI_ERRNO_AFNOSUPPORT`: the address family is not supported
    /// by this socket, which is what an `AF_INET` socket must report for
    /// an `AF_INET6` peer address and vice versa.
    pub(crate) const AFNOSUPPORT: i32 = 5;
    pub(crate) const AGAIN: i32 = 6;
    pub(crate) const BADF: i32 = 8;
    /// `__WASI_ERRNO_CONNABORTED`.
    pub(crate) const CONNABORTED: i32 = 13;
    /// `__WASI_ERRNO_CONNRESET`.
    pub(crate) const CONNRESET: i32 = 15;
    pub(crate) const EXIST: i32 = 20;
    pub(crate) const FAULT: i32 = 21;
    pub(crate) const INVAL: i32 = 28;
    pub(crate) const IO: i32 = 29;
    pub(crate) const ISDIR: i32 = 31;
    pub(crate) const LOOP: i32 = 32;
    pub(crate) const NETDOWN: i32 = 38;
    pub(crate) const HOSTUNREACH: i32 = 23;
    pub(crate) const NOMEM: i32 = 48;
    pub(crate) const NOENT: i32 = 44;
    pub(crate) const NOTDIR: i32 = 54;
    pub(crate) const NOTEMPTY: i32 = 55;
    pub(crate) const NOTSOCK: i32 = 57;
    pub(crate) const NOTSUP: i32 = 58;
    pub(crate) const OVERFLOW: i32 = 61;
    pub(crate) const PERM: i32 = 63;
    pub(crate) const RANGE: i32 = 68;
    pub(crate) const ROFS: i32 = 69;
    pub(crate) const SPIPE: i32 = 70;
    pub(crate) const SRCH: i32 = 71;
    pub(crate) const TIMEDOUT: i32 = 73;
    pub(crate) const XDEV: i32 = 75;
    pub(crate) const NOTCAPABLE: i32 = 76;
}

#[cfg(test)]
pub(crate) fn authority_for(function: &str) -> Option<Preview1Authority> {
    let authority = match function {
        "args_get" | "args_sizes_get" => Preview1Authority::Args,
        "environ_get" | "environ_sizes_get" => Preview1Authority::Environment,
        "clock_res_get" | "clock_time_get" | "sched_yield" => Preview1Authority::ClockRead,
        "fd_close" | "fd_fdstat_set_flags" | "fd_fdstat_set_rights" | "fd_renumber" => {
            Preview1Authority::DescriptorMutate
        }
        "fd_pread" | "fd_read" | "fd_readdir" => Preview1Authority::DescriptorRead,
        "fd_pwrite" | "fd_write" => Preview1Authority::DescriptorWrite,
        "fd_advise"
        | "fd_allocate"
        | "fd_datasync"
        | "fd_filestat_set_size"
        | "fd_filestat_set_times"
        | "fd_seek"
        | "fd_sync" => Preview1Authority::DescriptorMutate,
        "fd_fdstat_get"
        | "fd_filestat_get"
        | "fd_prestat_get"
        | "fd_prestat_dir_name"
        | "fd_tell" => Preview1Authority::DescriptorMetadata,
        "path_filestat_get" | "path_readlink" => Preview1Authority::PathRead,
        "path_create_directory"
        | "path_filestat_set_times"
        | "path_open"
        | "path_remove_directory"
        | "path_rename"
        | "path_symlink"
        | "path_unlink_file" => Preview1Authority::PathMutate,
        "path_link" => Preview1Authority::Link,
        "poll_oneoff" => Preview1Authority::Poll,
        "proc_exit" => Preview1Authority::ProcessExit,
        "proc_raise" => Preview1Authority::ProcessSignal,
        "random_get" => Preview1Authority::Random,
        "sock_accept" | "sock_recv" | "sock_send" | "sock_shutdown" => Preview1Authority::Socket,
        _ => return None,
    };
    Some(authority)
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeSet;

    use super::{PREVIEW1_FUNCTIONS, Preview1Authority, authority_for};

    const PREVIEW1_WITX: &str =
        include_str!("../../../../../wasmtime/crates/wasi/witx/p1/wasi_snapshot_preview1.witx");

    #[test]
    fn preview1_manifest_matches_witx_exports() {
        let manifest = PREVIEW1_FUNCTIONS.iter().copied().collect::<BTreeSet<_>>();
        let witx = witx_exports(PREVIEW1_WITX);
        assert_eq!(manifest, witx);
    }

    #[test]
    fn preview1_manifest_has_authority_mapping() {
        for function in PREVIEW1_FUNCTIONS {
            assert!(
                authority_for(function).is_some(),
                "Preview1 function {function} has no adapter authority mapping"
            );
        }
    }

    #[test]
    fn sensitive_preview1_functions_map_to_specific_authority() {
        assert_eq!(authority_for("path_link"), Some(Preview1Authority::Link));
        assert_eq!(
            authority_for("path_symlink"),
            Some(Preview1Authority::PathMutate)
        );
        assert_eq!(
            authority_for("proc_raise"),
            Some(Preview1Authority::ProcessSignal)
        );
        assert_eq!(authority_for("random_get"), Some(Preview1Authority::Random));
        assert_eq!(authority_for("sock_send"), Some(Preview1Authority::Socket));
    }

    fn witx_exports(witx: &'static str) -> BTreeSet<&'static str> {
        witx.lines()
            .filter_map(|line| {
                line.split_once("(export \"")
                    .and_then(|(_, rest)| rest.split_once('"'))
                    .map(|(name, _)| name)
            })
            .collect()
    }
}
