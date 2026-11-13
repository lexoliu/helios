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
pub(super) struct WasixChildInheritance<Net: ComponentHostNetwork> {
    pub(super) environment: Option<Vec<(String, String)>>,
    pub(super) authority: Option<ProcessAuthority>,
    pub(super) descriptors: Option<Preview1DescriptorTable<Net>>,
    pub(super) signal_dispositions: Vec<WasixSignalDisposition>,
}

pub(super) struct WasixPreparedProgram {
    pub(super) guest_name: String,
    pub(super) source: ProgramSource,
}

pub(super) struct WasixSpawnFdSnapshot<Net: ComponentHostNetwork> {
    pub(super) descriptors: Preview1DescriptorTable<Net>,
    pub(super) authority: ProcessAuthority,
    pub(super) cwd: Option<Preview1Cwd>,
}

pub(super) enum WasixExecSearchPath<'a> {
    Default,
    Guest(&'a str),
}

pub(super) fn add_wasix_extended_program_imports<CpuImpl, Net, HostFs>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, Net, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "thread_spawn_v2",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (_copy_memory, ret_pid): (i32, i32)| {
                Box::new(
                    async move { p1_reply(wasix_proc_fork(&mut caller, ret_pid as u32).await) },
                )
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "proc_exec",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
                    p1_reply(
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
                        .await,
                    )
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "proc_spawn2",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
                    p1_reply(
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
                        .await,
                    )
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "proc_join",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (pid, flags, ret_status): (i32, i32, i32)| {
                Box::new(async move {
                    p1_reply(
                        wasix_proc_join(&mut caller, pid as u32, flags as u32, ret_status as u32)
                            .await,
                    )
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "proc_snapshot",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>, (): ()| {
                Box::new(async move { p1_reply(wasix_proc_snapshot(&mut caller).await) })
            },
        )
        .map_err(map_program_runtime_error)?;
    add_wasix_port_imports(linker)?;
    add_wasix_socket_imports(linker)?;
    add_wasix_epoll_imports(linker)?;
    Ok(())
}

pub(super) fn add_wasix_program_imports<CpuImpl, Net, HostFs>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, Net, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap(
            WASIX_MODULE,
            "clock_time_set",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             clock_id: i32,
             timestamp: i64|
             -> i32 { wasix_clock_time_set(&mut caller, clock_id, timestamp) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_dup",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             fd: i32,
             ret_fd: i32|
             -> i32 { wasix_fd_dup(&mut caller, fd, ret_fd as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_dup2",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             ret_fd1: i32,
             ret_fd2: i32|
             -> i32 { wasix_fd_pipe(&mut caller, ret_fd1 as u32, ret_fd2 as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "tty_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             state: i32|
             -> i32 { wasix_tty_get(&mut caller, state as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "tty_set",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             state: i32|
             -> i32 { wasix_tty_set(&mut caller, state as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "getcwd",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             path: i32,
             path_len: i32|
             -> i32 { wasix_getcwd(&mut caller, path as u32, path_len as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "chdir",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (path, path_len): (i32, i32)| {
                Box::new(async move {
                    p1_reply(wasix_chdir(&mut caller, path as u32, path_len as u32).await)
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_event",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
                    p1_reply(
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
                        .await,
                    )
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_fdflags_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             fd: i32,
             ret_flags: i32|
             -> i32 { wasix_fd_fdflags_get(&mut caller, fd, ret_flags as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_fdflags_set",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             fd: i32,
             flags: i32|
             -> i32 { wasix_fd_fdflags_set(&mut caller, fd, flags as u16) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "callback_signal",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             ret_pid: i32|
             -> i32 { wasix_proc_id(&mut caller, ret_pid as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "proc_signal",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>, buf: i32| -> i32 {
                wasix_proc_signals_get(&mut caller, buf as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "proc_signals_sizes_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             ret_size: i32|
             -> i32 { wasix_proc_signals_sizes_get(&mut caller, ret_size as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "resolve",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (host, host_len, port, addrs, naddrs, ret_naddrs): (
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
            )| {
                Box::new(async move {
                    p1_reply(
                        wasix_resolve(
                            &mut caller,
                            host as u32,
                            host_len as u32,
                            port,
                            addrs as u32,
                            naddrs as u32,
                            ret_naddrs as u32,
                        )
                        .await,
                    )
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "proc_parent",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             pid: i32,
             ret_pid: i32|
             -> i32 { wasix_proc_parent(&mut caller, pid as u32, ret_pid as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "thread_sleep",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (duration,): (i64,)| {
                Box::new(async move { wasix_thread_sleep(&mut caller, duration).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "thread_id",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             ret_tid: i32|
             -> i32 { wasix_thread_id(&mut caller, ret_tid as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "thread_join",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>, (tid,): (i32,)| {
                Box::new(async move { wasix_thread_join(&mut caller, tid as u32).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "thread_parallelism",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             ret_parallelism: i32|
             -> i32 { wasix_thread_parallelism(&mut caller, ret_parallelism as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "thread_signal",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             tid: i32,
             signal: i32|
             -> wasmtime::Result<i32> { wasix_thread_signal(&mut caller, tid, signal) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "thread_exit",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             code: i32|
             -> wasmtime::Result<()> { wasix_thread_exit(&mut caller, code as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "futex_wait",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             futex: i32,
             ret_woken: i32|
             -> i32 { wasix_futex_wake(&mut caller, futex as u32, ret_woken as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "futex_wake_all",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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

pub(super) fn add_wasix_preview1_alias_imports<CpuImpl, Net, HostFs>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, Net, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap(
            WASIX_MODULE,
            "args_sizes_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             argv: i32,
             argv_buf: i32|
             -> i32 { p1_args_get(&mut caller, argv as u32, argv_buf as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "environ_sizes_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             count: i32,
             size: i32|
             -> i32 { p1_environ_sizes_get(&mut caller, count as u32, size as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "environ_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             environ: i32,
             environ_buf: i32|
             -> i32 { p1_environ_get(&mut caller, environ as u32, environ_buf as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "clock_time_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>, fd: i32| -> i32 {
                caller.data_mut().descriptors.close(fd)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_fdstat_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             fd: i32,
             stat: i32|
             -> i32 { p1_fd_fdstat_get(&mut caller, fd, stat as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_fdstat_set_flags",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             fd: i32,
             fdflags: i32|
             -> i32 { p1_fd_fdstat_set_flags(&mut caller, fd, fdflags as u16) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "fd_filestat_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, stat): (i32, i32)| {
                Box::new(
                    async move { p1_reply(p1_fd_filestat_get(&mut caller, fd, stat as u32).await) },
                )
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_prestat_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             fd: i32,
             buf: i32|
             -> i32 { p1_fd_prestat_get(&mut caller, fd, buf as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_prestat_dir_name",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, iovs, iovs_len, nread): (i32, i32, i32, i32)| {
                Box::new(async move {
                    p1_reply(
                        p1_fd_read(&mut caller, fd, iovs as u32, iovs_len as u32, nread as u32)
                            .await,
                    )
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "fd_readdir",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, buf, buf_len, cookie, bufused): (i32, i32, i32, i64, i32)| {
                Box::new(async move {
                    p1_reply(
                        p1_fd_readdir(
                            &mut caller,
                            fd,
                            buf as u32,
                            buf_len as u32,
                            cookie as u64,
                            bufused as u32,
                        )
                        .await,
                    )
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_renumber",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             from: i32,
             to: i32|
             -> i32 { p1_fd_renumber(&mut caller, from, to) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "fd_seek",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, offset, whence, new_offset): (i32, i64, i32, i32)| {
                Box::new(async move {
                    p1_reply(
                        p1_fd_seek(&mut caller, fd, offset, whence as u8, new_offset as u32).await,
                    )
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "fd_write",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, iovs, iovs_len, nwritten): (i32, i32, i32, i32)| {
                Box::new(async move {
                    p1_reply(
                        p1_fd_write(
                            &mut caller,
                            fd,
                            iovs as u32,
                            iovs_len as u32,
                            nwritten as u32,
                        )
                        .await,
                    )
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "path_filestat_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, flags, path, path_len, stat): (i32, i32, i32, i32, i32)| {
                Box::new(async move {
                    p1_reply(
                        wasix_path_filestat_get(
                            &mut caller,
                            fd,
                            flags as u32,
                            path as u32,
                            path_len as u32,
                            stat as u32,
                        )
                        .await,
                    )
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "path_open",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
                    p1_reply(
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
                        .await,
                    )
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sched_yield",
            |_caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>, ()| {
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
pub(super) async fn wasix_path_open<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    dirflags: u32,
    path: u32,
    path_len: u32,
    oflags: u16,
    rights: u64,
    fdflags: u16,
    opened_fd: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    let path = wasix_read_exec_string(&memory, path, path_len).await?;
    let (base, path) = match caller.data().resolve_wasix_path_base(fd, &path) {
        Ok(resolved) => resolved,
        Err(errno) => return Ok(errno),
    };
    let path_flags = p1_path_flags(dirflags);
    let open_flags = p1_open_flags(oflags);
    if !p1_file_fdflags_supported(fdflags) {
        return Ok(p1::errno::INVAL);
    }
    let mut descriptor_flags = p1_descriptor_flags(rights, fdflags);
    if open_flags.intersects(fs_types::OpenFlags::CREATE | fs_types::OpenFlags::TRUNCATE) {
        descriptor_flags |= fs_types::DescriptorFlags::WRITE;
    }
    p1_path_open_resolved(
        caller,
        &memory,
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

pub(super) async fn wasix_path_filestat_get<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    flags: u32,
    path: u32,
    path_len: u32,
    stat: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    let stat = GuestAddress::wasm32(stat);
    let path = memory
        .read_path(GuestAddress::wasm32(path), path_len)
        .await?;
    let (base, path) = match caller.data().resolve_wasix_path_base(fd, &path) {
        Ok(resolved) => resolved,
        Err(errno) => return Ok(errno),
    };
    let absolute = match crate::resolve_child_path(&base.path, &path) {
        Ok(absolute) => absolute,
        Err(error) => return Ok(p1_errno_from_component_path(error)),
    };
    if absolute == WASIX_NULL_DEVICE_PATH {
        return p1_write_filestat(
            caller,
            stat,
            p1_null_device_identity(),
            p1_null_device_stat(),
        )
        .await;
    }
    let follow = wasi_fs::follow_within(&base, p1_path_flags(flags));
    let (identity, stat_value) = match p1_stat_absolute_path(caller, &absolute, follow).await {
        Ok(stat) => stat,
        Err(errno) => return Ok(errno),
    };
    p1_write_filestat(caller, stat, identity, stat_value).await
}

pub(super) fn wasix_clock_time_set<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    clock_id: i32,
    timestamp: i64,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

pub(super) fn wasix_fd_dup<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    ret_fd: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let duplicated = match caller.data_mut().descriptors.dup(fd) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    p1_write_u32(caller, memory, GuestAddress::wasm32(ret_fd), duplicated)
}

pub(super) fn wasix_fd_dup2<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    target_fd: i32,
    cloexec: bool,
    ret_fd: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let duplicated = match caller.data_mut().descriptors.dup_to(fd, target_fd, cloexec) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    p1_write_u32(caller, memory, GuestAddress::wasm32(ret_fd), duplicated)
}

pub(super) fn wasix_fd_pipe<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    ret_fd1: u32,
    ret_fd2: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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
    let status = p1_write_u32(caller, memory, GuestAddress::wasm32(ret_fd1), read_fd);
    if status != p1::errno::SUCCESS {
        return status;
    }
    p1_write_u32(caller, memory, GuestAddress::wasm32(ret_fd2), write_fd)
}

pub(super) fn wasix_fd_event<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    initial_value: u64,
    flags: u32,
    ret_fd: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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
    p1_write_u32(caller, memory, GuestAddress::wasm32(ret_fd), fd)
}

pub(super) fn wasix_tty_get<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    state: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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
    write_wasix_tty_state(caller, memory, GuestAddress::wasm32(state), tty)
}

pub(super) fn wasix_tty_set<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    state: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_tty_control();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let tty = match read_wasix_tty_state(caller, memory, GuestAddress::wasm32(state)) {
        Ok(tty) => tty,
        Err(errno) => return errno,
    };
    caller.data_mut().tty_state = tty;
    p1::errno::SUCCESS
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn wasix_path_open2<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    dirflags: u32,
    path: u32,
    path_len: u32,
    oflags: u16,
    rights: u64,
    fdflags: u16,
    fdflagsext: u16,
    opened_fd: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let close_on_exec = match wasix_close_on_exec_flag(fdflagsext) {
        Ok(close_on_exec) => close_on_exec,
        Err(errno) => return Ok(errno),
    };
    let status = wasix_path_open(
        caller, fd, dirflags, path, path_len, oflags, rights, fdflags, opened_fd,
    )
    .await?;
    if status != p1::errno::SUCCESS {
        return Ok(status);
    }
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    let opened = memory.read_u32(GuestAddress::wasm32(opened_fd)).await?;
    Ok(caller
        .data_mut()
        .descriptors
        .set_close_on_exec(opened as i32, close_on_exec))
}

pub(super) fn wasix_fd_fdflags_get<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    ret_flags: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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
    p1_write_u16(caller, memory, GuestAddress::wasm32(ret_flags), flags)
}

pub(super) fn wasix_fd_fdflags_set<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    flags: u16,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

pub(super) fn wasix_getcwd<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    path: u32,
    path_len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let (path, path_len) = (GuestAddress::wasm32(path), GuestAddress::wasm32(path_len));
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
        return preview1_write_memory(memory, path.offset(u64::from(needed)), &[0]);
    }
    p1::errno::SUCCESS
}

pub(super) fn wasix_getcwd_required_len(cwd: &str) -> Result<u32, i32> {
    u32::try_from(cwd.len()).map_err(|_| p1::errno::OVERFLOW)
}

pub(super) async fn wasix_chdir<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    path: u32,
    path_len: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    let path = wasix_read_exec_string(&memory, path, path_len).await?;
    Ok(caller.data_mut().chdir(&path).await)
}

pub(super) fn wasix_callback_signal<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    callback: u32,
    callback_len: u32,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        }));
    };
    let callback = p1_read_path(caller, memory, GuestAddress::wasm32(callback), callback_len)
        .map_err(wasmtime::Error::new)?;
    caller.data_mut().signal_callback = Some(callback);
    Ok(())
}

pub(super) fn wasix_proc_id<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    ret_pid: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let pid = match u32::try_from(caller.data().instance().id().raw()) {
        Ok(pid) => pid,
        Err(_) => return p1::errno::OVERFLOW,
    };
    p1_write_u32(caller, memory, GuestAddress::wasm32(ret_pid), pid)
}

pub(super) fn wasix_proc_parent<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    pid: u32,
    ret_pid: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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
    p1_write_u32(caller, memory, GuestAddress::wasm32(ret_pid), parent)
}

pub(super) fn wasix_proc_signal<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    pid: u32,
    signal: i32,
) -> wasmtime::Result<i32>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

pub(super) fn wasix_proc_signals_get<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    buf: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let buf = GuestAddress::wasm32(buf);
    let dispositions = caller.data().signal_dispositions.clone();
    for (index, disposition) in dispositions.iter().enumerate() {
        let offset = buf.offset(index as u64 * u64::from(WASIX_SIGNAL_DISPOSITION_SIZE));
        let action = match disposition.action {
            WasixSignalDispositionAction::Default => WASIX_SIGNAL_DISPOSITION_DEFAULT,
            WasixSignalDispositionAction::Ignore => WASIX_SIGNAL_DISPOSITION_IGNORE,
        };
        let status = p1_write_u8(
            caller,
            memory,
            offset.offset(u64::from(WASIX_SIGNAL_DISPOSITION_SIGNAL_OFFSET)),
            disposition.signal,
        )
        .max(p1_write_u8(
            caller,
            memory,
            offset.offset(u64::from(WASIX_SIGNAL_DISPOSITION_ACTION_OFFSET)),
            action,
        ));
        if status != p1::errno::SUCCESS {
            return status;
        }
    }
    p1::errno::SUCCESS
}

pub(super) fn wasix_proc_signals_sizes_get<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    ret_size: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let Ok(len) = u32::try_from(caller.data().signal_dispositions.len()) else {
        return p1::errno::OVERFLOW;
    };
    p1_write_u32(caller, memory, GuestAddress::wasm32(ret_size), len)
}

pub(super) fn wasix_proc_raise_interval<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    signal: i32,
    interval: i64,
    repeat: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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
        // Dropping the handle cancels the timer task, so the instance
        // share gets its block back now rather than one interval later.
        caller.data_mut().signal_interval = None;
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
    let spawner = caller.data().spawner.clone();
    let timer_task = spawner.try_spawn(async move {
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
    });
    match timer_task {
        // The store owns the timer for the rest of its life: a repeating
        // interval sleeps between raises, so a detached one would hold
        // its block in the executor's instance share long after the
        // instance that asked for it had exited (#132). Assigning also
        // cancels whatever interval this store had before, which is the
        // generation bump above expressed as ownership.
        Ok(task) => caller.data_mut().signal_interval = Some(task),
        Err(error) => {
            tracing::warn!(
                target: "helios_kernel::program",
                %error,
                "refused a signal interval timer: the executor's instance share is full"
            );
            return p1::errno::NOMEM;
        }
    }
    p1::errno::SUCCESS
}

pub(super) async fn wasix_proc_fork<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    ret_pid: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    if let Some(value) = caller.data_mut().asyncify.rewind_value.take() {
        if wasix_call_asyncify_stop_rewind(caller).await != p1::errno::SUCCESS {
            return Ok(p1::errno::NOTSUP);
        }
        let Some(memory) = p1_async_memory(caller) else {
            return Ok(p1::errno::FAULT);
        };
        let Ok(pid) = u32::try_from(value) else {
            return Ok(p1::errno::OVERFLOW);
        };
        memory.write_u32(GuestAddress::wasm32(ret_pid), pid).await?;
        return Ok(p1::errno::SUCCESS);
    }
    let status = caller.data().require_fork_authority();
    if status != p1::errno::SUCCESS {
        return Ok(status);
    }
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    if caller.data().current_core_module.is_none() {
        return Ok(p1::errno::NOTSUP);
    }
    let Ok((stack_lower, stack_upper, stack_pointer)) = wasix_stack_bounds_from_caller(caller)
    else {
        return Ok(p1::errno::NOTSUP);
    };
    if stack_lower >= stack_pointer || stack_pointer > stack_upper {
        return Ok(p1::errno::INVAL);
    }
    let memory_stack_len = match usize::try_from(stack_upper - stack_pointer) {
        Ok(len) => len,
        Err(_) => return Ok(p1::errno::OVERFLOW),
    };
    let memory_stack = memory
        .read_bytes(GuestAddress::wasm32(stack_pointer), memory_stack_len)
        .await?;
    let unwind_stack_begin = match stack_lower.checked_add(WASIX_ASYNCIFY_DATA_SIZE) {
        Some(begin) if begin <= stack_pointer => begin,
        _ => return Ok(p1::errno::OVERFLOW),
    };
    memory.write_u32(GuestAddress::wasm32(ret_pid), 0).await?;
    let stack_lower_address = GuestAddress::wasm32(stack_lower);
    memory
        .write_u32(stack_lower_address, unwind_stack_begin)
        .await?;
    memory
        .write_u32(stack_lower_address.offset(4), stack_pointer)
        .await?;
    caller.data_mut().asyncify.phase = WasixAsyncifyPhase::Forking {
        ret_pid,
        stack_lower,
        stack_upper,
        unwind_stack_begin,
        memory_stack,
        stack_pointer,
    };
    Ok(wasix_call_asyncify_start_unwind(caller, stack_lower).await)
}

/// The program name a launch's timeline carries, decoded only while the
/// launch target is watched. A string the guest left unreadable is
/// recorded as empty and refused by the launch itself; a lost page traps.
async fn wasix_launch_program_label(
    memory: &Preview1AsyncMemory,
    name: u32,
    name_len: u32,
) -> Result<String, crate::memory::SwapFaultError> {
    if phases::gated(|| ()).is_none() {
        return Ok(String::new());
    }
    match wasix_read_exec_string(memory, name, name_len).await {
        Ok(program) => Ok(program),
        Err(ExecReadError::Program(_)) => Ok(String::new()),
        Err(ExecReadError::Swap(error)) => Err(error),
    }
}

/// The launch line's terminal for an errno-shaped exit: resource
/// failures are failures; every other errno exit refused the call.
fn launch_end_for_errno(errno: i32) -> phases::LaunchEnd {
    match errno {
        p1::errno::IO | p1::errno::NOMEM => phases::LaunchEnd::Failed,
        _ => phases::LaunchEnd::Refused,
    }
}

/// Marks `trace`'s terminal for an errno return site and yields the
/// errno back out for the `return`.
fn launch_exit_errno<CpuImpl: Cpu>(trace: &phases::Trace<CpuImpl>, errno: i32) -> i32 {
    trace.exit_errno(launch_end_for_errno(errno), errno)
}

/// Marks `trace`'s terminal for a launch-path error — the
/// `ProgramExecError`'s own kind when it carries one, `internal`
/// otherwise.
fn launch_exit_error<CpuImpl: Cpu>(trace: &phases::Trace<CpuImpl>, error: &wasmtime::Error) {
    match error.downcast_ref::<ProgramExecError>() {
        Some(error) => trace.exit_error(error),
        None => trace.failed(ProgramExecErrorKind::Internal),
    }
}

/// Marks `trace`'s terminal for a launch-path error on an errno
/// return site — `error_kind` and `errno` both when the error carries
/// a kind — and yields the errno back out for the `return`.
fn launch_exit_error_errno<CpuImpl: Cpu>(
    trace: &phases::Trace<CpuImpl>,
    error: &wasmtime::Error,
) -> i32 {
    let errno = p1_errno_from_wasmtime_error(error);
    match error.downcast_ref::<ProgramExecError>() {
        Some(error) => trace.exit_error_errno(error, errno),
        None => trace.exit_errno(launch_end_for_errno(errno), errno),
    }
}

/// Marks `trace`'s terminal for a stop that ended a launch call: an errno
/// as [`launch_exit_errno`] does, the loss of a guest page as a failure.
fn launch_exit_stop<CpuImpl: Cpu>(trace: &phases::Trace<CpuImpl>, stop: P1Stop) -> P1Stop {
    match stop {
        P1Stop::Errno(errno) => P1Stop::Errno(launch_exit_errno(trace, errno)),
        P1Stop::Trap(_) => {
            trace.failed(ProgramExecErrorKind::Internal);
            stop
        }
    }
}

/// [`launch_exit_error_errno`] for a launch-path error that may be the
/// loss of a guest page, which traps the guest instead of answering an
/// errno.
fn launch_exit_error_stop<CpuImpl: Cpu>(
    trace: &phases::Trace<CpuImpl>,
    error: &wasmtime::Error,
) -> Result<i32, P1Stop> {
    match error.downcast_ref::<crate::memory::SwapFaultError>() {
        Some(error) => Err(launch_exit_stop(trace, P1Stop::Trap(*error))),
        None => Ok(launch_exit_error_errno(trace, error)),
    }
}

pub(super) async fn wasix_proc_exec<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    name: u32,
    name_len: u32,
    args: u32,
    args_len: u32,
    env: Option<(u32, u32)>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_async_memory(caller) else {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        }));
    };
    // The launch call arrives before its program string can be
    // decoded, so the timeline's `program` is the decoded `name` — and
    // only a watched stream pays for reading it twice.
    let program = wasix_launch_program_label(&memory, name, name_len)
        .await
        .map_err(wasmtime::Error::new)?;
    let mut trace = phases::Trace::begin(caller.data().cpu.clone(), "exec", &program);
    let exec_authority = match caller.data().require_exec_authority() {
        Ok(authority) => authority,
        Err(_) => {
            trace.refused(ProgramExecErrorKind::PermissionDenied);
            return Err(wasmtime::Error::new(ProgramExecError {
                kind: ProgramExecErrorKind::PermissionDenied,
                detail: ProgramExecErrorDetail::ProcessAuthorityDenied,
            }));
        }
    };
    let compile_authority = CompileAuthority::from(&exec_authority);
    let prepared = match wasix_prepare_program(caller, &memory, name, name_len).await {
        Ok(prepared) => prepared,
        Err(error) => {
            launch_exit_error(&trace, &error);
            return Err(error);
        }
    };
    trace.record(LaunchPhase::SourceRead);
    trace.record_source_bytes(prepared.source.payload_len());
    // Emission hands to the exec path's own guard.
    trace.disarm();
    wasix_exec_prepared_program(
        caller,
        &compile_authority,
        &memory,
        prepared,
        args,
        args_len,
        env,
        trace.timeline().clone(),
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "the compile authority is the one argument that must be spelled out so no exec reaches the compiler without a process right"
)]
pub(super) async fn wasix_exec_prepared_program<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    compile: &CompileAuthority,
    memory: &Preview1AsyncMemory,
    prepared: WasixPreparedProgram,
    args: u32,
    args_len: u32,
    env: Option<(u32, u32)>,
    timeline: phases::Timeline,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let mut trace = phases::Trace::adopt(caller.data().cpu.clone(), timeline);
    let argv = wasix_read_exec_string(memory, args, args_len)
        .await
        .map(|value| wasix_split_lines(&value))
        .map_err(ExecReadError::into_error)
        .inspect_err(|error| launch_exit_error(&trace, error))?;
    let argv = ProgramArgv::from_caller(&prepared.guest_name, argv);
    let environment = match env {
        Some((ptr, len)) => match wasix_read_exec_string(memory, ptr, len).await {
            Ok(value) => wasix_split_environment(&value),
            Err(ExecReadError::Program(_)) => Vec::new(),
            Err(error @ ExecReadError::Swap(_)) => {
                let error = error.into_error();
                launch_exit_error(&trace, &error);
                return Err(error);
            }
        },
        None => caller.data().environment.clone(),
    };
    // The replacement keeps this instance, so it keeps this instance's id.
    let environment =
        LaunchEnvironment::from_guest(environment).into_process_env(caller.data().instance().id());
    let service = caller
        .data()
        .runtime_state
        .program_service()
        .ok_or(ProgramExecError {
            kind: ProgramExecErrorKind::Unavailable,
            detail: ProgramExecErrorDetail::HostOperationFailed,
        })
        .inspect_err(|error| trace.exit_error(error))
        .map_err(wasmtime::Error::new)?;
    let exec_context = caller.data().exec_context();
    let authority = caller.data().authority.clone();
    let descriptors = caller.data().descriptors.clone_for_exec();
    let filesystem = Some(caller.data().filesystem.clone());
    let signal_state = caller.data().signal_state.clone();
    let signal_dispositions = caller.data().signal_dispositions.clone();
    let executable = service
        .load_executable(
            compile,
            &exec_context,
            argv.program_name(),
            &prepared.source,
            None,
            trace.timeline(),
        )
        .await
        .map_err(wasmtime::Error::new)
        .inspect_err(|error| launch_exit_error(&trace, error))?;
    // The reply of an exec is the replacement itself: the call returns
    // by unwinding the calling instance into the new program, which
    // then owns the timeline's emit at its completion.
    trace.set_instance(caller.data().instance().id());
    trace.record(LaunchPhase::Reply);
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
            timeline: trace.timeline().for_task(),
        });
    trace.disarm();
    Err(wasmtime::Error::new(Preview1Exit))
}

#[expect(
    clippy::too_many_arguments,
    reason = "the parameter list is the guest ABI of this call, so grouping it would hide the contract and break the one-to-one match with the linker registration"
)]
pub(super) async fn wasix_proc_exec3<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
    Net: ComponentHostNetwork,
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
pub(super) async fn wasix_proc_exec_with_search<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_async_memory(caller) else {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        }));
    };
    let program = wasix_launch_program_label(&memory, name, name_len)
        .await
        .map_err(wasmtime::Error::new)?;
    let mut trace = phases::Trace::begin(caller.data().cpu.clone(), "exec", &program);
    let exec_authority = match caller.data().require_exec_authority() {
        Ok(authority) => authority,
        Err(_) => {
            trace.refused(ProgramExecErrorKind::PermissionDenied);
            return Err(wasmtime::Error::new(ProgramExecError {
                kind: ProgramExecErrorKind::PermissionDenied,
                detail: ProgramExecErrorDetail::ProcessAuthorityDenied,
            }));
        }
    };
    let compile_authority = CompileAuthority::from(&exec_authority);
    let prepared = match wasix_prepare_program_with_search(
        caller,
        &memory,
        name,
        name_len,
        search_path,
        path,
        path_len,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(error) => {
            launch_exit_error(&trace, &error);
            return Err(error);
        }
    };
    trace.record(LaunchPhase::SourceRead);
    trace.record_source_bytes(prepared.source.payload_len());
    trace.disarm();
    wasix_exec_prepared_program(
        caller,
        &compile_authority,
        &memory,
        prepared,
        args,
        args_len,
        env,
        trace.timeline().clone(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn wasix_proc_spawn<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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
pub(super) async fn wasix_proc_spawn_inner<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    // The boundary lands at the argv decode — before the chroot,
    // preopen and cwd work — symmetric with `proc_spawn2` and
    // `proc_exec`.
    let argv = wasix_split_lines(&wasix_read_exec_string(&memory, args, args_len).await?);
    let program = phases::gated(|| argv.first().cloned().unwrap_or_default()).unwrap_or_default();
    let mut trace = phases::Trace::begin(caller.data().cpu.clone(), "spawn", &program);
    let spawn_authority = match caller.data().require_spawn_authority() {
        Ok(authority) => authority,
        Err(status) => return Ok(launch_exit_errno(&trace, status)),
    };
    let compile_authority = CompileAuthority::from(&spawn_authority);
    let chroot = match chroot {
        0 => false,
        1 => true,
        _ => return Ok(launch_exit_errno(&trace, p1::errno::INVAL)),
    };
    let mut authority = if preopen_len == 0 {
        if chroot {
            return Ok(launch_exit_errno(&trace, p1::errno::INVAL));
        }
        caller.data().authority.clone()
    } else {
        let preopen = match wasix_read_exec_string(&memory, preopen, preopen_len).await {
            Ok(preopen) => preopen,
            Err(error) => return Err(launch_exit_stop(&trace, error.into())),
        };
        match wasix_proc_spawn_preopen_authority(caller, &preopen, chroot).await {
            Ok(authority) => authority,
            Err(errno) => return Ok(launch_exit_errno(&trace, errno)),
        }
    };
    if working_dir_len != 0 {
        let working_dir = match wasix_read_exec_string(&memory, working_dir, working_dir_len).await
        {
            Ok(working_dir) => working_dir,
            Err(error) => return Err(launch_exit_stop(&trace, error.into())),
        };
        if !working_dir.is_empty() && working_dir != "." {
            let cwd = match wasix_proc_spawn_resolve_child_cwd(&authority, &working_dir) {
                Ok(cwd) => cwd,
                Err(errno) => return Ok(launch_exit_errno(&trace, errno)),
            };
            let namespace = p1_namespace(caller);
            if let Err(errno) = wasix_require_directory(&namespace, &cwd.descriptor.path).await {
                return Ok(launch_exit_errno(&trace, errno));
            }
            let cap = match authority.derive_directory_cap(
                &cwd.descriptor.path,
                &cwd.guest_name,
                descriptor_flags_to_directory_authority(cwd.descriptor.flags),
            ) {
                Ok(cap) => cap,
                Err(_) => return Ok(launch_exit_errno(&trace, p1::errno::NOTCAPABLE)),
            };
            authority.chdir(cap);
        }
    }
    let prepared = match wasix_prepare_program(caller, &memory, name, name_len).await {
        Ok(prepared) => prepared,
        Err(error) => return launch_exit_error_stop(&trace, &error),
    };
    trace.record(LaunchPhase::SourceRead);
    trace.record_source_bytes(prepared.source.payload_len());
    let result = match wasix_spawn_child(
        caller,
        &compile_authority,
        prepared,
        argv,
        &mut trace,
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
        // The child marked the launch's terminal on the error it returned.
        Err(errno) => return Ok(errno),
    };
    wasix_write_process_handles(&memory, GuestAddress::wasm32(ret_handles), result).await?;
    Ok(p1::errno::SUCCESS)
}

/// Refuses a path that does not name a directory.
pub(super) async fn wasix_require_directory<HostFs>(
    namespace: &Namespace<HostFs>,
    path: &str,
) -> Result<(), i32>
where
    HostFs: crate::HostFileSystem,
{
    let metadata = wasi_fs::stat_path(namespace, path, crate::Follow::No)
        .await
        .map_err(p1_errno_from_fs)?;
    if metadata.kind != helios_hal::fs::NodeKind::Directory {
        return Err(p1::errno::NOTDIR);
    }
    Ok(())
}

pub(super) async fn wasix_proc_spawn_preopen_authority<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    preopen: &str,
    chroot: bool,
) -> Result<ProcessAuthority, i32>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let namespace = p1_namespace(caller);
    let mut authority =
        wasix_proc_spawn_inherited_non_directory_authority(&caller.data().authority);
    let mut chroot_entry = None;
    for entry in wasix_split_lines(preopen) {
        let store = caller.data();
        let guest_name = wasix_proc_spawn_preopen_guest_name(store.cwd.as_ref(), &entry)?;
        let (source_path, flags) = store.resolve_absolute_guest_path(&guest_name)?;
        wasix_require_directory(&namespace, &source_path).await?;
        let store = caller.data();
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
        let preopen = caller
            .data()
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
    authority.grant_mount_rights(parent.mount_rights());
    // Device classes follow the shell to the programs it runs. A named
    // device does not: it belongs to the driver instance it was granted
    // to, not to that instance's children.
    authority.grant_device_classes(parent.device_class_rights());
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

/// The working directory `path` names for a spawned child under
/// `authority`; whether it is a directory is the caller's to check, with
/// [`wasix_require_directory`].
pub(super) fn wasix_proc_spawn_resolve_child_cwd(
    authority: &ProcessAuthority,
    path: &str,
) -> Result<Preview1Cwd, i32> {
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
    Ok(Preview1Cwd {
        guest_name,
        descriptor: FsDescriptor {
            path: source_path,
            kind: FsNodeKind::Directory,
            flags,
            identity: None,
            mount: None,
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
pub(super) async fn wasix_proc_spawn2<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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
pub(super) async fn wasix_proc_spawn2_inner<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    let argv = wasix_split_lines(&wasix_read_exec_string(&memory, args, args_len).await?);
    let program = phases::gated(|| argv.first().cloned().unwrap_or_default()).unwrap_or_default();
    let mut trace = phases::Trace::begin(caller.data().cpu.clone(), "spawn", &program);
    let spawn_authority = match caller.data().require_spawn_authority() {
        Ok(authority) => authority,
        Err(status) => return Ok(launch_exit_errno(&trace, status)),
    };
    let compile_authority = CompileAuthority::from(&spawn_authority);
    let environment = if env == 0 && env_len == 0 {
        None
    } else {
        match wasix_read_exec_string(&memory, env, env_len).await {
            Ok(value) => Some(wasix_split_environment(&value)),
            Err(error) => return Err(launch_exit_stop(&trace, error.into())),
        }
    };
    let prepared = match wasix_prepare_program_with_search(
        caller,
        &memory,
        name,
        name_len,
        search_path,
        path,
        path_len,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(error) => return launch_exit_error_stop(&trace, &error),
    };
    trace.record(LaunchPhase::SourceRead);
    trace.record_source_bytes(prepared.source.payload_len());
    let snapshot = if fd_ops == 0 && fd_ops_len == 0 {
        None
    } else {
        match wasix_spawn_descriptor_snapshot(caller, &memory, fd_ops, fd_ops_len).await {
            Ok(snapshot) => Some(snapshot),
            Err(stop) => return Err(launch_exit_stop(&trace, stop)),
        }
    };
    let (authority, descriptors) = match snapshot {
        Some(snapshot) => (Some(snapshot.authority), Some(snapshot.descriptors)),
        None => (None, None),
    };
    let signal_dispositions =
        match wasix_read_signal_dispositions(&memory, GuestAddress::wasm32(signals), signals_len)
            .await
        {
            Ok(dispositions) => dispositions,
            Err(stop) => return Err(launch_exit_stop(&trace, stop)),
        };
    let result = match wasix_spawn_child(
        caller,
        &compile_authority,
        prepared,
        argv,
        &mut trace,
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
        // The child marked the launch's terminal on the error it returned.
        Err(errno) => return Ok(errno),
    };
    memory
        .write_u32(GuestAddress::wasm32(ret_pid), result.pid)
        .await?;
    Ok(p1::errno::SUCCESS)
}

pub(super) async fn wasix_proc_join<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    pid: u32,
    flags: u32,
    ret_status: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let started = p1_kernel_profile_start(caller.data());
    let result = wasix_proc_join_inner(caller, pid, flags, ret_status).await;
    p1_record_optional_kernel_profile(caller.data(), "proc_join", started);
    result
}

pub(super) async fn wasix_proc_join_inner<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    pid: u32,
    flags: u32,
    ret_status: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_join_authority();
    if status != p1::errno::SUCCESS {
        return Ok(status);
    }
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    if flags & !WASIX_JOIN_FLAGS_SUPPORTED != 0 {
        return Ok(p1::errno::INVAL);
    }
    let (pid, ret_status) = (GuestAddress::wasm32(pid), GuestAddress::wasm32(ret_status));
    let requested_pid = wasix_read_option_pid(&memory, pid).await?;
    let Some(index) = caller.data().find_child_index(requested_pid) else {
        wasix_write_join_nothing(&memory, ret_status).await?;
        return Ok(p1::errno::SUCCESS);
    };
    if flags & WASIX_JOIN_FLAG_NON_BLOCKING != 0 {
        match caller.data_mut().poll_child_exit(index) {
            Ok(Some(code)) => {
                let child = caller.data_mut().children.swap_remove(index);
                wasix_write_join_exit(&memory, pid, ret_status, child.pid, code).await?;
            }
            Ok(None) => {
                crate::yield_now().await;
                wasix_write_join_nothing(&memory, ret_status).await?;
            }
            Err(errno) => return Ok(errno),
        }
    } else {
        let child_pid = caller.data().children[index].pid;
        match caller.data_mut().poll_child_exit(index) {
            Ok(Some(code)) => {
                let child = caller.data_mut().children.swap_remove(index);
                wasix_write_join_exit(&memory, pid, ret_status, child.pid, code).await?;
                return Ok(p1::errno::SUCCESS);
            }
            Ok(None) => {}
            Err(errno) => return Ok(errno),
        }
        let Some(exit) = caller.data_mut().children[index].exit.take() else {
            wasix_write_join_nothing(&memory, ret_status).await?;
            return Ok(p1::errno::SUCCESS);
        };
        let (code, filesystem) = wasix_child_exit_result(exit.await);
        if let Some(filesystem) = filesystem {
            caller.data_mut().filesystem.adopt_root_layer(filesystem);
        }
        let Some(index) = caller.data().find_child_index(Some(child_pid)) else {
            wasix_write_join_nothing(&memory, ret_status).await?;
            return Ok(p1::errno::SUCCESS);
        };
        let child = caller.data_mut().children.swap_remove(index);
        wasix_write_join_exit(&memory, pid, ret_status, child.pid, code).await?;
    }
    Ok(p1::errno::SUCCESS)
}

pub(super) fn wasix_child_exit_result(
    result: Result<Result<ChildExit, ProgramExecError>, futures::channel::oneshot::Canceled>,
) -> (u32, Option<MemoryFs>) {
    match result {
        Ok(Ok(exit)) => (exit.exit_code, exit.filesystem),
        Ok(Err(_)) | Err(_) => (u32::from(p1::errno::IO as u16), None),
    }
}

pub(super) async fn wasix_proc_snapshot<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    if caller.data_mut().asyncify.process_snapshot_rewinding {
        caller.data_mut().asyncify.process_snapshot_rewinding = false;
        if wasix_call_asyncify_stop_rewind(caller).await != p1::errno::SUCCESS {
            return Ok(p1::errno::NOTSUP);
        }
        return Ok(p1::errno::SUCCESS);
    }

    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    if caller.data().current_core_module.is_none() {
        return Ok(p1::errno::NOTSUP);
    }
    let Ok((stack_lower, stack_upper, stack_pointer)) = wasix_stack_bounds_from_caller(caller)
    else {
        return Ok(p1::errno::NOTSUP);
    };
    if stack_lower >= stack_pointer || stack_pointer > stack_upper {
        return Ok(p1::errno::INVAL);
    }
    let memory_stack_len = match usize::try_from(stack_upper - stack_pointer) {
        Ok(len) => len,
        Err(_) => return Ok(p1::errno::OVERFLOW),
    };
    let memory_stack = memory
        .read_bytes(GuestAddress::wasm32(stack_pointer), memory_stack_len)
        .await?;
    let unwind_stack_begin = match stack_lower.checked_add(WASIX_ASYNCIFY_DATA_SIZE) {
        Some(begin) if begin <= stack_pointer => begin,
        _ => return Ok(p1::errno::OVERFLOW),
    };
    let stack_lower_address = GuestAddress::wasm32(stack_lower);
    memory
        .write_u32(stack_lower_address, unwind_stack_begin)
        .await?;
    memory
        .write_u32(stack_lower_address.offset(4), stack_pointer)
        .await?;
    caller.data_mut().asyncify.phase = WasixAsyncifyPhase::ProcessSnapshot {
        stack_lower,
        stack_upper,
        unwind_stack_begin,
        memory_stack,
        stack_pointer,
    };
    Ok(wasix_call_asyncify_start_unwind(caller, stack_lower).await)
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

pub(super) fn wasix_prepare_child_io<CpuImpl, Net, HostFs>(
    store: &Preview1ProgramStore<CpuImpl, Net, HostFs>,
    io: WasixSpawnIo,
) -> Result<WasixSpawnPreparedIo, i32>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

pub(super) fn wasix_prepare_child_output_route<CpuImpl, Net, HostFs>(
    store: &Preview1ProgramStore<CpuImpl, Net, HostFs>,
    mode: WasixStdioMode,
    stream: crate::ComponentOutputStreamKind,
) -> Result<(OutputRoute, Option<crate::ByteReader>), i32>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

pub(super) async fn wasix_prepare_program<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    memory: &Preview1AsyncMemory,
    name: u32,
    name_len: u32,
) -> wasmtime::Result<WasixPreparedProgram>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let name = wasix_read_exec_string(memory, name, name_len)
        .await
        .map_err(ExecReadError::into_error)?;
    wasix_prepare_program_from_name(caller, &name).await
}

pub(super) async fn wasix_prepare_program_with_search<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    memory: &Preview1AsyncMemory,
    name: u32,
    name_len: u32,
    search_path: i32,
    path: u32,
    path_len: u32,
) -> wasmtime::Result<WasixPreparedProgram>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let name = wasix_read_exec_string(memory, name, name_len)
        .await
        .map_err(ExecReadError::into_error)?;
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

    let path = wasix_read_exec_string(memory, path, path_len)
        .await
        .map_err(ExecReadError::into_error)?;
    wasix_prepare_program_from_search_paths(caller, &name, WasixExecSearchPath::Guest(&path)).await
}

pub(super) async fn wasix_prepare_program_from_search_paths<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    name: &str,
    search_path: WasixExecSearchPath<'_>,
) -> wasmtime::Result<WasixPreparedProgram>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

pub(super) async fn wasix_prepare_program_from_search_directory<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    name: &str,
    directory: &str,
) -> Option<WasixPreparedProgram>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

pub(super) async fn wasix_spawn_descriptor_snapshot<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    memory: &Preview1AsyncMemory,
    fd_ops: u32,
    fd_ops_len: u32,
) -> Result<WasixSpawnFdSnapshot<Net>, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    if fd_ops == 0 {
        return Err(P1Stop::Errno(p1::errno::FAULT));
    }
    let mut snapshot = WasixSpawnFdSnapshot {
        descriptors: caller.data().descriptors.clone(),
        authority: caller.data().authority.clone(),
        cwd: caller.data().cwd.clone(),
    };
    let fd_ops = GuestAddress::wasm32(fd_ops);
    for index in 0..fd_ops_len {
        let op = fd_ops.offset(u64::from(index) * u64::from(WASIX_PROC_SPAWN_FD_OP_SIZE));
        wasix_apply_spawn_fd_op(caller, memory, &mut snapshot, op).await?;
    }
    Ok(snapshot)
}

pub(super) async fn wasix_apply_spawn_fd_op<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    memory: &Preview1AsyncMemory,
    snapshot: &mut WasixSpawnFdSnapshot<Net>,
    op: GuestAddress,
) -> Result<(), P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let field = |offset: u32| op.offset(u64::from(offset));
    let cmd = memory
        .read_u8(field(WASIX_PROC_SPAWN_FD_OP_CMD_OFFSET))
        .await?;
    let fd = memory
        .read_u32(field(WASIX_PROC_SPAWN_FD_OP_FD_OFFSET))
        .await?;
    let fd = i32::try_from(fd).map_err(|_| P1Stop::Errno(p1::errno::OVERFLOW))?;
    match cmd {
        WASIX_PROC_SPAWN_FD_OP_CLOSE => {
            let status = snapshot.descriptors.close(fd);
            if status == p1::errno::SUCCESS {
                Ok(())
            } else {
                Err(P1Stop::Errno(status))
            }
        }
        WASIX_PROC_SPAWN_FD_OP_DUP2 => {
            let source_fd = memory
                .read_u32(field(WASIX_PROC_SPAWN_FD_OP_SRC_FD_OFFSET))
                .await?;
            let source_fd =
                i32::try_from(source_fd).map_err(|_| P1Stop::Errno(p1::errno::OVERFLOW))?;
            let fdflagsext = memory
                .read_u16(field(WASIX_PROC_SPAWN_FD_OP_FDFLAGSEXT_OFFSET))
                .await?;
            let close_on_exec = wasix_close_on_exec_flag(fdflagsext)?;
            Ok(snapshot
                .descriptors
                .dup_to(source_fd, fd, close_on_exec)
                .map(drop)?)
        }
        WASIX_PROC_SPAWN_FD_OP_CHDIR => {
            let path_ptr = memory
                .read_u32(field(WASIX_PROC_SPAWN_FD_OP_PATH_OFFSET))
                .await?;
            let path_len = memory
                .read_u32(field(WASIX_PROC_SPAWN_FD_OP_PATH_LEN_OFFSET))
                .await?;
            let path = wasix_read_exec_string(memory, path_ptr, path_len).await?;
            let namespace = p1_namespace(caller);
            Ok(wasix_apply_spawn_chdir(&namespace, snapshot, fd, &path).await?)
        }
        WASIX_PROC_SPAWN_FD_OP_FCHDIR => Ok(wasix_apply_spawn_fchdir(snapshot, fd)?),
        WASIX_PROC_SPAWN_FD_OP_OPEN => {
            let source_fd = memory
                .read_u32(field(WASIX_PROC_SPAWN_FD_OP_SRC_FD_OFFSET))
                .await?;
            let source_fd =
                i32::try_from(source_fd).map_err(|_| P1Stop::Errno(p1::errno::OVERFLOW))?;
            let path_ptr = memory
                .read_u32(field(WASIX_PROC_SPAWN_FD_OP_PATH_OFFSET))
                .await?;
            let path_len = memory
                .read_u32(field(WASIX_PROC_SPAWN_FD_OP_PATH_LEN_OFFSET))
                .await?;
            let dirflags = memory
                .read_u32(field(WASIX_PROC_SPAWN_FD_OP_DIRFLAGS_OFFSET))
                .await?;
            let oflags = memory
                .read_u16(field(WASIX_PROC_SPAWN_FD_OP_OFLAGS_OFFSET))
                .await?;
            let rights = memory
                .read_u64(field(WASIX_PROC_SPAWN_FD_OP_RIGHTS_BASE_OFFSET))
                .await?;
            let fdflags = memory
                .read_u16(field(WASIX_PROC_SPAWN_FD_OP_FDFLAGS_OFFSET))
                .await?;
            let fdflagsext = memory
                .read_u16(field(WASIX_PROC_SPAWN_FD_OP_FDFLAGSEXT_OFFSET))
                .await?;
            let close_on_exec = wasix_close_on_exec_flag(fdflagsext)?;
            let path = memory
                .read_path(GuestAddress::wasm32(path_ptr), path_len)
                .await?;
            Ok(wasix_apply_spawn_open(
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
            .await?)
        }
        _ => Err(P1Stop::Errno(p1::errno::INVAL)),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn wasix_apply_spawn_open<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    snapshot: &mut WasixSpawnFdSnapshot<Net>,
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
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    if !p1_file_fdflags_supported(fdflags) {
        return Err(p1::errno::INVAL);
    }
    // Refuse a target past the descriptor limit before the file is opened.
    snapshot.descriptors.target_index(fd)?;
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

pub(super) fn wasix_spawn_resolve_open_base<Net: ComponentHostNetwork>(
    snapshot: &WasixSpawnFdSnapshot<Net>,
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

pub(super) async fn wasix_apply_spawn_chdir<HostFs, Net>(
    namespace: &Namespace<HostFs>,
    snapshot: &mut WasixSpawnFdSnapshot<Net>,
    fd: i32,
    path: &str,
) -> Result<(), i32>
where
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let cwd = wasix_spawn_resolve_cwd_target(snapshot, fd, path)?;
    wasix_require_directory(namespace, &cwd.descriptor.path).await?;
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

pub(super) fn wasix_apply_spawn_fchdir<Net: ComponentHostNetwork>(
    snapshot: &mut WasixSpawnFdSnapshot<Net>,
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

/// The working directory a spawn's `chdir` action names; whether it is a
/// directory is the caller's to check, with [`wasix_require_directory`].
pub(super) fn wasix_spawn_resolve_cwd_target<Net: ComponentHostNetwork>(
    snapshot: &WasixSpawnFdSnapshot<Net>,
    fd: i32,
    path: &str,
) -> Result<Preview1Cwd, i32> {
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
    Ok(Preview1Cwd {
        guest_name,
        descriptor: FsDescriptor {
            path: source_path,
            kind: FsNodeKind::Directory,
            flags,
            identity: None,
            mount: None,
        },
    })
}

pub(super) fn wasix_spawn_guest_name_for_source<Net: ComponentHostNetwork>(
    snapshot: &WasixSpawnFdSnapshot<Net>,
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

pub(super) fn wasix_spawn_resolve_absolute_guest_base<Net: ComponentHostNetwork>(
    snapshot: &WasixSpawnFdSnapshot<Net>,
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

pub(super) async fn wasix_prepare_program_from_name<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    name: &str,
) -> wasmtime::Result<WasixPreparedProgram>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let guest_name = wasix_resolve_exec_guest_name(caller.data(), name)?;
    wasix_prepare_program_from_guest_name(caller, guest_name).await
}

pub(super) async fn wasix_prepare_program_from_guest_name<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    guest_name: String,
) -> wasmtime::Result<WasixPreparedProgram>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let source_path = wasix_resolve_exec_source_path(caller.data(), &guest_name)?;
    let source = wasix_read_program_source(caller, &source_path).await?;
    Ok(WasixPreparedProgram { guest_name, source })
}

pub(super) async fn wasix_read_program_source<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    source_path: &str,
) -> wasmtime::Result<ProgramSource>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let namespace = p1_namespace(caller);
    let not_granted = || {
        wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::PermissionDenied,
            detail: ProgramExecErrorDetail::ProgramSourceNotGranted,
        })
    };
    let floor = caller
        .data()
        .authority
        .program_load_floor(source_path)
        .ok_or_else(not_granted)?
        .to_owned();
    // Trust follows the layer that produced the bytes, never the path or
    // the mount: only boot-image bytes skip verification (#483).
    wasi_fs::read_program_file(&namespace, source_path, &floor)
        .await
        .map(ProgramSource::from_file)
        .map_err(|_| not_granted())
}

pub(super) fn wasix_resolve_exec_guest_name<CpuImpl, Net, HostFs>(
    store: &Preview1ProgramStore<CpuImpl, Net, HostFs>,
    name: &str,
) -> wasmtime::Result<String>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

pub(super) fn wasix_resolve_exec_source_path<CpuImpl, Net, HostFs>(
    store: &Preview1ProgramStore<CpuImpl, Net, HostFs>,
    guest_name: &str,
) -> wasmtime::Result<String>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

/// Spawns `prepared` as a child of the calling instance. Every error
/// it returns already carries the launch's terminal mark — typed
/// `error_kind`/`errno` where the error was a `ProgramExecError`,
/// classified `end`+`errno` otherwise — so its callers return the
/// errno untouched rather than reclassifying from the bare `i32`.
pub(super) async fn wasix_spawn_child<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    compile: &CompileAuthority,
    prepared: WasixPreparedProgram,
    argv: Vec<String>,
    trace: &mut phases::Trace<CpuImpl>,
    io: WasixSpawnIo,
    inheritance: WasixChildInheritance<Net>,
) -> Result<WasixSpawnResult, i32>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let WasixChildInheritance {
        environment,
        authority,
        descriptors,
        signal_dispositions,
    } = inheritance;
    let prepared_io = wasix_prepare_child_io(caller.data(), io).inspect_err(|errno| {
        launch_exit_errno(trace, *errno);
    })?;
    let argv = ProgramArgv::from_caller(&prepared.guest_name, argv);
    let environment = environment.unwrap_or_else(|| caller.data().environment.clone());
    let runtime_state = caller.data().runtime_state.clone();
    let service = runtime_state.wait_for_program_service().await;
    let exec_context = caller.data().exec_context();
    let authority = authority.unwrap_or_else(|| caller.data().authority.clone());
    let filesystem = Some(caller.data().filesystem.clone());
    let launch_started = p1_kernel_profile_start(caller.data());
    let mut child = service
        .spawn_with_output_mode(
            compile,
            exec_context,
            prepared.source,
            ProgramLaunch {
                argv,
                env: LaunchEnvironment::from_guest(environment),
                authority,
                filesystem,
                descriptors,
                signal_dispositions,
                timeline: trace.timeline().for_task(),
            },
            prepared_io.output_mode,
            ChildStdio {
                stdin: prepared_io.stdin_writer,
                stdout: prepared_io.stdout_reader,
                stderr: prepared_io.stderr_reader,
            },
        )
        .await
        .map_err(|error| {
            trace.exit_error_errno(&error, p1_errno_from_program_exec_error(&error))
        })?;
    p1_record_optional_kernel_profile(caller.data(), "proc_spawn_child_launch", launch_started);
    // The child's task holds a `for_task` handle and owns the line's
    // emit from here on — a failure wiring its stdio is this call's to
    // report, but not a second launch line.
    trace.disarm();
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
    trace.set_instance(child.instance_id);
    trace.record(LaunchPhase::Reply);
    Ok(WasixSpawnResult {
        pid,
        stdin_fd,
        stdout_fd,
        stderr_fd,
    })
}

pub(super) fn wasix_insert_child_stdin<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    child: &mut ChildHandle,
) -> Result<Option<u32>, i32>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

pub(super) fn wasix_insert_child_output<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    child: &mut ChildHandle,
    stream: crate::ComponentOutputStreamKind,
) -> Result<Option<u32>, i32>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

pub(super) async fn wasix_write_process_handles(
    memory: &Preview1AsyncMemory,
    ret_handles: GuestAddress,
    result: WasixSpawnResult,
) -> Result<(), GuestAccessError> {
    memory.write_u32(ret_handles, result.pid).await?;
    wasix_write_option_fd(
        memory,
        ret_handles.offset(u64::from(WASIX_PROCESS_HANDLES_STDIN_OFFSET)),
        result.stdin_fd,
    )
    .await?;
    wasix_write_option_fd(
        memory,
        ret_handles.offset(u64::from(WASIX_PROCESS_HANDLES_STDOUT_OFFSET)),
        result.stdout_fd,
    )
    .await?;
    wasix_write_option_fd(
        memory,
        ret_handles.offset(u64::from(WASIX_PROCESS_HANDLES_STDERR_OFFSET)),
        result.stderr_fd,
    )
    .await
}

pub(super) async fn wasix_write_option_fd(
    memory: &Preview1AsyncMemory,
    ptr: GuestAddress,
    fd: Option<u32>,
) -> Result<(), GuestAccessError> {
    memory
        .write_u8(
            ptr,
            if fd.is_some() {
                WASIX_OPTION_SOME
            } else {
                WASIX_OPTION_NONE
            },
        )
        .await?;
    memory
        .write_u32(
            ptr.offset(u64::from(WASIX_OPTION_UNION_U32_OFFSET)),
            fd.unwrap_or(0),
        )
        .await
}

pub(super) async fn wasix_write_join_nothing(
    memory: &Preview1AsyncMemory,
    ret_status: GuestAddress,
) -> Result<(), GuestAccessError> {
    memory
        .write_u8(ret_status, WASIX_JOIN_STATUS_NOTHING)
        .await?;
    memory
        .write_u16(
            ret_status.offset(u64::from(WASIX_JOIN_STATUS_UNION_OFFSET)),
            0,
        )
        .await
}

pub(super) async fn wasix_write_join_exit(
    memory: &Preview1AsyncMemory,
    pid_ptr: GuestAddress,
    ret_status: GuestAddress,
    pid: u32,
    code: u32,
) -> Result<(), GuestAccessError> {
    let errno = u16::try_from(code).unwrap_or(u16::MAX);
    memory.write_u8(pid_ptr, WASIX_OPTION_SOME).await?;
    memory
        .write_u32(
            pid_ptr.offset(u64::from(WASIX_OPTION_UNION_U32_OFFSET)),
            pid,
        )
        .await?;
    memory
        .write_u8(ret_status, WASIX_JOIN_STATUS_EXIT_NORMAL)
        .await?;
    memory
        .write_u16(
            ret_status.offset(u64::from(WASIX_JOIN_STATUS_UNION_OFFSET)),
            errno,
        )
        .await
}

/// Why a guest string a launch call names could not be read: the guest's
/// own mistake, which the call answers as it always has, or the loss of a
/// guest page, which traps the guest.
pub(super) enum ExecReadError {
    Program(ProgramExecError),
    Swap(crate::memory::SwapFaultError),
}

impl ExecReadError {
    /// The error a launch call that answers with a `wasmtime::Error`
    /// returns for this failure.
    pub(super) fn into_error(self) -> wasmtime::Error {
        match self {
            Self::Program(error) => wasmtime::Error::new(error),
            Self::Swap(error) => wasmtime::Error::new(error),
        }
    }
}

impl From<ExecReadError> for P1Stop {
    fn from(error: ExecReadError) -> Self {
        match error {
            ExecReadError::Program(_) => Self::Errno(p1::errno::FAULT),
            ExecReadError::Swap(error) => Self::Trap(error),
        }
    }
}

pub(super) async fn wasix_read_exec_string(
    memory: &Preview1AsyncMemory,
    ptr: u32,
    len: u32,
) -> Result<String, ExecReadError> {
    if len == 0 {
        return Ok(String::new());
    }
    let mut bytes = match memory
        .read_bytes(GuestAddress::wasm32(ptr), len as usize)
        .await
    {
        Ok(bytes) => bytes,
        Err(GuestAccessError::OutOfBounds) => {
            return Err(ExecReadError::Program(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
            }));
        }
        Err(GuestAccessError::Swap(error)) => return Err(ExecReadError::Swap(error)),
    };
    while bytes.last().is_some_and(|byte| *byte == 0) {
        bytes.pop();
    }
    String::from_utf8(bytes).map_err(|_| {
        ExecReadError::Program(ProgramExecError {
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

pub(super) async fn wasix_read_signal_dispositions(
    memory: &Preview1AsyncMemory,
    ptr: GuestAddress,
    len: u32,
) -> Result<Vec<WasixSignalDisposition>, P1Stop> {
    if len == 0 {
        if ptr == GuestAddress::wasm32(0) {
            return Ok(Vec::new());
        }
        return Err(P1Stop::Errno(p1::errno::INVAL));
    }
    if ptr == GuestAddress::wasm32(0) {
        return Err(P1Stop::Errno(p1::errno::FAULT));
    }
    let mut dispositions = Vec::new();
    for index in 0..len {
        let offset = ptr.offset(u64::from(index) * u64::from(WASIX_SIGNAL_DISPOSITION_SIZE));
        let signal = memory
            .read_u8(offset.offset(u64::from(WASIX_SIGNAL_DISPOSITION_SIGNAL_OFFSET)))
            .await?;
        let action = memory
            .read_u8(offset.offset(u64::from(WASIX_SIGNAL_DISPOSITION_ACTION_OFFSET)))
            .await?;
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
    ptr: GuestAddress,
) -> Result<WasixTtyState, i32> {
    Ok(WasixTtyState {
        cols: p1_try_read_u32(caller, memory, ptr).map_err(|_| p1::errno::FAULT)?,
        rows: p1_try_read_u32(caller, memory, ptr.offset(4)).map_err(|_| p1::errno::FAULT)?,
        width: p1_try_read_u32(caller, memory, ptr.offset(8)).map_err(|_| p1::errno::FAULT)?,
        height: p1_try_read_u32(caller, memory, ptr.offset(12)).map_err(|_| p1::errno::FAULT)?,
        stdin_tty: p1_read_wasix_bool(caller, memory, ptr.offset(16))?,
        stdout_tty: p1_read_wasix_bool(caller, memory, ptr.offset(17))?,
        stderr_tty: p1_read_wasix_bool(caller, memory, ptr.offset(18))?,
        echo: p1_read_wasix_bool(caller, memory, ptr.offset(19))?,
        line_buffered: p1_read_wasix_bool(caller, memory, ptr.offset(20))?,
        line_feeds: p1_read_wasix_bool(caller, memory, ptr.offset(21))?,
    })
}

pub(super) fn write_wasix_tty_state<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: GuestAddress,
    state: WasixTtyState,
) -> i32 {
    p1_write_u32(caller, memory, ptr, state.cols)
        .max(p1_write_u32(caller, memory, ptr.offset(4), state.rows))
        .max(p1_write_u32(caller, memory, ptr.offset(8), state.width))
        .max(p1_write_u32(caller, memory, ptr.offset(12), state.height))
        .max(p1_write_wasix_bool(
            caller,
            memory,
            ptr.offset(16),
            state.stdin_tty,
        ))
        .max(p1_write_wasix_bool(
            caller,
            memory,
            ptr.offset(17),
            state.stdout_tty,
        ))
        .max(p1_write_wasix_bool(
            caller,
            memory,
            ptr.offset(18),
            state.stderr_tty,
        ))
        .max(p1_write_wasix_bool(
            caller,
            memory,
            ptr.offset(19),
            state.echo,
        ))
        .max(p1_write_wasix_bool(
            caller,
            memory,
            ptr.offset(20),
            state.line_buffered,
        ))
        .max(p1_write_wasix_bool(
            caller,
            memory,
            ptr.offset(21),
            state.line_feeds,
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The accessor over `bytes`, on a machine that cannot swap, so every
    /// page is present.
    fn memory_over(bytes: &mut [u8]) -> Preview1AsyncMemory {
        Preview1AsyncMemory::new(Preview1Memory {
            base: bytes.as_mut_ptr() as usize,
            len: bytes.len(),
        })
    }

    fn run<F: Future>(future: F) -> F::Output {
        futures_lite::future::block_on(future)
    }

    fn read_error_stop(error: ExecReadError) -> P1Stop {
        error.into()
    }

    /// A launch string is read in full in Chinese, Japanese and Korean,
    /// with the NUL padding a guest's C string carries trimmed off.
    #[test]
    fn a_launch_string_reads_in_every_cjk_script_without_its_padding() {
        for text in ["程序\n参数", "プログラム\n引数", "프로그램\n인수"] {
            let mut guest = vec![0_u8; 128];
            guest[16..16 + text.len()].copy_from_slice(text.as_bytes());
            let padded = text.len() + 3;
            let memory = memory_over(&mut guest);
            let read = run(wasix_read_exec_string(&memory, 16, padded as u32));
            assert_eq!(read.ok().as_deref(), Some(text));
        }
    }

    /// A string outside memory or not UTF-8 is the guest's mistake, which a
    /// launch call answers with `EFAULT`; an empty one needs no memory.
    #[test]
    fn a_launch_string_the_guest_cannot_supply_is_efault() {
        let mut guest = vec![0xff_u8; 32];
        let memory = memory_over(&mut guest);
        for (ptr, len) in [(0, 4), (16, 64), (u32::MAX, 1)] {
            let error = run(wasix_read_exec_string(&memory, ptr, len))
                .expect_err("the string is not there");
            assert_eq!(read_error_stop(error), P1Stop::Errno(p1::errno::FAULT));
        }
        assert_eq!(
            run(wasix_read_exec_string(&memory, u32::MAX, 0))
                .ok()
                .as_deref(),
            Some("")
        );
    }

    /// The process handles of a spawn land as the guest's layout has
    /// them: the pid, then three optional descriptors at their offsets.
    #[test]
    fn spawned_process_handles_land_at_their_offsets() {
        let mut guest = vec![0xee_u8; 64];
        let memory = memory_over(&mut guest);
        run(wasix_write_process_handles(
            &memory,
            GuestAddress::wasm32(8),
            WasixSpawnResult {
                pid: 77,
                stdin_fd: Some(3),
                stdout_fd: None,
                stderr_fd: Some(5),
            },
        ))
        .expect("the record fits");
        let at = |offset: u32| 8 + offset as usize;
        assert_eq!(
            guest[at(0)..at(4)],
            77_u32.to_le_bytes(),
            "the pid opens the record"
        );
        let option = |offset: u32| {
            (
                guest[at(offset)],
                u32::from_le_bytes(
                    guest[at(offset + WASIX_OPTION_UNION_U32_OFFSET)
                        ..at(offset + WASIX_OPTION_UNION_U32_OFFSET + 4)]
                        .try_into()
                        .expect("four bytes"),
                ),
            )
        };
        assert_eq!(option(WASIX_PROCESS_HANDLES_STDIN_OFFSET), (1, 3));
        assert_eq!(option(WASIX_PROCESS_HANDLES_STDOUT_OFFSET), (0, 0));
        assert_eq!(option(WASIX_PROCESS_HANDLES_STDERR_OFFSET), (1, 5));
    }

    /// A record that does not fit in memory is refused, not truncated.
    #[test]
    fn process_handles_past_the_end_of_memory_are_refused() {
        let mut guest = vec![0_u8; 24];
        let memory = memory_over(&mut guest);
        let result = || WasixSpawnResult {
            pid: 1,
            stdin_fd: None,
            stdout_fd: None,
            stderr_fd: None,
        };
        assert_eq!(
            run(wasix_write_process_handles(
                &memory,
                GuestAddress::wasm32(8),
                result()
            )),
            Err(GuestAccessError::OutOfBounds)
        );
    }

    /// A join that reaped a child reports it as the guest's record has
    /// it, and one that found nothing reports so.
    #[test]
    fn a_join_reports_an_exit_and_the_absence_of_one() {
        let mut guest = vec![0xee_u8; 64];
        let memory = memory_over(&mut guest);
        run(wasix_write_join_exit(
            &memory,
            GuestAddress::wasm32(0),
            GuestAddress::wasm32(16),
            9,
            3,
        ))
        .expect("the records fit");
        run(wasix_write_join_nothing(&memory, GuestAddress::wasm32(32))).expect("fits");
        assert_eq!(guest[0], WASIX_OPTION_SOME);
        assert_eq!(guest[4..8], 9_u32.to_le_bytes());
        assert_eq!(guest[16], WASIX_JOIN_STATUS_EXIT_NORMAL);
        assert_eq!(
            guest[16 + WASIX_JOIN_STATUS_UNION_OFFSET as usize..][..2],
            3_u16.to_le_bytes()
        );
        assert_eq!(guest[32], WASIX_JOIN_STATUS_NOTHING);
        assert_eq!(
            guest[32 + WASIX_JOIN_STATUS_UNION_OFFSET as usize..][..2],
            0_u16.to_le_bytes()
        );
    }

    /// Signal dispositions are decoded one record at a time; an unknown
    /// signal or action is `EINVAL`, a null array with a count is
    /// `EFAULT`, and a count with no array is `EINVAL`.
    #[test]
    fn signal_dispositions_decode_and_refuse_what_is_not_one() {
        let mut guest = vec![0_u8; 32];
        guest[8..12].copy_from_slice(&[
            2,
            WASIX_SIGNAL_DISPOSITION_IGNORE,
            15,
            WASIX_SIGNAL_DISPOSITION_DEFAULT,
        ]);
        let memory = memory_over(&mut guest);
        assert_eq!(
            run(wasix_read_signal_dispositions(
                &memory,
                GuestAddress::wasm32(8),
                2
            )),
            Ok(vec![
                WasixSignalDisposition {
                    signal: 2,
                    action: WasixSignalDispositionAction::Ignore,
                },
                WasixSignalDisposition {
                    signal: 15,
                    action: WasixSignalDispositionAction::Default,
                },
            ])
        );
        assert_eq!(
            run(wasix_read_signal_dispositions(
                &memory,
                GuestAddress::wasm32(4),
                1
            )),
            Err(P1Stop::Errno(p1::errno::INVAL)),
            "signal 0 is no signal"
        );
        assert_eq!(
            run(wasix_read_signal_dispositions(
                &memory,
                GuestAddress::wasm32(0),
                1
            )),
            Err(P1Stop::Errno(p1::errno::FAULT)),
            "a null array with a count"
        );
        assert_eq!(
            run(wasix_read_signal_dispositions(
                &memory,
                GuestAddress::wasm32(0),
                0
            )),
            Ok(Vec::new())
        );
        assert_eq!(
            run(wasix_read_signal_dispositions(
                &memory,
                GuestAddress::wasm32(8),
                0
            )),
            Err(P1Stop::Errno(p1::errno::INVAL))
        );
        assert_eq!(
            run(wasix_read_signal_dispositions(
                &memory,
                GuestAddress::wasm32(31),
                1
            )),
            Err(P1Stop::Errno(p1::errno::FAULT)),
            "the record's action byte is past the end of memory"
        );
    }
}
