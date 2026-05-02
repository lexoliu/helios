//! WASIX adapter-local syscall manifest and capability mapping.
//!
//! WASIX names stay in this adapter module. Core kernel authority is expressed
//! by `ProcessAuthority` rights; this table documents which authority class an
//! eventual WASIX lowering must consume before dispatching to kernel services.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WasixAuthority {
    Args,
    Environment,
    ClockRead,
    DescriptorRead,
    DescriptorWrite,
    DescriptorMetadata,
    DescriptorMutate,
    PathMetadata,
    ProcessExit,
    DescriptorTable,
    TerminalControl,
    Thread,
    Futex,
    StackCheckpoint,
    ProcessQuery,
    ProcessJoin,
    ProcessSignal,
    Fork,
    Exec,
    Spawn,
    Poll,
    SetWallClock,
    NetworkAdmin,
    Socket,
    Dns,
    Multicast,
    PrivilegedBindIfLowPort,
}

pub(crate) fn manifest() -> impl Iterator<Item = &'static str> {
    include_str!("wasix_syscalls.txt")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
}

pub(crate) fn authority_for(syscall: &str) -> Option<WasixAuthority> {
    let authority = match syscall {
        "args_get" | "args_sizes_get" => WasixAuthority::Args,
        "environ_get" | "environ_sizes_get" => WasixAuthority::Environment,
        "clock_time_get" | "sched_yield" => WasixAuthority::ClockRead,
        "fd_read" | "fd_readdir" => WasixAuthority::DescriptorRead,
        "fd_write" => WasixAuthority::DescriptorWrite,
        "fd_fdstat_get" | "fd_filestat_get" | "fd_prestat_get" | "fd_prestat_dir_name" => {
            WasixAuthority::DescriptorMetadata
        }
        "fd_close" | "fd_fdstat_set_flags" | "fd_renumber" | "fd_seek" | "path_open" => {
            WasixAuthority::DescriptorMutate
        }
        "path_filestat_get" => WasixAuthority::PathMetadata,
        "proc_exit" | "proc_exit2" => WasixAuthority::ProcessExit,
        "clock_time_set" => WasixAuthority::SetWallClock,
        "fd_dup" | "fd_dup2" | "fd_event" | "fd_pipe" | "path_open2" | "fd_fdflags_get"
        | "fd_fdflags_set" | "getcwd" | "chdir" => WasixAuthority::DescriptorTable,
        "tty_get" | "tty_set" => WasixAuthority::TerminalControl,
        "callback_signal" | "thread_spawn_v2" | "thread_sleep" | "thread_id" | "thread_join"
        | "thread_parallelism" | "thread_signal" | "thread_exit" => WasixAuthority::Thread,
        "futex_wait" | "futex_wake" | "futex_wake_all" => WasixAuthority::Futex,
        "stack_checkpoint" | "stack_restore" => WasixAuthority::StackCheckpoint,
        "proc_raise_interval" | "proc_signal" => WasixAuthority::ProcessSignal,
        "proc_fork" => WasixAuthority::Fork,
        "proc_exec" | "proc_exec2" | "proc_exec3" => WasixAuthority::Exec,
        "proc_spawn" | "proc_spawn2" => WasixAuthority::Spawn,
        "proc_id"
        | "proc_parent"
        | "proc_signals_get"
        | "proc_signals_sizes_get"
        | "proc_snapshot" => WasixAuthority::ProcessQuery,
        "proc_join" => WasixAuthority::ProcessJoin,
        "epoll_create" | "epoll_ctl" | "epoll_wait" => WasixAuthority::Poll,
        "port_bridge" | "port_unbridge" | "port_dhcp_acquire" | "port_addr_add"
        | "port_addr_remove" | "port_addr_clear" | "port_mac" | "port_addr_list"
        | "port_gateway_set" | "port_route_add" | "port_route_remove" | "port_route_clear"
        | "port_route_list" => WasixAuthority::NetworkAdmin,
        "sock_status" | "sock_addr_local" | "sock_addr_peer" | "sock_open" | "sock_pair"
        | "sock_set_opt_flag" | "sock_get_opt_flag" | "sock_set_opt_time" | "sock_get_opt_time"
        | "sock_set_opt_size" | "sock_get_opt_size" | "sock_listen" | "sock_accept_v2"
        | "sock_connect" | "sock_recv_from" | "sock_send_to" | "sock_send_file" => {
            WasixAuthority::Socket
        }
        "sock_join_multicast_v4"
        | "sock_leave_multicast_v4"
        | "sock_join_multicast_v6"
        | "sock_leave_multicast_v6" => WasixAuthority::Multicast,
        "sock_bind" => WasixAuthority::PrivilegedBindIfLowPort,
        "resolve" => WasixAuthority::Dns,
        _ => return None,
    };
    Some(authority)
}

pub(crate) fn manifest_is_mapped() -> bool {
    manifest().all(|syscall| authority_for(syscall).is_some())
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeSet;

    use super::{WasixAuthority, authority_for, manifest};

    #[test]
    fn manifest_has_unique_syscall_names() {
        let mut seen = BTreeSet::new();
        for syscall in manifest() {
            assert!(seen.insert(syscall), "duplicate WASIX syscall {syscall}");
        }
    }

    #[test]
    fn authority_mapping_covers_manifest() {
        for syscall in manifest() {
            assert!(
                authority_for(syscall).is_some(),
                "WASIX syscall {syscall} has no capability mapping"
            );
        }
    }

    #[test]
    fn sensitive_syscalls_map_to_explicit_authority() {
        assert_eq!(
            authority_for("clock_time_set"),
            Some(WasixAuthority::SetWallClock)
        );
        assert_eq!(
            authority_for("port_route_add"),
            Some(WasixAuthority::NetworkAdmin)
        );
        assert_eq!(
            authority_for("sock_join_multicast_v4"),
            Some(WasixAuthority::Multicast)
        );
        assert_eq!(
            authority_for("sock_bind"),
            Some(WasixAuthority::PrivilegedBindIfLowPort)
        );
        assert_eq!(authority_for("proc_fork"), Some(WasixAuthority::Fork));
        assert_eq!(authority_for("proc_exec"), Some(WasixAuthority::Exec));
        assert_eq!(authority_for("proc_spawn"), Some(WasixAuthority::Spawn));
    }
}
