use super::*;

/// The operand tuples the WASIX spawn calls take on the wire.
///
/// A `func_wrap_async` closure has to spell out the guest's parameter
/// tuple, and these two calls pass every argument as an `i32` pointer
/// or length; the shape is the ABI, not a modelling choice.
type WasixHostArgs13 = (
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
);
type WasixHostArgs14 = (
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
);

/// What a spawning guest hands down to its child besides the program
/// itself. `None` means the child inherits the parent's.
pub(super) struct WasixChildInheritance {
    pub(super) environment: Option<Vec<(String, String)>>,
    pub(super) authority: Option<ProcessAuthority>,
    pub(super) descriptors: Option<Preview1DescriptorTable>,
    pub(super) signal_dispositions: Vec<WasixSignalDisposition>,
}

pub(super) struct WasixPreparedProgram {
    pub(super) guest_name: String,
    pub(super) source: ProgramSource,
}

pub(super) struct WasixSpawnFdSnapshot {
    pub(super) descriptors: Preview1DescriptorTable,
    pub(super) authority: ProcessAuthority,
    pub(super) cwd: Option<Preview1Cwd>,
}

pub(super) enum WasixExecSearchPath<'a> {
    Default,
    Guest(&'a str),
}

pub(super) fn add_wasix_extended_program_imports<CpuImpl, HostFs>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "thread_spawn_v2",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (args, ret_tid): (i32, i32)| {
                Box::new(async move {
                    wasix_thread_spawn_v2(&mut caller, args as u32, ret_tid as u32).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "stack_checkpoint",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (snapshot, ret_value): (i32, i32)| {
                Box::new(async move {
                    wasix_stack_checkpoint(&mut caller, snapshot as u32, ret_value as u32).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "stack_restore",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (snapshot, value): (i32, i64)| {
                Box::new(async move {
                    wasix_stack_restore(&mut caller, snapshot as u32, value as u64).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "proc_raise_interval",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             signal: i32,
             interval: i64,
             repeat: i32|
             -> i32 {
                wasix_proc_raise_interval(&mut caller, signal, interval, repeat)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "proc_fork",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (_copy_memory, ret_pid): (i32, i32)| {
                Box::new(async move { wasix_proc_fork(&mut caller, ret_pid as u32).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "proc_exec",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (name, name_len, args, args_len): (i32, i32, i32, i32)| {
                Box::new(async move {
                    wasix_proc_exec(
                        &mut caller,
                        name as u32,
                        name_len as u32,
                        args as u32,
                        args_len as u32,
                        None,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "proc_exec2",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (name, name_len, args, args_len, env, env_len): (i32, i32, i32, i32, i32, i32)| {
                Box::new(async move {
                    wasix_proc_exec(
                        &mut caller,
                        name as u32,
                        name_len as u32,
                        args as u32,
                        args_len as u32,
                        Some((env as u32, env_len as u32)),
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "proc_exec3",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (name, name_len, args, args_len, env, env_len, search_path, path, path_len): (
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
            )| {
                Box::new(async move {
                    wasix_proc_exec3(
                        &mut caller,
                        name as u32,
                        name_len as u32,
                        args as u32,
                        args_len as u32,
                        env as u32,
                        env_len as u32,
                        search_path,
                        path as u32,
                        path_len as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "proc_spawn",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (
                name,
                name_len,
                chroot,
                args,
                args_len,
                preopen,
                preopen_len,
                stdin,
                stdout,
                stderr,
                working_dir,
                working_dir_len,
                ret_handles,
            ): WasixHostArgs13| {
                Box::new(async move {
                    wasix_proc_spawn(
                        &mut caller,
                        name as u32,
                        name_len as u32,
                        chroot,
                        args as u32,
                        args_len as u32,
                        preopen as u32,
                        preopen_len as u32,
                        stdin,
                        stdout,
                        stderr,
                        working_dir as u32,
                        working_dir_len as u32,
                        ret_handles as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "proc_spawn2",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (
                name,
                name_len,
                args,
                args_len,
                env,
                env_len,
                fd_ops,
                fd_ops_len,
                signals,
                signals_len,
                search_path,
                path,
                path_len,
                ret_pid,
            ): WasixHostArgs14| {
                Box::new(async move {
                    wasix_proc_spawn2(
                        &mut caller,
                        name as u32,
                        name_len as u32,
                        args as u32,
                        args_len as u32,
                        env as u32,
                        env_len as u32,
                        fd_ops as u32,
                        fd_ops_len as u32,
                        signals as u32,
                        signals_len as u32,
                        search_path,
                        path as u32,
                        path_len as u32,
                        ret_pid as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "proc_join",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (pid, flags, ret_status): (i32, i32, i32)| {
                Box::new(async move {
                    wasix_proc_join(&mut caller, pid as u32, flags as u32, ret_status as u32).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "proc_snapshot",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, (): ()| {
                Box::new(async move { wasix_proc_snapshot(&mut caller).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    add_wasix_port_imports(linker)?;
    add_wasix_socket_imports(linker)?;
    add_wasix_epoll_imports(linker)?;
    Ok(())
}

pub(super) fn add_wasix_program_imports<CpuImpl, HostFs>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap(
            WASIX_MODULE,
            "clock_time_set",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             clock_id: i32,
             timestamp: i64|
             -> i32 { wasix_clock_time_set(&mut caller, clock_id, timestamp) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_dup",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             ret_fd: i32|
             -> i32 { wasix_fd_dup(&mut caller, fd, ret_fd as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_dup2",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             min_result_fd: i32,
             cloexec: i32,
             ret_fd: i32|
             -> i32 {
                wasix_fd_dup2(&mut caller, fd, min_result_fd, cloexec != 0, ret_fd as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_pipe",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             ret_fd1: i32,
             ret_fd2: i32|
             -> i32 { wasix_fd_pipe(&mut caller, ret_fd1 as u32, ret_fd2 as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "tty_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, state: i32| -> i32 {
                wasix_tty_get(&mut caller, state as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "tty_set",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, state: i32| -> i32 {
                wasix_tty_set(&mut caller, state as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "getcwd",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             path: i32,
             path_len: i32|
             -> i32 { wasix_getcwd(&mut caller, path as u32, path_len as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "chdir",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             path: i32,
             path_len: i32|
             -> i32 { wasix_chdir(&mut caller, path as u32, path_len as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_event",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             initial_value: i64,
             flags: i32,
             ret_fd: i32|
             -> i32 {
                wasix_fd_event(
                    &mut caller,
                    initial_value as u64,
                    flags as u32,
                    ret_fd as u32,
                )
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "path_open2",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (
                fd,
                dirflags,
                path,
                path_len,
                oflags,
                fs_rights_base,
                _fs_rights_inheriting,
                fdflags,
                fdflagsext,
                opened_fd,
            ): (i32, i32, i32, i32, i32, i64, i64, i32, i32, i32)| {
                Box::new(async move {
                    wasix_path_open2(
                        &mut caller,
                        fd,
                        dirflags as u32,
                        path as u32,
                        path_len as u32,
                        oflags as u16,
                        fs_rights_base as u64,
                        fdflags as u16,
                        fdflagsext as u16,
                        opened_fd as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_fdflags_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             ret_flags: i32|
             -> i32 { wasix_fd_fdflags_get(&mut caller, fd, ret_flags as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_fdflags_set",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             flags: i32|
             -> i32 { wasix_fd_fdflags_set(&mut caller, fd, flags as u16) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "callback_signal",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             callback: i32,
             callback_len: i32|
             -> wasmtime::Result<()> {
                wasix_callback_signal(&mut caller, callback as u32, callback_len as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "proc_id",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, ret_pid: i32| -> i32 {
                wasix_proc_id(&mut caller, ret_pid as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "proc_signal",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             pid: i32,
             signal: i32|
             -> wasmtime::Result<i32> {
                wasix_proc_signal(&mut caller, pid as u32, signal)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "proc_signals_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, buf: i32| -> i32 {
                wasix_proc_signals_get(&mut caller, buf as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "proc_signals_sizes_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, ret_size: i32| -> i32 {
                wasix_proc_signals_sizes_get(&mut caller, ret_size as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "resolve",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (host, host_len, port, addrs, naddrs, ret_naddrs): (
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
            )| {
                Box::new(async move {
                    wasix_resolve(
                        &mut caller,
                        host as u32,
                        host_len as u32,
                        port,
                        addrs as u32,
                        naddrs as u32,
                        ret_naddrs as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "proc_parent",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             pid: i32,
             ret_pid: i32|
             -> i32 { wasix_proc_parent(&mut caller, pid as u32, ret_pid as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "thread_sleep",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, (duration,): (i64,)| {
                Box::new(async move { wasix_thread_sleep(&mut caller, duration).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "thread_id",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, ret_tid: i32| -> i32 {
                wasix_thread_id(&mut caller, ret_tid as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "thread_join",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, (tid,): (i32,)| {
                Box::new(async move { wasix_thread_join(&mut caller, tid as u32).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "thread_parallelism",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             ret_parallelism: i32|
             -> i32 { wasix_thread_parallelism(&mut caller, ret_parallelism as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "thread_signal",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             tid: i32,
             signal: i32|
             -> wasmtime::Result<i32> { wasix_thread_signal(&mut caller, tid, signal) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "thread_exit",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             code: i32|
             -> wasmtime::Result<()> { wasix_thread_exit(&mut caller, code as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "futex_wait",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (futex, expected, timeout, ret_woken): (i32, i32, i32, i32)| {
                Box::new(async move {
                    wasix_futex_wait(
                        &mut caller,
                        futex as u32,
                        expected as u32,
                        timeout as u32,
                        ret_woken as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "futex_wake",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             futex: i32,
             ret_woken: i32|
             -> i32 { wasix_futex_wake(&mut caller, futex as u32, ret_woken as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "futex_wake_all",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             futex: i32,
             ret_woken: i32|
             -> i32 {
                wasix_futex_wake_all(&mut caller, futex as u32, ret_woken as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    add_wasix_preview1_alias_imports(linker)?;
    add_wasix_extended_program_imports(linker)?;
    Ok(())
}

pub(super) fn add_wasix_preview1_alias_imports<CpuImpl, HostFs>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap(
            WASIX_MODULE,
            "args_sizes_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             argc: i32,
             argv_buf_size: i32|
             -> i32 {
                p1_args_sizes_get(&mut caller, argc as u32, argv_buf_size as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "args_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             argv: i32,
             argv_buf: i32|
             -> i32 { p1_args_get(&mut caller, argv as u32, argv_buf as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "environ_sizes_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             count: i32,
             size: i32|
             -> i32 { p1_environ_sizes_get(&mut caller, count as u32, size as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "environ_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             environ: i32,
             environ_buf: i32|
             -> i32 { p1_environ_get(&mut caller, environ as u32, environ_buf as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "clock_time_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             id: i32,
             _precision: i64,
             timestamp: i32|
             -> i32 { p1_clock_time_get(&mut caller, id, timestamp as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_close",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, fd: i32| -> i32 {
                caller.data_mut().descriptors.close(fd)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_fdstat_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             stat: i32|
             -> i32 { p1_fd_fdstat_get(&mut caller, fd, stat as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_fdstat_set_flags",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             fdflags: i32|
             -> i32 { p1_fd_fdstat_set_flags(&mut caller, fd, fdflags as u16) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "fd_filestat_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, stat): (i32, i32)| {
                Box::new(async move { p1_fd_filestat_get(&mut caller, fd, stat as u32).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_prestat_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             buf: i32|
             -> i32 { p1_fd_prestat_get(&mut caller, fd, buf as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_prestat_dir_name",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             path: i32,
             len: i32|
             -> i32 {
                p1_fd_prestat_dir_name(&mut caller, fd, path as u32, len as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "fd_read",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, iovs, iovs_len, nread): (i32, i32, i32, i32)| {
                Box::new(async move {
                    p1_fd_read(&mut caller, fd, iovs as u32, iovs_len as u32, nread as u32).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "fd_readdir",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, buf, buf_len, cookie, bufused): (i32, i32, i32, i64, i32)| {
                Box::new(async move {
                    p1_fd_readdir(
                        &mut caller,
                        fd,
                        buf as u32,
                        buf_len as u32,
                        cookie as u64,
                        bufused as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_renumber",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             from: i32,
             to: i32|
             -> i32 { p1_fd_renumber(&mut caller, from, to) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "fd_seek",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, offset, whence, new_offset): (i32, i64, i32, i32)| {
                Box::new(async move {
                    p1_fd_seek(&mut caller, fd, offset, whence as u8, new_offset as u32).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "fd_write",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, iovs, iovs_len, nwritten): (i32, i32, i32, i32)| {
                Box::new(async move {
                    p1_fd_write(
                        &mut caller,
                        fd,
                        iovs as u32,
                        iovs_len as u32,
                        nwritten as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "path_filestat_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, flags, path, path_len, stat): (i32, i32, i32, i32, i32)| {
                Box::new(async move {
                    wasix_path_filestat_get(
                        &mut caller,
                        fd,
                        flags as u32,
                        path as u32,
                        path_len as u32,
                        stat as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "path_open",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (
                fd,
                dirflags,
                path,
                path_len,
                oflags,
                fs_rights_base,
                _fs_rights_inheriting,
                fdflags,
                opened_fd,
            ): (i32, i32, i32, i32, i32, i64, i64, i32, i32)| {
                Box::new(async move {
                    wasix_path_open(
                        &mut caller,
                        fd,
                        dirflags as u32,
                        path as u32,
                        path_len as u32,
                        oflags as u16,
                        fs_rights_base as u64,
                        fdflags as u16,
                        opened_fd as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sched_yield",
            |_caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, ()| {
                Box::new(async move {
                    crate::yield_now().await;
                    p1::errno::SUCCESS
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "proc_exit",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             code: i32|
             -> wasmtime::Result<()> {
                caller.data_mut().request_exit(code as u32);
                Err(wasmtime::Error::new(Preview1Exit))
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "proc_exit2",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             code: i32|
             -> wasmtime::Result<()> {
                caller.data_mut().request_exit(code as u32);
                Err(wasmtime::Error::new(Preview1Exit))
            },
        )
        .map_err(map_program_runtime_error)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn wasix_path_open<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    dirflags: u32,
    path: u32,
    path_len: u32,
    oflags: u16,
    rights: u64,
    fdflags: u16,
    opened_fd: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let path = match wasix_read_exec_string(caller, memory, path, path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let (base, path) = match caller.data().resolve_wasix_path_base(fd, &path) {
        Ok(resolved) => resolved,
        Err(errno) => return errno,
    };
    let path_flags = p1_path_flags(dirflags);
    let open_flags = p1_open_flags(oflags);
    if !p1_file_fdflags_supported(fdflags) {
        return p1::errno::INVAL;
    }
    let mut descriptor_flags = p1_descriptor_flags(rights, fdflags);
    if open_flags.intersects(fs_types::OpenFlags::CREATE | fs_types::OpenFlags::TRUNCATE) {
        descriptor_flags |= fs_types::DescriptorFlags::WRITE;
    }
    p1_path_open_resolved(
        caller,
        memory,
        base,
        path,
        path_flags,
        open_flags,
        descriptor_flags,
        fdflags,
        opened_fd,
    )
    .await
}

pub(super) async fn wasix_path_filestat_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    _flags: u32,
    path: u32,
    path_len: u32,
    stat: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let path = match p1_read_path(caller, memory, path, path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let (base, path) = match caller.data().resolve_wasix_path_base(fd, &path) {
        Ok(resolved) => resolved,
        Err(errno) => return errno,
    };
    let absolute = match crate::resolve_child_path(&base.path, &path) {
        Ok(absolute) => absolute,
        Err(error) => return p1_errno_from_component_path(error),
    };
    if absolute == WASIX_NULL_DEVICE_PATH {
        return p1_write_filestat(
            caller,
            stat,
            p1_null_device_identity(),
            p1_null_device_stat(),
        );
    }
    let (identity, stat_value) = match p1_stat_absolute_path(caller, &absolute).await {
        Ok(stat) => stat,
        Err(errno) => return errno,
    };
    p1_write_filestat(caller, stat, identity, stat_value)
}

pub(super) fn wasix_clock_time_set<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    clock_id: i32,
    timestamp: i64,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if clock_id != 0 {
        return p1::errno::INVAL;
    }
    let Ok(timestamp) = u64::try_from(timestamp) else {
        return p1::errno::INVAL;
    };
    caller.data_mut().set_system_time_nanos(timestamp)
}

pub(super) fn wasix_fd_dup<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    ret_fd: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let duplicated = match caller.data_mut().descriptors.dup(fd) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    p1_write_u32(caller, memory, ret_fd, duplicated)
}

pub(super) fn wasix_fd_dup2<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    target_fd: i32,
    cloexec: bool,
    ret_fd: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let duplicated = match caller.data_mut().descriptors.dup_to(fd, target_fd, cloexec) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    p1_write_u32(caller, memory, ret_fd, duplicated)
}

pub(super) fn wasix_fd_pipe<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    ret_fd1: u32,
    ret_fd2: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let (writer, reader) = crate::byte_channel();
    let read_fd = match caller
        .data_mut()
        .descriptors
        .insert(Preview1Descriptor::PipeRead {
            reader,
            carry: Bytes::new(),
        }) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    let write_fd = match caller
        .data_mut()
        .descriptors
        .insert(Preview1Descriptor::PipeWrite { writer })
    {
        Ok(fd) => fd,
        Err(errno) => {
            let _ = caller.data_mut().descriptors.close(read_fd as i32);
            return errno;
        }
    };
    let status = p1_write_u32(caller, memory, ret_fd1, read_fd);
    if status != p1::errno::SUCCESS {
        return status;
    }
    p1_write_u32(caller, memory, ret_fd2, write_fd)
}

pub(super) fn wasix_fd_event<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    initial_value: u64,
    flags: u32,
    ret_fd: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if flags & !WASIX_EVENTFDFLAG_SEMAPHORE != 0 {
        return p1::errno::INVAL;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let fd = match caller
        .data_mut()
        .descriptors
        .insert(Preview1Descriptor::Event(EventFd::new(
            initial_value,
            flags & WASIX_EVENTFDFLAG_SEMAPHORE != 0,
        ))) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    p1_write_u32(caller, memory, ret_fd, fd)
}

pub(super) fn wasix_tty_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    state: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_tty_control();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let tty = caller.data().tty_state;
    write_wasix_tty_state(caller, memory, state, tty)
}

pub(super) fn wasix_tty_set<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    state: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_tty_control();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let tty = match read_wasix_tty_state(caller, memory, state) {
        Ok(tty) => tty,
        Err(errno) => return errno,
    };
    caller.data_mut().tty_state = tty;
    p1::errno::SUCCESS
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn wasix_path_open2<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    dirflags: u32,
    path: u32,
    path_len: u32,
    oflags: u16,
    rights: u64,
    fdflags: u16,
    fdflagsext: u16,
    opened_fd: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let close_on_exec = match wasix_close_on_exec_flag(fdflagsext) {
        Ok(close_on_exec) => close_on_exec,
        Err(errno) => return errno,
    };
    let status = wasix_path_open(
        caller, fd, dirflags, path, path_len, oflags, rights, fdflags, opened_fd,
    )
    .await;
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let opened = match p1_try_read_u32(caller, memory, opened_fd) {
        Ok(opened) => opened,
        Err(_) => return p1::errno::FAULT,
    };
    caller
        .data_mut()
        .descriptors
        .set_close_on_exec(opened as i32, close_on_exec)
}

pub(super) fn wasix_fd_fdflags_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    ret_flags: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let close_on_exec = match caller.data().descriptors.close_on_exec(fd) {
        Ok(close_on_exec) => close_on_exec,
        Err(errno) => return errno,
    };
    let flags = if close_on_exec {
        WASIX_FDFLAGSEXT_CLOEXEC
    } else {
        0
    };
    p1_write_u16(caller, memory, ret_flags, flags)
}

pub(super) fn wasix_fd_fdflags_set<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    flags: u16,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let close_on_exec = match wasix_close_on_exec_flag(flags) {
        Ok(close_on_exec) => close_on_exec,
        Err(errno) => return errno,
    };
    caller
        .data_mut()
        .descriptors
        .set_close_on_exec(fd, close_on_exec)
}

pub(super) fn wasix_close_on_exec_flag(flags: u16) -> Result<bool, i32> {
    if flags & !WASIX_FDFLAGSEXT_CLOEXEC != 0 {
        return Err(p1::errno::INVAL);
    }
    Ok(flags & WASIX_FDFLAGSEXT_CLOEXEC != 0)
}

pub(super) fn wasix_getcwd<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    path: u32,
    path_len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let capacity = match p1_try_read_u32(caller, memory, path_len) {
        Ok(capacity) => capacity,
        Err(_) => return p1::errno::FAULT,
    };
    let cwd = match caller.data().getcwd() {
        Ok(cwd) => cwd,
        Err(errno) => return errno,
    };
    let needed = match wasix_getcwd_required_len(cwd) {
        Ok(needed) => needed,
        Err(errno) => return errno,
    };
    let status = preview1_write_u32(memory, path_len, needed);
    if status != p1::errno::SUCCESS {
        return status;
    }
    if capacity < needed {
        return p1::errno::RANGE;
    }
    let status = preview1_write_memory(memory, path, cwd.as_bytes());
    if status != p1::errno::SUCCESS {
        return status;
    }
    if capacity > needed {
        return preview1_write_memory(memory, path.saturating_add(needed), &[0]);
    }
    p1::errno::SUCCESS
}

pub(super) fn wasix_getcwd_required_len(cwd: &str) -> Result<u32, i32> {
    u32::try_from(cwd.len()).map_err(|_| p1::errno::OVERFLOW)
}

pub(super) fn wasix_chdir<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    path: u32,
    path_len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let path = match wasix_read_exec_string(caller, memory, path, path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    caller.data_mut().chdir(&path)
}

pub(super) fn wasix_callback_signal<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    callback: u32,
    callback_len: u32,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        }));
    };
    let callback =
        p1_read_path(caller, memory, callback, callback_len).map_err(wasmtime::Error::new)?;
    caller.data_mut().signal_callback = Some(callback);
    Ok(())
}

pub(super) fn wasix_proc_id<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    ret_pid: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let pid = match u32::try_from(caller.data().instance().id().raw()) {
        Ok(pid) => pid,
        Err(_) => return p1::errno::OVERFLOW,
    };
    p1_write_u32(caller, memory, ret_pid, pid)
}

pub(super) fn wasix_proc_parent<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    pid: u32,
    ret_pid: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let current = match u32::try_from(caller.data().instance().id().raw()) {
        Ok(pid) => pid,
        Err(_) => return p1::errno::OVERFLOW,
    };
    if pid != current {
        return p1::errno::NOENT;
    }
    let parent = match caller.data().parent_instance_id {
        Some(parent) => match u32::try_from(parent.raw()) {
            Ok(pid) => pid,
            Err(_) => return p1::errno::OVERFLOW,
        },
        None => 0,
    };
    p1_write_u32(caller, memory, ret_pid, parent)
}

pub(super) fn wasix_proc_signal<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    pid: u32,
    signal: i32,
) -> wasmtime::Result<i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_signal_authority();
    if status != p1::errno::SUCCESS {
        return Ok(status);
    }
    let current = match u32::try_from(caller.data().instance().id().raw()) {
        Ok(pid) => pid,
        Err(_) => return Ok(p1::errno::OVERFLOW),
    };
    if !(0..=31).contains(&signal) {
        return Ok(p1::errno::INVAL);
    };
    let signal = signal as u32;
    if pid == current {
        caller
            .data_mut()
            .request_exit(128u32.saturating_add(signal));
        return Err(wasmtime::Error::new(Preview1Exit));
    }
    let Some(index) = caller.data().find_child_index(Some(pid)) else {
        return Ok(p1::errno::SRCH);
    };
    if caller.data().children[index].completed.is_some() {
        return Ok(p1::errno::SRCH);
    }
    caller.data().children[index].signal_state.raise(signal);
    Ok(p1::errno::SUCCESS)
}

pub(super) fn wasix_proc_signals_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    buf: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let dispositions = caller.data().signal_dispositions.clone();
    for (index, disposition) in dispositions.iter().enumerate() {
        let Ok(index) = u32::try_from(index) else {
            return p1::errno::OVERFLOW;
        };
        let Some(offset) = index
            .checked_mul(WASIX_SIGNAL_DISPOSITION_SIZE)
            .and_then(|offset| buf.checked_add(offset))
        else {
            return p1::errno::OVERFLOW;
        };
        let action = match disposition.action {
            WasixSignalDispositionAction::Default => WASIX_SIGNAL_DISPOSITION_DEFAULT,
            WasixSignalDispositionAction::Ignore => WASIX_SIGNAL_DISPOSITION_IGNORE,
        };
        let status = p1_write_u8(
            caller,
            memory,
            offset + WASIX_SIGNAL_DISPOSITION_SIGNAL_OFFSET,
            disposition.signal,
        )
        .max(p1_write_u8(
            caller,
            memory,
            offset + WASIX_SIGNAL_DISPOSITION_ACTION_OFFSET,
            action,
        ));
        if status != p1::errno::SUCCESS {
            return status;
        }
    }
    p1::errno::SUCCESS
}

pub(super) fn wasix_proc_signals_sizes_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    ret_size: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let Ok(len) = u32::try_from(caller.data().signal_dispositions.len()) else {
        return p1::errno::OVERFLOW;
    };
    p1_write_u32(caller, memory, ret_size, len)
}

pub(super) fn wasix_proc_raise_interval<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    signal: i32,
    interval: i64,
    repeat: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_signal_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    if !(0..=31).contains(&signal) {
        return p1::errno::INVAL;
    }
    if interval < 0 || !matches!(repeat, 0 | 1) {
        return p1::errno::INVAL;
    }
    let signal = match u32::try_from(signal) {
        Ok(signal) => signal,
        Err(_) => return p1::errno::INVAL,
    };
    if interval == 0 {
        caller.data().signal_state.cancel_interval();
        return p1::errno::SUCCESS;
    }
    let interval = match u64::try_from(interval) {
        Ok(interval) => interval,
        Err(_) => return p1::errno::INVAL,
    };
    let repeat = repeat != 0;
    let signal_state = caller.data().signal_state.clone();
    let generation = signal_state.next_interval_generation();
    let timer = caller.data().timer();
    if let Err(error) = caller.data().spawner.try_spawn_detached(async move {
        loop {
            timer.sleep_for(Duration::from_nanos(interval)).await;
            if !signal_state.interval_generation_is_current(generation) {
                break;
            }
            signal_state.raise(signal);
            if !repeat {
                break;
            }
        }
    }) {
        tracing::warn!(
            target: "helios_kernel::program",
            %error,
            "refused a signal interval timer: the executor's instance share is full"
        );
        return p1::errno::NOMEM;
    }
    p1::errno::SUCCESS
}

pub(super) async fn wasix_proc_fork<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    ret_pid: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if let Some(value) = caller.data_mut().asyncify.rewind_value.take() {
        if wasix_call_asyncify_stop_rewind(caller).await != p1::errno::SUCCESS {
            return p1::errno::NOTSUP;
        }
        let Some(memory) = p1_memory(caller) else {
            return p1::errno::FAULT;
        };
        let Ok(pid) = u32::try_from(value) else {
            return p1::errno::OVERFLOW;
        };
        return p1_write_u32(caller, memory, ret_pid, pid);
    }
    let status = caller.data().require_fork_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    if caller.data().current_core_module.is_none() {
        return p1::errno::NOTSUP;
    }
    let Ok((stack_lower, stack_upper, stack_pointer)) = wasix_stack_bounds_from_caller(caller)
    else {
        return p1::errno::NOTSUP;
    };
    if stack_lower >= stack_pointer || stack_pointer > stack_upper {
        return p1::errno::INVAL;
    }
    let memory_stack_len = match usize::try_from(stack_upper - stack_pointer) {
        Ok(len) => len,
        Err(_) => return p1::errno::OVERFLOW,
    };
    let memory_stack = match p1_read_memory(caller, memory, stack_pointer, memory_stack_len) {
        Ok(stack) => stack,
        Err(_) => return p1::errno::FAULT,
    };
    let unwind_stack_begin = match stack_lower.checked_add(WASIX_ASYNCIFY_DATA_SIZE) {
        Some(begin) if begin <= stack_pointer => begin,
        _ => return p1::errno::OVERFLOW,
    };
    let status = p1_write_u32(caller, memory, ret_pid, 0);
    if status != p1::errno::SUCCESS {
        return status;
    }
    let status = p1_write_u32(caller, memory, stack_lower, unwind_stack_begin).max(p1_write_u32(
        caller,
        memory,
        stack_lower + 4,
        stack_pointer,
    ));
    if status != p1::errno::SUCCESS {
        return status;
    }
    caller.data_mut().asyncify.phase = WasixAsyncifyPhase::Forking {
        ret_pid,
        stack_lower,
        stack_upper,
        unwind_stack_begin,
        memory_stack,
        stack_pointer,
    };
    wasix_call_asyncify_start_unwind(caller, stack_lower).await
}

pub(super) async fn wasix_proc_exec<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    name: u32,
    name_len: u32,
    args: u32,
    args_len: u32,
    env: Option<(u32, u32)>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_exec_authority();
    if status != p1::errno::SUCCESS {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::PermissionDenied,
            detail: ProgramExecErrorDetail::ProcessAuthorityDenied,
        }));
    }
    let Some(memory) = p1_memory(caller) else {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        }));
    };
    let prepared = wasix_prepare_program(caller, memory, name, name_len).await?;
    wasix_exec_prepared_program(caller, memory, prepared, args, args_len, env).await
}

pub(super) async fn wasix_exec_prepared_program<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    memory: Preview1Memory,
    prepared: WasixPreparedProgram,
    args: u32,
    args_len: u32,
    env: Option<(u32, u32)>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let argv = wasix_read_exec_string(caller, memory, args, args_len)
        .map(|value| wasix_split_lines(&value))
        .map_err(wasmtime::Error::new)?;
    let argv = ProgramArgv::from_caller(&prepared.guest_name, argv);
    let mut environment = match env {
        Some((ptr, len)) => wasix_read_exec_string(caller, memory, ptr, len)
            .map(|value| wasix_split_environment(&value))
            .unwrap_or_default(),
        None => caller.data().environment.clone(),
    };
    let process_id = caller.data().instance().id().raw().to_string();
    environment.retain(|(name, _)| name.as_str() != HELIOS_PROCESS_ID_ENV);
    environment.push((HELIOS_PROCESS_ID_ENV.into(), process_id));
    let service = caller
        .data()
        .runtime_state
        .program_service()
        .ok_or_else(|| {
            wasmtime::Error::new(ProgramExecError {
                kind: ProgramExecErrorKind::Unavailable,
                detail: ProgramExecErrorDetail::HostOperationFailed,
            })
        })?;
    let exec_context = caller.data().exec_context();
    let authority = caller.data().authority.clone();
    let descriptors = caller.data().descriptors.clone_for_exec();
    let filesystem = Some(caller.data().filesystem.snapshot());
    let signal_state = caller.data().signal_state.clone();
    let signal_dispositions = caller.data().signal_dispositions.clone();
    let write_serial = caller.data().write_serial;
    let executable = service
        .load_executable(&exec_context, &prepared.source, None, write_serial)
        .await
        .map_err(wasmtime::Error::new)?;
    caller
        .data_mut()
        .request_exec_replacement(WasixExecReplacement {
            argv,
            env: environment,
            executable,
            authority,
            filesystem,
            descriptors: Some(descriptors),
            signal_state,
            signal_dispositions,
        });
    Err(wasmtime::Error::new(Preview1Exit))
}

#[expect(
    clippy::too_many_arguments,
    reason = "the parameter list is the guest ABI of this call, so grouping it would hide the contract and break the one-to-one match with the linker registration"
)]
pub(super) async fn wasix_proc_exec3<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    name: u32,
    name_len: u32,
    args: u32,
    args_len: u32,
    env: u32,
    env_len: u32,
    search_path: i32,
    path: u32,
    path_len: u32,
) -> wasmtime::Result<i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    wasix_proc_exec_with_search(
        caller,
        name,
        name_len,
        args,
        args_len,
        Some((env, env_len)),
        search_path,
        path,
        path_len,
    )
    .await
    .map(|()| p1::errno::SUCCESS)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn wasix_proc_exec_with_search<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    name: u32,
    name_len: u32,
    args: u32,
    args_len: u32,
    env: Option<(u32, u32)>,
    search_path: i32,
    path: u32,
    path_len: u32,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_exec_authority();
    if status != p1::errno::SUCCESS {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::PermissionDenied,
            detail: ProgramExecErrorDetail::ProcessAuthorityDenied,
        }));
    }
    let Some(memory) = p1_memory(caller) else {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        }));
    };
    let prepared = wasix_prepare_program_with_search(
        caller,
        memory,
        name,
        name_len,
        search_path,
        path,
        path_len,
    )
    .await?;
    wasix_exec_prepared_program(caller, memory, prepared, args, args_len, env).await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn wasix_proc_spawn<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    name: u32,
    name_len: u32,
    chroot: i32,
    args: u32,
    args_len: u32,
    preopen: u32,
    preopen_len: u32,
    stdin: i32,
    stdout: i32,
    stderr: i32,
    working_dir: u32,
    working_dir_len: u32,
    ret_handles: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let started = p1_kernel_profile_start(caller.data());
    let result = wasix_proc_spawn_inner(
        caller,
        name,
        name_len,
        chroot,
        args,
        args_len,
        preopen,
        preopen_len,
        stdin,
        stdout,
        stderr,
        working_dir,
        working_dir_len,
        ret_handles,
    )
    .await;
    p1_record_optional_kernel_profile(caller.data(), "proc_spawn", started);
    result
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn wasix_proc_spawn_inner<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    name: u32,
    name_len: u32,
    chroot: i32,
    args: u32,
    args_len: u32,
    preopen: u32,
    preopen_len: u32,
    stdin: i32,
    stdout: i32,
    stderr: i32,
    working_dir: u32,
    working_dir_len: u32,
    ret_handles: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_spawn_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let chroot = match chroot {
        0 => false,
        1 => true,
        _ => return p1::errno::INVAL,
    };
    let mut authority = if preopen_len == 0 {
        if chroot {
            return p1::errno::INVAL;
        }
        caller.data().authority.clone()
    } else {
        let preopen = match wasix_read_exec_string(caller, memory, preopen, preopen_len) {
            Ok(preopen) => preopen,
            Err(_) => return p1::errno::FAULT,
        };
        match wasix_proc_spawn_preopen_authority(caller.data(), &preopen, chroot) {
            Ok(authority) => authority,
            Err(errno) => return errno,
        }
    };
    if working_dir_len != 0 {
        let working_dir = match wasix_read_exec_string(caller, memory, working_dir, working_dir_len)
        {
            Ok(working_dir) => working_dir,
            Err(_) => return p1::errno::FAULT,
        };
        if !working_dir.is_empty() && working_dir != "." {
            let cwd =
                match wasix_proc_spawn_resolve_child_cwd(caller.data(), &authority, &working_dir) {
                    Ok(cwd) => cwd,
                    Err(errno) => return errno,
                };
            let cap = match authority.derive_directory_cap(
                &cwd.descriptor.path,
                &cwd.guest_name,
                descriptor_flags_to_directory_authority(cwd.descriptor.flags),
            ) {
                Ok(cap) => cap,
                Err(_) => return p1::errno::NOTCAPABLE,
            };
            authority.chdir(cap);
        }
    }
    let argv = match wasix_read_exec_string(caller, memory, args, args_len) {
        Ok(value) => wasix_split_lines(&value),
        Err(_) => return p1::errno::FAULT,
    };
    let prepared = match wasix_prepare_program(caller, memory, name, name_len).await {
        Ok(prepared) => prepared,
        Err(error) => return p1_errno_from_wasmtime_error(&error),
    };
    let result = match wasix_spawn_child(
        caller,
        prepared,
        argv,
        WasixSpawnIo::from_modes(stdin, stdout, stderr),
        WasixChildInheritance {
            environment: None,
            authority: Some(authority),
            descriptors: None,
            signal_dispositions: Vec::new(),
        },
    )
    .await
    {
        Ok(result) => result,
        Err(errno) => return errno,
    };
    wasix_write_process_handles(caller, memory, ret_handles, result)
}

pub(super) fn wasix_proc_spawn_preopen_authority<CpuImpl, HostFs>(
    store: &Preview1ProgramStore<CpuImpl, HostFs>,
    preopen: &str,
    chroot: bool,
) -> Result<ProcessAuthority, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let mut authority = wasix_proc_spawn_inherited_non_directory_authority(&store.authority);
    let mut chroot_entry = None;
    for entry in wasix_split_lines(preopen) {
        let guest_name = wasix_proc_spawn_preopen_guest_name(store.cwd.as_ref(), &entry)?;
        let (source_path, flags) = store.resolve_absolute_guest_path(&guest_name)?;
        let stat = store
            .filesystem
            .stat(&source_path)
            .map_err(p1_errno_from_fs)?;
        if !matches!(stat.type_, fs_types::DescriptorType::Directory) {
            return Err(p1::errno::NOTDIR);
        }
        let rights = descriptor_flags_to_directory_authority(flags);
        if chroot {
            if chroot_entry.is_some() {
                return Err(p1::errno::INVAL);
            }
            chroot_entry = Some((source_path, rights));
            continue;
        }
        let preopen = store
            .authority
            .derive_directory_preopen(&source_path, &guest_name, rights)
            .map_err(|_| p1::errno::NOTCAPABLE)?;
        authority.insert_directory_preopen(preopen);
    }
    if chroot {
        let Some((source_path, rights)) = chroot_entry else {
            return Err(p1::errno::INVAL);
        };
        let preopen = store
            .authority
            .derive_directory_preopen(&source_path, "/", rights)
            .map_err(|_| p1::errno::NOTCAPABLE)?;
        authority.insert_directory_preopen(preopen);
        let cwd = authority
            .derive_directory_cap(&source_path, "/", rights)
            .map_err(|_| p1::errno::NOTCAPABLE)?;
        authority.chdir(cwd);
    }
    Ok(authority)
}

pub(super) fn wasix_proc_spawn_inherited_non_directory_authority(
    parent: &ProcessAuthority,
) -> ProcessAuthority {
    let mut authority = ProcessAuthority::empty();
    authority.grant_network_rights(parent.network_rights());
    authority.grant_clock_rights(parent.clock_rights());
    authority.grant_terminal_rights(parent.terminal_rights());
    authority.grant_process_rights(parent.process_rights());
    authority.grant_link_rights(parent.link_rights());
    authority
}

pub(super) fn wasix_proc_spawn_preopen_guest_name(
    cwd: Option<&Preview1Cwd>,
    entry: &str,
) -> Result<String, i32> {
    if entry.starts_with('/') {
        crate::resolve_absolute_path(entry).map_err(p1_errno_from_component_path)
    } else {
        let cwd = cwd.ok_or(p1::errno::NOTCAPABLE)?;
        crate::resolve_child_path(&cwd.guest_name, entry).map_err(p1_errno_from_component_path)
    }
}

pub(super) fn wasix_proc_spawn_resolve_child_cwd<CpuImpl, HostFs>(
    store: &Preview1ProgramStore<CpuImpl, HostFs>,
    authority: &ProcessAuthority,
    path: &str,
) -> Result<Preview1Cwd, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let (guest_name, source_path, flags) = if path.starts_with('/') {
        let guest_name =
            crate::resolve_absolute_path(path).map_err(p1_errno_from_component_path)?;
        let (source_path, flags) =
            wasix_authority_resolve_absolute_guest_path(authority, &guest_name)?;
        (guest_name, source_path, flags)
    } else {
        let cwd = preview1_cwd_from_authority(authority).ok_or(p1::errno::NOTCAPABLE)?;
        let guest_name = crate::resolve_child_path(&cwd.guest_name, path)
            .map_err(p1_errno_from_component_path)?;
        let source_path = crate::resolve_child_path(&cwd.descriptor.path, path)
            .map_err(p1_errno_from_component_path)?;
        (guest_name, source_path, cwd.descriptor.flags)
    };
    if !flags.contains(fs_types::DescriptorFlags::READ) {
        return Err(p1::errno::NOTCAPABLE);
    }
    let stat = store
        .filesystem
        .stat(&source_path)
        .map_err(p1_errno_from_fs)?;
    if !matches!(stat.type_, fs_types::DescriptorType::Directory) {
        return Err(p1::errno::NOTDIR);
    }
    Ok(Preview1Cwd {
        guest_name,
        descriptor: FsDescriptor {
            path: source_path,
            kind: FsNodeKind::Directory,
            flags,
            identity: None,
        },
    })
}

pub(super) fn wasix_authority_resolve_absolute_guest_path(
    authority: &ProcessAuthority,
    guest_name: &str,
) -> Result<(String, fs_types::DescriptorFlags), i32> {
    let mut best: Option<(&str, &str, DirectoryAuthorityRights)> = None;
    for preopen in authority.directory_preopens() {
        let preopen_guest = preopen.guest_name();
        if !guest_path_is_within_preopen(guest_name, preopen_guest) {
            continue;
        }
        if best.is_none_or(|(best_guest, _, _)| preopen_guest.len() > best_guest.len()) {
            best = Some((preopen_guest, preopen.source_path(), preopen.rights()));
        }
    }
    if let Some(cwd) = authority.cwd()
        && guest_path_is_within_preopen(guest_name, cwd.guest_name())
        && best.is_none_or(|(best_guest, _, _)| cwd.guest_name().len() > best_guest.len())
    {
        best = Some((cwd.guest_name(), cwd.source_path(), cwd.rights()));
    }
    let Some((preopen_guest, source_path, rights)) = best else {
        return Err(p1::errno::NOTCAPABLE);
    };
    let suffix = guest_path_suffix(guest_name, preopen_guest);
    let source_path = if suffix.is_empty() {
        source_path.to_owned()
    } else {
        crate::resolve_child_path(source_path, suffix).map_err(p1_errno_from_component_path)?
    };
    Ok((source_path, directory_authority_to_descriptor_flags(rights)))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn wasix_proc_spawn2<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    name: u32,
    name_len: u32,
    args: u32,
    args_len: u32,
    env: u32,
    env_len: u32,
    fd_ops: u32,
    fd_ops_len: u32,
    signals: u32,
    signals_len: u32,
    search_path: i32,
    path: u32,
    path_len: u32,
    ret_pid: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let started = p1_kernel_profile_start(caller.data());
    let result = wasix_proc_spawn2_inner(
        caller,
        name,
        name_len,
        args,
        args_len,
        env,
        env_len,
        fd_ops,
        fd_ops_len,
        signals,
        signals_len,
        search_path,
        path,
        path_len,
        ret_pid,
    )
    .await;
    p1_record_optional_kernel_profile(caller.data(), "proc_spawn2", started);
    result
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn wasix_proc_spawn2_inner<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    name: u32,
    name_len: u32,
    args: u32,
    args_len: u32,
    env: u32,
    env_len: u32,
    fd_ops: u32,
    fd_ops_len: u32,
    signals: u32,
    signals_len: u32,
    search_path: i32,
    path: u32,
    path_len: u32,
    ret_pid: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_spawn_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let argv = match wasix_read_exec_string(caller, memory, args, args_len) {
        Ok(value) => wasix_split_lines(&value),
        Err(_) => return p1::errno::FAULT,
    };
    let environment = if env == 0 && env_len == 0 {
        None
    } else {
        match wasix_read_exec_string(caller, memory, env, env_len) {
            Ok(value) => Some(wasix_split_environment(&value)),
            Err(_) => return p1::errno::FAULT,
        }
    };
    let prepared = match wasix_prepare_program_with_search(
        caller,
        memory,
        name,
        name_len,
        search_path,
        path,
        path_len,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(error) => return p1_errno_from_wasmtime_error(&error),
    };
    let snapshot = if fd_ops == 0 && fd_ops_len == 0 {
        None
    } else {
        match wasix_spawn_descriptor_snapshot(caller, memory, fd_ops, fd_ops_len).await {
            Ok(snapshot) => Some(snapshot),
            Err(errno) => return errno,
        }
    };
    let (authority, descriptors) = match snapshot {
        Some(snapshot) => (Some(snapshot.authority), Some(snapshot.descriptors)),
        None => (None, None),
    };
    let signal_dispositions =
        match wasix_read_signal_dispositions(caller, memory, signals, signals_len) {
            Ok(dispositions) => dispositions,
            Err(errno) => return errno,
        };
    let result = match wasix_spawn_child(
        caller,
        prepared,
        argv,
        WasixSpawnIo::inherit(),
        WasixChildInheritance {
            environment,
            authority,
            descriptors,
            signal_dispositions,
        },
    )
    .await
    {
        Ok(result) => result,
        Err(errno) => return errno,
    };
    p1_write_u32(caller, memory, ret_pid, result.pid)
}

pub(super) async fn wasix_proc_join<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    pid: u32,
    flags: u32,
    ret_status: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let started = p1_kernel_profile_start(caller.data());
    let result = wasix_proc_join_inner(caller, pid, flags, ret_status).await;
    p1_record_optional_kernel_profile(caller.data(), "proc_join", started);
    result
}

pub(super) async fn wasix_proc_join_inner<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    pid: u32,
    flags: u32,
    ret_status: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_join_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    if flags & !WASIX_JOIN_FLAGS_SUPPORTED != 0 {
        return p1::errno::INVAL;
    }
    let requested_pid = match wasix_read_option_pid(caller, memory, pid) {
        Ok(pid) => pid,
        Err(errno) => return errno,
    };
    let Some(index) = caller.data().find_child_index(requested_pid) else {
        return wasix_write_join_nothing(caller, memory, ret_status);
    };
    if flags & WASIX_JOIN_FLAG_NON_BLOCKING != 0 {
        match caller.data_mut().poll_child_exit(index) {
            Ok(Some(code)) => {
                let child = caller.data_mut().children.swap_remove(index);
                wasix_write_join_exit(caller, memory, pid, ret_status, child.pid, code)
            }
            Ok(None) => {
                crate::yield_now().await;
                wasix_write_join_nothing(caller, memory, ret_status)
            }
            Err(errno) => errno,
        }
    } else {
        let child_pid = caller.data().children[index].pid;
        match caller.data_mut().poll_child_exit(index) {
            Ok(Some(code)) => {
                let child = caller.data_mut().children.swap_remove(index);
                return wasix_write_join_exit(caller, memory, pid, ret_status, child.pid, code);
            }
            Ok(None) => {}
            Err(errno) => return errno,
        }
        let Some(exit) = caller.data_mut().children[index].exit.take() else {
            return wasix_write_join_nothing(caller, memory, ret_status);
        };
        let (code, filesystem) = wasix_child_exit_result(exit.await);
        if let Some(filesystem) = filesystem {
            caller
                .data_mut()
                .filesystem
                .replace_with_snapshot(filesystem);
        }
        let Some(index) = caller.data().find_child_index(Some(child_pid)) else {
            return wasix_write_join_nothing(caller, memory, ret_status);
        };
        let child = caller.data_mut().children.swap_remove(index);
        wasix_write_join_exit(caller, memory, pid, ret_status, child.pid, code)
    }
}

pub(super) fn wasix_child_exit_result(
    result: Result<Result<ChildExit, ProgramExecError>, futures::channel::oneshot::Canceled>,
) -> (u32, Option<DebugFileSystemSnapshot>) {
    match result {
        Ok(Ok(exit)) => (exit.exit_code, exit.filesystem),
        Ok(Err(_)) | Err(_) => (u32::from(p1::errno::IO as u16), None),
    }
}

pub(super) async fn wasix_proc_snapshot<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if caller.data_mut().asyncify.process_snapshot_rewinding {
        caller.data_mut().asyncify.process_snapshot_rewinding = false;
        if wasix_call_asyncify_stop_rewind(caller).await != p1::errno::SUCCESS {
            return p1::errno::NOTSUP;
        }
        return p1::errno::SUCCESS;
    }

    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    if caller.data().current_core_module.is_none() {
        return p1::errno::NOTSUP;
    }
    let Ok((stack_lower, stack_upper, stack_pointer)) = wasix_stack_bounds_from_caller(caller)
    else {
        return p1::errno::NOTSUP;
    };
    if stack_lower >= stack_pointer || stack_pointer > stack_upper {
        return p1::errno::INVAL;
    }
    let memory_stack_len = match usize::try_from(stack_upper - stack_pointer) {
        Ok(len) => len,
        Err(_) => return p1::errno::OVERFLOW,
    };
    let memory_stack = match p1_read_memory(caller, memory, stack_pointer, memory_stack_len) {
        Ok(stack) => stack,
        Err(_) => return p1::errno::FAULT,
    };
    let unwind_stack_begin = match stack_lower.checked_add(WASIX_ASYNCIFY_DATA_SIZE) {
        Some(begin) if begin <= stack_pointer => begin,
        _ => return p1::errno::OVERFLOW,
    };
    let status = p1_write_u32(caller, memory, stack_lower, unwind_stack_begin).max(p1_write_u32(
        caller,
        memory,
        stack_lower + 4,
        stack_pointer,
    ));
    if status != p1::errno::SUCCESS {
        return status;
    }
    caller.data_mut().asyncify.phase = WasixAsyncifyPhase::ProcessSnapshot {
        stack_lower,
        stack_upper,
        unwind_stack_begin,
        memory_stack,
        stack_pointer,
    };
    wasix_call_asyncify_start_unwind(caller, stack_lower).await
}

#[derive(Clone, Copy)]
pub(super) struct WasixSpawnIo {
    pub(super) stdin: WasixStdioMode,
    pub(super) stdout: WasixStdioMode,
    pub(super) stderr: WasixStdioMode,
}

#[derive(Clone, Copy)]
pub(super) enum WasixStdioMode {
    Piped,
    Inherit,
    Null,
    Log,
    Invalid,
}

pub(super) struct WasixSpawnResult {
    pub(super) pid: u32,
    pub(super) stdin_fd: Option<u32>,
    pub(super) stdout_fd: Option<u32>,
    pub(super) stderr_fd: Option<u32>,
}

pub(super) struct WasixSpawnPreparedIo {
    pub(super) output_mode: OutputMode,
    pub(super) stdin_writer: Option<crate::ByteWriter>,
    pub(super) stdout_reader: Option<crate::ByteReader>,
    pub(super) stderr_reader: Option<crate::ByteReader>,
}

impl WasixSpawnIo {
    pub(super) const fn inherit() -> Self {
        Self {
            stdin: WasixStdioMode::Inherit,
            stdout: WasixStdioMode::Inherit,
            stderr: WasixStdioMode::Inherit,
        }
    }

    pub(super) const fn from_modes(stdin: i32, stdout: i32, stderr: i32) -> Self {
        Self {
            stdin: WasixStdioMode::from_raw(stdin),
            stdout: WasixStdioMode::from_raw(stdout),
            stderr: WasixStdioMode::from_raw(stderr),
        }
    }
}

pub(super) fn wasix_prepare_child_io<CpuImpl, HostFs>(
    store: &Preview1ProgramStore<CpuImpl, HostFs>,
    io: WasixSpawnIo,
) -> Result<WasixSpawnPreparedIo, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if matches!(
        (io.stdin, io.stdout, io.stderr),
        (WasixStdioMode::Invalid, _, _)
            | (_, WasixStdioMode::Invalid, _)
            | (_, _, WasixStdioMode::Invalid)
    ) {
        return Err(p1::errno::INVAL);
    }

    let (stdin_writer, stdin_reader) = crate::byte_channel();
    let stdin_writer = match io.stdin {
        WasixStdioMode::Piped => Some(stdin_writer),
        WasixStdioMode::Inherit | WasixStdioMode::Null | WasixStdioMode::Log => None,
        WasixStdioMode::Invalid => unreachable!("invalid stdio mode already rejected"),
    };
    let (stdout, stdout_reader) = wasix_prepare_child_output_route(
        store,
        io.stdout,
        crate::ComponentOutputStreamKind::Stdout,
    )?;
    let (stderr, stderr_reader) = wasix_prepare_child_output_route(
        store,
        io.stderr,
        crate::ComponentOutputStreamKind::Stderr,
    )?;
    Ok(WasixSpawnPreparedIo {
        output_mode: OutputMode::RoutedChild {
            stdin_rx: stdin_reader,
            stdout,
            stderr,
        },
        stdin_writer,
        stdout_reader,
        stderr_reader,
    })
}

pub(super) fn wasix_prepare_child_output_route<CpuImpl, HostFs>(
    store: &Preview1ProgramStore<CpuImpl, HostFs>,
    mode: WasixStdioMode,
    stream: crate::ComponentOutputStreamKind,
) -> Result<(OutputRoute, Option<crate::ByteReader>), i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    match mode {
        WasixStdioMode::Piped => {
            let (writer, reader) = crate::byte_channel();
            Ok((OutputRoute::Child(writer), Some(reader)))
        }
        WasixStdioMode::Inherit | WasixStdioMode::Log => Ok((store.output_route(stream), None)),
        WasixStdioMode::Null => Ok((OutputRoute::Discard, None)),
        WasixStdioMode::Invalid => Err(p1::errno::INVAL),
    }
}

impl WasixStdioMode {
    pub(super) const fn from_raw(value: i32) -> Self {
        match value {
            WASIX_STDIO_MODE_PIPED => Self::Piped,
            WASIX_STDIO_MODE_INHERIT => Self::Inherit,
            WASIX_STDIO_MODE_NULL => Self::Null,
            WASIX_STDIO_MODE_LOG => Self::Log,
            _ => Self::Invalid,
        }
    }
}

pub(super) async fn wasix_prepare_program<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    memory: Preview1Memory,
    name: u32,
    name_len: u32,
) -> wasmtime::Result<WasixPreparedProgram>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let name =
        wasix_read_exec_string(caller, memory, name, name_len).map_err(wasmtime::Error::new)?;
    wasix_prepare_program_from_name(caller, &name).await
}

pub(super) async fn wasix_prepare_program_with_search<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    memory: Preview1Memory,
    name: u32,
    name_len: u32,
    search_path: i32,
    path: u32,
    path_len: u32,
) -> wasmtime::Result<WasixPreparedProgram>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let name =
        wasix_read_exec_string(caller, memory, name, name_len).map_err(wasmtime::Error::new)?;
    let search_path = match search_path {
        0 => false,
        1 => true,
        _ => {
            return Err(wasmtime::Error::new(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidPath,
                detail: ProgramExecErrorDetail::InvalidProgramPath,
            }));
        }
    };
    if !search_path || name.contains('/') {
        return wasix_prepare_program_from_name(caller, &name).await;
    }

    if path == 0 && path_len == 0 {
        return wasix_prepare_program_from_search_paths(
            caller,
            &name,
            WasixExecSearchPath::Default,
        )
        .await;
    }

    let path =
        wasix_read_exec_string(caller, memory, path, path_len).map_err(wasmtime::Error::new)?;
    wasix_prepare_program_from_search_paths(caller, &name, WasixExecSearchPath::Guest(&path)).await
}

pub(super) async fn wasix_prepare_program_from_search_paths<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    name: &str,
    search_path: WasixExecSearchPath<'_>,
) -> wasmtime::Result<WasixPreparedProgram>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    match search_path {
        WasixExecSearchPath::Default => {
            for directory in DEFAULT_WASIX_EXEC_SEARCH_PATHS {
                if let Some(prepared) =
                    wasix_prepare_program_from_search_directory(caller, name, directory).await
                {
                    return Ok(prepared);
                }
            }
        }
        WasixExecSearchPath::Guest(path) => {
            for directory in path.split(':') {
                if let Some(prepared) =
                    wasix_prepare_program_from_search_directory(caller, name, directory).await
                {
                    return Ok(prepared);
                }
            }
        }
    }

    Err(wasmtime::Error::new(ProgramExecError {
        kind: ProgramExecErrorKind::MissingEntry,
        detail: ProgramExecErrorDetail::MissingArtifactPayload,
    }))
}

pub(super) async fn wasix_prepare_program_from_search_directory<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    name: &str,
    directory: &str,
) -> Option<WasixPreparedProgram>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let cwd = caller
        .data()
        .cwd
        .as_ref()
        .map(|cwd| cwd.guest_name.as_str());
    let candidate = wasix_search_path_candidate(cwd, directory, name)?;
    wasix_prepare_program_from_guest_name(caller, candidate)
        .await
        .ok()
}

pub(super) async fn wasix_spawn_descriptor_snapshot<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    memory: Preview1Memory,
    fd_ops: u32,
    fd_ops_len: u32,
) -> Result<WasixSpawnFdSnapshot, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if fd_ops == 0 {
        return Err(p1::errno::FAULT);
    }
    let mut snapshot = WasixSpawnFdSnapshot {
        descriptors: caller.data().descriptors.clone(),
        authority: caller.data().authority.clone(),
        cwd: caller.data().cwd.clone(),
    };
    for index in 0..fd_ops_len {
        let offset = index
            .checked_mul(WASIX_PROC_SPAWN_FD_OP_SIZE)
            .and_then(|offset| fd_ops.checked_add(offset))
            .ok_or(p1::errno::OVERFLOW)?;
        wasix_apply_spawn_fd_op(caller, memory, &mut snapshot, offset).await?;
    }
    Ok(snapshot)
}

pub(super) async fn wasix_apply_spawn_fd_op<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    memory: Preview1Memory,
    snapshot: &mut WasixSpawnFdSnapshot,
    op: u32,
) -> Result<(), i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let cmd = p1_try_read_u8(caller, memory, op + WASIX_PROC_SPAWN_FD_OP_CMD_OFFSET)
        .map_err(|_| p1::errno::FAULT)?;
    let fd = p1_try_read_u32(caller, memory, op + WASIX_PROC_SPAWN_FD_OP_FD_OFFSET)
        .map_err(|_| p1::errno::FAULT)?;
    let fd = i32::try_from(fd).map_err(|_| p1::errno::OVERFLOW)?;
    match cmd {
        WASIX_PROC_SPAWN_FD_OP_CLOSE => {
            let status = snapshot.descriptors.close(fd);
            if status == p1::errno::SUCCESS {
                Ok(())
            } else {
                Err(status)
            }
        }
        WASIX_PROC_SPAWN_FD_OP_DUP2 => {
            let source_fd =
                p1_try_read_u32(caller, memory, op + WASIX_PROC_SPAWN_FD_OP_SRC_FD_OFFSET)
                    .map_err(|_| p1::errno::FAULT)?;
            let source_fd = i32::try_from(source_fd).map_err(|_| p1::errno::OVERFLOW)?;
            let fdflagsext = p1_try_read_u16(
                caller,
                memory,
                op + WASIX_PROC_SPAWN_FD_OP_FDFLAGSEXT_OFFSET,
            )
            .map_err(|_| p1::errno::FAULT)?;
            let close_on_exec = wasix_close_on_exec_flag(fdflagsext)?;
            snapshot
                .descriptors
                .dup_to(source_fd, fd, close_on_exec)
                .map(drop)
        }
        WASIX_PROC_SPAWN_FD_OP_CHDIR => {
            let path_ptr = p1_try_read_u32(caller, memory, op + WASIX_PROC_SPAWN_FD_OP_PATH_OFFSET)
                .map_err(|_| p1::errno::FAULT)?;
            let path_len =
                p1_try_read_u32(caller, memory, op + WASIX_PROC_SPAWN_FD_OP_PATH_LEN_OFFSET)
                    .map_err(|_| p1::errno::FAULT)?;
            let path = wasix_read_exec_string(caller, memory, path_ptr, path_len)
                .map_err(|_| p1::errno::FAULT)?;
            wasix_apply_spawn_chdir(caller.data(), snapshot, fd, &path)
        }
        WASIX_PROC_SPAWN_FD_OP_FCHDIR => wasix_apply_spawn_fchdir(snapshot, fd),
        WASIX_PROC_SPAWN_FD_OP_OPEN => {
            let source_fd =
                p1_try_read_u32(caller, memory, op + WASIX_PROC_SPAWN_FD_OP_SRC_FD_OFFSET)
                    .map_err(|_| p1::errno::FAULT)?;
            let source_fd = i32::try_from(source_fd).map_err(|_| p1::errno::OVERFLOW)?;
            let path_ptr = p1_try_read_u32(caller, memory, op + WASIX_PROC_SPAWN_FD_OP_PATH_OFFSET)
                .map_err(|_| p1::errno::FAULT)?;
            let path_len =
                p1_try_read_u32(caller, memory, op + WASIX_PROC_SPAWN_FD_OP_PATH_LEN_OFFSET)
                    .map_err(|_| p1::errno::FAULT)?;
            let dirflags =
                p1_try_read_u32(caller, memory, op + WASIX_PROC_SPAWN_FD_OP_DIRFLAGS_OFFSET)
                    .map_err(|_| p1::errno::FAULT)?;
            let oflags = p1_try_read_u16(caller, memory, op + WASIX_PROC_SPAWN_FD_OP_OFLAGS_OFFSET)
                .map_err(|_| p1::errno::FAULT)?;
            let rights = p1_try_read_u64(
                caller,
                memory,
                op + WASIX_PROC_SPAWN_FD_OP_RIGHTS_BASE_OFFSET,
            )
            .map_err(|_| p1::errno::FAULT)?;
            let fdflags =
                p1_try_read_u16(caller, memory, op + WASIX_PROC_SPAWN_FD_OP_FDFLAGS_OFFSET)
                    .map_err(|_| p1::errno::FAULT)?;
            let fdflagsext = p1_try_read_u16(
                caller,
                memory,
                op + WASIX_PROC_SPAWN_FD_OP_FDFLAGSEXT_OFFSET,
            )
            .map_err(|_| p1::errno::FAULT)?;
            let close_on_exec = wasix_close_on_exec_flag(fdflagsext)?;
            let path =
                p1_read_path(caller, memory, path_ptr, path_len).map_err(|_| p1::errno::FAULT)?;
            wasix_apply_spawn_open(
                caller,
                snapshot,
                fd,
                source_fd,
                &path,
                dirflags,
                oflags,
                rights,
                fdflags,
                close_on_exec,
            )
            .await
        }
        _ => Err(p1::errno::INVAL),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn wasix_apply_spawn_open<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    snapshot: &mut WasixSpawnFdSnapshot,
    fd: i32,
    source_fd: i32,
    path: &str,
    dirflags: u32,
    oflags: u16,
    rights: u64,
    fdflags: u16,
    close_on_exec: bool,
) -> Result<(), i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if !p1_file_fdflags_supported(fdflags) {
        return Err(p1::errno::INVAL);
    }
    let (base, relative_path) = wasix_spawn_resolve_open_base(snapshot, source_fd, path)?;
    let descriptor = p1_open_descriptor_resolved(
        caller,
        &base,
        p1_path_flags(dirflags),
        &relative_path,
        p1_open_flags(oflags),
        p1_descriptor_flags(rights, fdflags),
    )
    .await?;
    snapshot
        .descriptors
        .insert_at(
            fd,
            Preview1Descriptor::File {
                descriptor,
                offset: 0,
                fdflags,
            },
            close_on_exec,
        )
        .map(drop)
}

pub(super) fn wasix_spawn_resolve_open_base(
    snapshot: &WasixSpawnFdSnapshot,
    source_fd: i32,
    path: &str,
) -> Result<(FsDescriptor, String), i32> {
    if path.starts_with('/') {
        let guest_name =
            crate::resolve_absolute_path(path).map_err(p1_errno_from_component_path)?;
        return wasix_spawn_resolve_absolute_guest_base(snapshot, &guest_name);
    }
    if let Some(cwd) = snapshot.cwd.as_ref() {
        return Ok((cwd.descriptor.clone(), path.to_owned()));
    }
    let base = match snapshot.descriptors.get(source_fd) {
        Some(Preview1Descriptor::Preopen { descriptor, .. })
        | Some(Preview1Descriptor::File { descriptor, .. })
            if descriptor.kind == FsNodeKind::Directory =>
        {
            descriptor.clone()
        }
        Some(_) => return Err(p1::errno::NOTDIR),
        None => return Err(p1::errno::BADF),
    };
    Ok((base, path.to_owned()))
}

pub(super) fn wasix_apply_spawn_chdir<CpuImpl, HostFs>(
    store: &Preview1ProgramStore<CpuImpl, HostFs>,
    snapshot: &mut WasixSpawnFdSnapshot,
    fd: i32,
    path: &str,
) -> Result<(), i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let cwd = wasix_spawn_resolve_cwd_target(store, snapshot, fd, path)?;
    let cap = snapshot
        .authority
        .derive_directory_cap(
            &cwd.descriptor.path,
            &cwd.guest_name,
            descriptor_flags_to_directory_authority(cwd.descriptor.flags),
        )
        .map_err(|_| p1::errno::NOTCAPABLE)?;
    snapshot.authority.chdir(cap);
    snapshot.cwd = Some(cwd);
    Ok(())
}

pub(super) fn wasix_apply_spawn_fchdir(
    snapshot: &mut WasixSpawnFdSnapshot,
    fd: i32,
) -> Result<(), i32> {
    let descriptor = match snapshot.descriptors.get(fd) {
        Some(Preview1Descriptor::Preopen {
            guest_name,
            descriptor,
        }) if descriptor.kind == FsNodeKind::Directory => Preview1Cwd {
            guest_name: guest_name.clone(),
            descriptor: descriptor.clone(),
        },
        Some(Preview1Descriptor::File { descriptor, .. })
            if descriptor.kind == FsNodeKind::Directory =>
        {
            let guest_name = wasix_spawn_guest_name_for_source(snapshot, &descriptor.path)?;
            Preview1Cwd {
                guest_name,
                descriptor: descriptor.clone(),
            }
        }
        Some(_) => return Err(p1::errno::NOTDIR),
        None => return Err(p1::errno::BADF),
    };
    let cap = snapshot
        .authority
        .derive_directory_cap(
            &descriptor.descriptor.path,
            &descriptor.guest_name,
            descriptor_flags_to_directory_authority(descriptor.descriptor.flags),
        )
        .map_err(|_| p1::errno::NOTCAPABLE)?;
    snapshot.authority.chdir(cap);
    snapshot.cwd = Some(descriptor);
    Ok(())
}

pub(super) fn wasix_spawn_resolve_cwd_target<CpuImpl, HostFs>(
    store: &Preview1ProgramStore<CpuImpl, HostFs>,
    snapshot: &WasixSpawnFdSnapshot,
    fd: i32,
    path: &str,
) -> Result<Preview1Cwd, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let (guest_name, source_path, flags) = if path.starts_with('/') {
        let guest_name =
            crate::resolve_absolute_path(path).map_err(p1_errno_from_component_path)?;
        let (descriptor, suffix) = wasix_spawn_resolve_absolute_guest_base(snapshot, &guest_name)?;
        let source_path = if suffix.is_empty() {
            descriptor.path
        } else {
            crate::resolve_child_path(&descriptor.path, &suffix)
                .map_err(p1_errno_from_component_path)?
        };
        (guest_name, source_path, descriptor.flags)
    } else {
        let base = match snapshot.descriptors.get(fd) {
            Some(Preview1Descriptor::Preopen { descriptor, .. })
            | Some(Preview1Descriptor::File { descriptor, .. })
                if descriptor.kind == FsNodeKind::Directory =>
            {
                descriptor.clone()
            }
            Some(_) => return Err(p1::errno::NOTDIR),
            None => return Err(p1::errno::BADF),
        };
        let base_guest = wasix_spawn_guest_name_for_source(snapshot, &base.path)?;
        let guest_name =
            crate::resolve_child_path(&base_guest, path).map_err(p1_errno_from_component_path)?;
        let source_path =
            crate::resolve_child_path(&base.path, path).map_err(p1_errno_from_component_path)?;
        (guest_name, source_path, base.flags)
    };

    if !flags.contains(fs_types::DescriptorFlags::READ) {
        return Err(p1::errno::NOTCAPABLE);
    }
    let stat = store
        .filesystem
        .stat(&source_path)
        .map_err(p1_errno_from_fs)?;
    if !matches!(stat.type_, fs_types::DescriptorType::Directory) {
        return Err(p1::errno::NOTDIR);
    }
    Ok(Preview1Cwd {
        guest_name,
        descriptor: FsDescriptor {
            path: source_path,
            kind: FsNodeKind::Directory,
            flags,
            identity: None,
        },
    })
}

pub(super) fn wasix_spawn_guest_name_for_source(
    snapshot: &WasixSpawnFdSnapshot,
    source_path: &str,
) -> Result<String, i32> {
    let mut best: Option<(&str, &str)> = None;
    for entry in &snapshot.descriptors.entries {
        let Some(entry) = entry else {
            continue;
        };
        let Preview1Descriptor::Preopen {
            guest_name,
            descriptor,
        } = &entry.descriptor
        else {
            continue;
        };
        if !guest_path_is_within_preopen(source_path, &descriptor.path) {
            continue;
        }
        if best.is_none_or(|(_, best_source)| descriptor.path.len() > best_source.len()) {
            best = Some((guest_name.as_str(), descriptor.path.as_str()));
        }
    }

    let cwd_source;
    if let Some(cwd) = snapshot.cwd.as_ref()
        && guest_path_is_within_preopen(source_path, &cwd.descriptor.path)
        && best.is_none_or(|(_, best_source)| cwd.descriptor.path.len() > best_source.len())
    {
        cwd_source = cwd.descriptor.path.clone();
        best = Some((cwd.guest_name.as_str(), cwd_source.as_str()));
    }

    let Some((guest_base, source_base)) = best else {
        return Err(p1::errno::NOTCAPABLE);
    };
    let suffix = guest_path_suffix(source_path, source_base);
    if suffix.is_empty() {
        Ok(guest_base.to_owned())
    } else {
        crate::resolve_child_path(guest_base, suffix).map_err(p1_errno_from_component_path)
    }
}

pub(super) fn wasix_spawn_resolve_absolute_guest_base(
    snapshot: &WasixSpawnFdSnapshot,
    guest_name: &str,
) -> Result<(FsDescriptor, String), i32> {
    let mut best: Option<(&str, &FsDescriptor)> = None;
    for entry in &snapshot.descriptors.entries {
        let Some(entry) = entry else {
            continue;
        };
        let Preview1Descriptor::Preopen {
            guest_name: preopen_guest,
            descriptor,
        } = &entry.descriptor
        else {
            continue;
        };
        if !guest_path_is_within_preopen(guest_name, preopen_guest) {
            continue;
        }
        if best.is_none_or(|(best_guest, _)| preopen_guest.len() > best_guest.len()) {
            best = Some((preopen_guest.as_str(), descriptor));
        }
    }

    let cwd_descriptor;
    if let Some(cwd) = snapshot.cwd.as_ref()
        && guest_path_is_within_preopen(guest_name, &cwd.guest_name)
        && best.is_none_or(|(best_guest, _)| cwd.guest_name.len() > best_guest.len())
    {
        cwd_descriptor = cwd.descriptor.clone();
        best = Some((cwd.guest_name.as_str(), &cwd_descriptor));
    }

    let Some((preopen_guest, descriptor)) = best else {
        return Err(p1::errno::NOTCAPABLE);
    };
    let suffix = guest_path_suffix(guest_name, preopen_guest);
    Ok((descriptor.clone(), suffix.to_owned()))
}

pub(super) async fn wasix_prepare_program_from_name<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    name: &str,
) -> wasmtime::Result<WasixPreparedProgram>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let guest_name = wasix_resolve_exec_guest_name(caller.data(), name)?;
    wasix_prepare_program_from_guest_name(caller, guest_name).await
}

pub(super) async fn wasix_prepare_program_from_guest_name<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    guest_name: String,
) -> wasmtime::Result<WasixPreparedProgram>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let source_path = wasix_resolve_exec_source_path(caller.data(), &guest_name)?;
    let source = wasix_read_program_source(caller, &source_path).await?;
    Ok(WasixPreparedProgram { guest_name, source })
}

pub(super) async fn wasix_read_program_source<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    source_path: &str,
) -> wasmtime::Result<ProgramSource>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let host_path = crate::guest_host_share_path(source_path).map(ToOwned::to_owned);
    let source_is_host = host_path.is_some();
    let source = if let Some(host_path) = host_path {
        let service = caller
            .data()
            .runtime_state
            .host_filesystem_service()
            .ok_or_else(|| {
                wasmtime::Error::new(ProgramExecError {
                    kind: ProgramExecErrorKind::PermissionDenied,
                    detail: ProgramExecErrorDetail::ProgramSourceNotGranted,
                })
            })?;
        service
            .read_file(&host_path)
            .await
            .map(Bytes::from)
            .map_err(|_| {
                wasmtime::Error::new(ProgramExecError {
                    kind: ProgramExecErrorKind::PermissionDenied,
                    detail: ProgramExecErrorDetail::ProgramSourceNotGranted,
                })
            })?
    } else {
        caller
            .data()
            .filesystem
            .read_program_file_bytes(source_path)
            .map_err(|_| {
                wasmtime::Error::new(ProgramExecError {
                    kind: ProgramExecErrorKind::PermissionDenied,
                    detail: ProgramExecErrorDetail::ProgramSourceNotGranted,
                })
            })?
    };
    let source = if cwasm::is_cwasm(&source) {
        if source_is_host {
            ProgramSource::SignedArtifact(source)
        } else {
            ProgramSource::BootfsArtifact(source)
        }
    } else {
        ProgramSource::RawWasm(source)
    };
    Ok(source)
}

pub(super) fn wasix_resolve_exec_guest_name<CpuImpl, HostFs>(
    store: &Preview1ProgramStore<CpuImpl, HostFs>,
    name: &str,
) -> wasmtime::Result<String>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if name.starts_with('/') {
        return crate::resolve_absolute_path(name).map_err(|_| {
            wasmtime::Error::new(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidPath,
                detail: ProgramExecErrorDetail::InvalidProgramPath,
            })
        });
    }

    let Some(cwd) = store.cwd.as_ref() else {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::PermissionDenied,
            detail: ProgramExecErrorDetail::ProgramSourceNotGranted,
        }));
    };
    crate::resolve_child_path(&cwd.guest_name, name).map_err(|_| {
        wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidPath,
            detail: ProgramExecErrorDetail::InvalidProgramPath,
        })
    })
}

pub(super) fn wasix_resolve_exec_source_path<CpuImpl, HostFs>(
    store: &Preview1ProgramStore<CpuImpl, HostFs>,
    guest_name: &str,
) -> wasmtime::Result<String>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let (path, _) = store.resolve_absolute_guest_path(guest_name).map_err(|_| {
        wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::PermissionDenied,
            detail: ProgramExecErrorDetail::ProgramSourceNotGranted,
        })
    })?;
    if !store.authority.can_load_program(&path) {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::PermissionDenied,
            detail: ProgramExecErrorDetail::ProgramSourceNotGranted,
        }));
    }
    Ok(path)
}

pub(super) fn wasix_search_path_candidate(
    cwd: Option<&str>,
    directory: &str,
    name: &str,
) -> Option<String> {
    let directory = if directory.is_empty() {
        cwd?.to_owned()
    } else if directory.starts_with('/') {
        crate::resolve_absolute_path(directory).ok()?
    } else {
        crate::resolve_child_path(cwd?, directory).ok()?
    };
    crate::resolve_child_path(&directory, name).ok()
}

pub(super) async fn wasix_spawn_child<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    prepared: WasixPreparedProgram,
    argv: Vec<String>,
    io: WasixSpawnIo,
    inheritance: WasixChildInheritance,
) -> Result<WasixSpawnResult, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let WasixChildInheritance {
        environment,
        authority,
        descriptors,
        signal_dispositions,
    } = inheritance;
    let prepared_io = wasix_prepare_child_io(caller.data(), io)?;
    let argv = ProgramArgv::from_caller(&prepared.guest_name, argv);
    let mut environment = environment.unwrap_or_else(|| caller.data().environment.clone());
    environment.retain(|(name, _)| name.as_str() != HELIOS_PROCESS_ID_ENV);
    let runtime_state = caller.data().runtime_state.clone();
    let service = runtime_state.wait_for_program_service().await;
    let exec_context = caller.data().exec_context();
    let authority = authority.unwrap_or_else(|| caller.data().authority.clone());
    let filesystem = Some(caller.data().filesystem.snapshot());
    let launch_started = p1_kernel_profile_start(caller.data());
    let mut child = service
        .spawn_with_output_mode(
            exec_context,
            prepared.source,
            None,
            ProgramLaunch {
                argv,
                env: environment,
                authority,
                filesystem,
                descriptors,
                signal_dispositions,
            },
            prepared_io.output_mode,
            ChildStdio {
                stdin: prepared_io.stdin_writer,
                stdout: prepared_io.stdout_reader,
                stderr: prepared_io.stderr_reader,
            },
        )
        .await
        .map_err(|error| p1_errno_from_program_exec_error(&error))?;
    p1_record_optional_kernel_profile(caller.data(), "proc_spawn_child_launch", launch_started);
    let pid = u32::try_from(child.instance_id.raw()).map_err(|_| p1::errno::OVERFLOW)?;
    let configure_started = p1_kernel_profile_start(caller.data());
    let stdin_fd = wasix_insert_child_stdin(caller, &mut child)?;
    let stdout_fd =
        wasix_insert_child_output(caller, &mut child, crate::ComponentOutputStreamKind::Stdout)?;
    let stderr_fd =
        wasix_insert_child_output(caller, &mut child, crate::ComponentOutputStreamKind::Stderr)?;
    p1_record_optional_kernel_profile(caller.data(), "proc_spawn_configure_io", configure_started);
    let exit = child.take_wait().ok_or(p1::errno::IO)?;
    caller
        .data_mut()
        .insert_child(pid, child.signal_state(), exit);
    Ok(WasixSpawnResult {
        pid,
        stdin_fd,
        stdout_fd,
        stderr_fd,
    })
}

pub(super) fn wasix_insert_child_stdin<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    child: &mut ChildHandle,
) -> Result<Option<u32>, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(writer) = child.take_stdin() else {
        return Ok(None);
    };
    caller
        .data_mut()
        .descriptors
        .insert(Preview1Descriptor::PipeWrite { writer })
        .map(Some)
}

pub(super) fn wasix_insert_child_output<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    child: &mut ChildHandle,
    stream: crate::ComponentOutputStreamKind,
) -> Result<Option<u32>, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let reader = match stream {
        crate::ComponentOutputStreamKind::Stdout => child.take_stdout(),
        crate::ComponentOutputStreamKind::Stderr => child.take_stderr(),
    };
    let Some(reader) = reader else {
        return Ok(None);
    };
    caller
        .data_mut()
        .descriptors
        .insert(Preview1Descriptor::PipeRead {
            reader,
            carry: Bytes::new(),
        })
        .map(Some)
}

pub(super) fn wasix_write_process_handles<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    memory: Preview1Memory,
    ret_handles: u32,
    result: WasixSpawnResult,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    p1_write_u32(caller, memory, ret_handles, result.pid)
        .max(wasix_write_option_fd(
            caller,
            memory,
            ret_handles + WASIX_PROCESS_HANDLES_STDIN_OFFSET,
            result.stdin_fd,
        ))
        .max(wasix_write_option_fd(
            caller,
            memory,
            ret_handles + WASIX_PROCESS_HANDLES_STDOUT_OFFSET,
            result.stdout_fd,
        ))
        .max(wasix_write_option_fd(
            caller,
            memory,
            ret_handles + WASIX_PROCESS_HANDLES_STDERR_OFFSET,
            result.stderr_fd,
        ))
}

pub(super) fn wasix_write_option_fd<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    fd: Option<u32>,
) -> i32 {
    let status = p1_write_u8(
        caller,
        memory,
        ptr,
        if fd.is_some() {
            WASIX_OPTION_SOME
        } else {
            WASIX_OPTION_NONE
        },
    );
    if status != p1::errno::SUCCESS {
        return status;
    }
    p1_write_u32(
        caller,
        memory,
        ptr + WASIX_OPTION_UNION_U32_OFFSET,
        fd.unwrap_or(0),
    )
}

pub(super) fn wasix_write_join_nothing<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    memory: Preview1Memory,
    ret_status: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    p1_write_u8(caller, memory, ret_status, WASIX_JOIN_STATUS_NOTHING).max(p1_write_u16(
        caller,
        memory,
        ret_status + WASIX_JOIN_STATUS_UNION_OFFSET,
        0,
    ))
}

pub(super) fn wasix_write_join_exit<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    memory: Preview1Memory,
    pid_ptr: u32,
    ret_status: u32,
    pid: u32,
    code: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let errno = u16::try_from(code).unwrap_or(u16::MAX);
    p1_write_u8(caller, memory, pid_ptr, WASIX_OPTION_SOME)
        .max(p1_write_u32(
            caller,
            memory,
            pid_ptr + WASIX_OPTION_UNION_U32_OFFSET,
            pid,
        ))
        .max(p1_write_u8(
            caller,
            memory,
            ret_status,
            WASIX_JOIN_STATUS_EXIT_NORMAL,
        ))
        .max(p1_write_u16(
            caller,
            memory,
            ret_status + WASIX_JOIN_STATUS_UNION_OFFSET,
            errno,
        ))
}

pub(super) fn wasix_read_exec_string<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    len: u32,
) -> Result<String, ProgramExecError> {
    if len == 0 {
        return Ok(String::new());
    }
    p1_read_memory(caller, memory, ptr, len as usize).and_then(|mut bytes| {
        while bytes.last().is_some_and(|byte| *byte == 0) {
            bytes.pop();
        }
        String::from_utf8(bytes).map_err(|_| ProgramExecError {
            kind: ProgramExecErrorKind::InvalidPath,
            detail: ProgramExecErrorDetail::InvalidProgramPathEncoding,
        })
    })
}

pub(super) fn wasix_split_lines(value: &str) -> Vec<String> {
    let mut lines = Vec::with_capacity(wasix_nonempty_line_count(value));
    lines.extend(
        value
            .split('\n')
            .filter(|entry| !entry.is_empty())
            .map(ToOwned::to_owned),
    );
    lines
}

pub(super) fn wasix_split_environment(value: &str) -> Vec<(String, String)> {
    let mut environment = Vec::with_capacity(wasix_nonempty_line_count(value));
    environment.extend(
        value
            .split('\n')
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                entry
                    .split_once('=')
                    .map(|(name, value)| (name.to_owned(), value.to_owned()))
                    .unwrap_or_else(|| (entry.to_owned(), String::new()))
            }),
    );
    environment
}

pub(super) fn wasix_nonempty_line_count(value: &str) -> usize {
    value.split('\n').filter(|entry| !entry.is_empty()).count()
}

pub(super) fn wasix_read_signal_dispositions<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    len: u32,
) -> Result<Vec<WasixSignalDisposition>, i32> {
    if len == 0 {
        if ptr == 0 {
            return Ok(Vec::new());
        }
        return Err(p1::errno::INVAL);
    }
    if ptr == 0 {
        return Err(p1::errno::FAULT);
    }
    let mut dispositions = Vec::new();
    for index in 0..len {
        let offset = index
            .checked_mul(WASIX_SIGNAL_DISPOSITION_SIZE)
            .and_then(|offset| ptr.checked_add(offset))
            .ok_or(p1::errno::OVERFLOW)?;
        let signal = p1_try_read_u8(
            caller,
            memory,
            offset + WASIX_SIGNAL_DISPOSITION_SIGNAL_OFFSET,
        )
        .map_err(|_| p1::errno::FAULT)?;
        let action = p1_try_read_u8(
            caller,
            memory,
            offset + WASIX_SIGNAL_DISPOSITION_ACTION_OFFSET,
        )
        .map_err(|_| p1::errno::FAULT)?;
        dispositions.push(wasix_signal_disposition_from_raw(signal, action)?);
    }
    Ok(dispositions)
}

pub(super) fn wasix_signal_disposition_from_raw(
    signal: u8,
    action: u8,
) -> Result<WasixSignalDisposition, i32> {
    if !(1..=31).contains(&signal) {
        return Err(p1::errno::INVAL);
    }
    let action = match action {
        WASIX_SIGNAL_DISPOSITION_DEFAULT => WasixSignalDispositionAction::Default,
        WASIX_SIGNAL_DISPOSITION_IGNORE => WasixSignalDispositionAction::Ignore,
        _ => return Err(p1::errno::INVAL),
    };
    Ok(WasixSignalDisposition { signal, action })
}

pub(super) fn read_wasix_tty_state<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> Result<WasixTtyState, i32> {
    Ok(WasixTtyState {
        cols: p1_try_read_u32(caller, memory, ptr).map_err(|_| p1::errno::FAULT)?,
        rows: p1_try_read_u32(caller, memory, ptr + 4).map_err(|_| p1::errno::FAULT)?,
        width: p1_try_read_u32(caller, memory, ptr + 8).map_err(|_| p1::errno::FAULT)?,
        height: p1_try_read_u32(caller, memory, ptr + 12).map_err(|_| p1::errno::FAULT)?,
        stdin_tty: p1_read_wasix_bool(caller, memory, ptr + 16)?,
        stdout_tty: p1_read_wasix_bool(caller, memory, ptr + 17)?,
        stderr_tty: p1_read_wasix_bool(caller, memory, ptr + 18)?,
        echo: p1_read_wasix_bool(caller, memory, ptr + 19)?,
        line_buffered: p1_read_wasix_bool(caller, memory, ptr + 20)?,
        line_feeds: p1_read_wasix_bool(caller, memory, ptr + 21)?,
    })
}

pub(super) fn write_wasix_tty_state<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    state: WasixTtyState,
) -> i32 {
    p1_write_u32(caller, memory, ptr, state.cols)
        .max(p1_write_u32(caller, memory, ptr + 4, state.rows))
        .max(p1_write_u32(caller, memory, ptr + 8, state.width))
        .max(p1_write_u32(caller, memory, ptr + 12, state.height))
        .max(p1_write_wasix_bool(
            caller,
            memory,
            ptr + 16,
            state.stdin_tty,
        ))
        .max(p1_write_wasix_bool(
            caller,
            memory,
            ptr + 17,
            state.stdout_tty,
        ))
        .max(p1_write_wasix_bool(
            caller,
            memory,
            ptr + 18,
            state.stderr_tty,
        ))
        .max(p1_write_wasix_bool(caller, memory, ptr + 19, state.echo))
        .max(p1_write_wasix_bool(
            caller,
            memory,
            ptr + 20,
            state.line_buffered,
        ))
        .max(p1_write_wasix_bool(
            caller,
            memory,
            ptr + 21,
            state.line_feeds,
        ))
}
