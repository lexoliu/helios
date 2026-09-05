use super::*;

pub(super) async fn wasix_thread_sleep<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    duration: i64,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Ok(duration) = u64::try_from(duration) else {
        return p1::errno::INVAL;
    };
    caller
        .data()
        .sleep_for(Duration::from_nanos(duration))
        .await;
    p1::errno::SUCCESS
}

pub(super) fn wasix_thread_id<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    ret_tid: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_u32(caller, memory, ret_tid, caller.data().thread_id)
}

pub(super) async fn wasix_thread_spawn_v2<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    args: u32,
    ret_tid: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let mut thread_start = [0_u8; WASIX_THREAD_START_SIZE];
    if p1_read_memory_into(caller, memory, args, &mut thread_start).is_err() {
        return p1::errno::FAULT;
    }
    let Some(imported_memory) = caller.data().imported_memory.clone() else {
        return p1::errno::NOTSUP;
    };
    let Some(compiled) = caller.data().current_core_module.clone() else {
        return p1::errno::NOTSUP;
    };
    if !compiled
        .module
        .exports()
        .any(|export| export.name() == "wasi_thread_start")
    {
        return p1::errno::NOTCAPABLE;
    }
    let tid = match caller.data_mut().allocate_thread_id() {
        Ok(tid) => tid,
        Err(errno) => return errno,
    };
    let status = p1_write_u32(caller, memory, ret_tid, tid);
    if status != p1::errno::SUCCESS {
        return status;
    }

    let (exit_tx, exit_rx) = futures::channel::oneshot::channel();
    let cpu = caller.data().cpu.clone();
    let timer = caller.data().timer();
    let spawner = caller.data().spawner.clone();
    let runtime_state = caller.data().runtime_state.clone();
    let instance = caller.data().instance().clone();
    let parent_instance_id = caller.data().parent_instance_id;
    let arguments = caller.data().arguments.clone();
    let environment = caller.data().environment.clone();
    let authority = caller.data().authority.clone();
    let output_mode = caller.data().output_mode.clone();
    let read_serial = caller.data().read_serial;
    let write_serial = caller.data().write_serial;
    let filesystem = caller.data().filesystem.snapshot();
    let descriptors = caller.data().descriptors.clone();
    let signal_state = caller.data().signal_state.clone();
    let wasix_abi = caller.data().wasix_abi;
    let engine = compiled.module.engine().clone();

    let mut store_data = Preview1ProgramStore::new(
        cpu,
        timer,
        spawner.clone(),
        runtime_state,
        instance,
        parent_instance_id,
        arguments,
        environment,
        authority,
        output_mode,
        read_serial,
        write_serial,
        Some(imported_memory.clone()),
        Some(filesystem),
        Some(descriptors),
        signal_state.clone(),
        caller.data().signal_dispositions.clone(),
        Some(compiled.clone()),
        wasix_abi,
    );
    store_data.set_thread_id(tid);
    // Spawn before the thread is recorded: a thread the executor has no
    // capacity for must not be left in the table for `thread_join` to
    // wait on forever.
    if let Err(error) = spawner.try_spawn_detached(async move {
        let code = run_wasix_thread(engine, compiled, imported_memory, store_data, tid, args).await;
        let _ = exit_tx.send(code);
    }) {
        tracing::warn!(
            target: "helios_kernel::program",
            %error,
            "refused a wasix thread: the executor's instance share is full"
        );
        return p1::errno::NOMEM;
    }
    caller.data_mut().insert_thread(tid, signal_state, exit_rx);
    p1::errno::SUCCESS
}

pub(super) async fn run_wasix_thread<CpuImpl, HostFs>(
    engine: wasmtime::Engine,
    compiled: Arc<WasmtimeCompiledCoreModule>,
    imported_memory: SharedMemory,
    store_data: Preview1ProgramStore<CpuImpl, HostFs>,
    tid: u32,
    args: u32,
) -> u32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let mut store = wasmtime::Store::new(&engine, store_data);
    configure_preview1_program_store(&mut store);

    let mut linker = CoreLinker::new(&engine);
    if add_preview1_program_imports(&mut linker).is_err() {
        return u32::from(p1::errno::IO as u16);
    }
    if define_imported_shared_memory(&mut linker, &store, &compiled.module, imported_memory)
        .is_err()
    {
        return u32::from(p1::errno::IO as u16);
    }
    let instance = match linker.instantiate_async(&mut store, &compiled.module).await {
        Ok(instance) => instance,
        Err(error) => {
            tracing::error!(tid, "wasix thread instantiate failed: {error:#}");
            return u32::from(p1::errno::IO as u16);
        }
    };
    let start = match instance.get_typed_func::<(i32, i32), ()>(&mut store, "wasi_thread_start") {
        Ok(start) => start,
        Err(error) => {
            tracing::error!(tid, "wasix thread start export lookup failed: {error:#}");
            return u32::from(p1::errno::NOTCAPABLE as u16);
        }
    };
    let tid_i32 = match i32::try_from(tid) {
        Ok(tid) => tid,
        Err(_) => return u32::from(p1::errno::OVERFLOW as u16),
    };
    let args_i32 = i32::from_ne_bytes(args.to_ne_bytes());
    let result = loop {
        let result = start.call_async(&mut store, (tid_i32, args_i32)).await;
        match handle_wasix_asyncify_completion(&mut store, &instance).await {
            Ok(true) => continue,
            Ok(false) => break result,
            Err(error) => {
                tracing::error!(tid, "wasix thread asyncify completion failed: {error}");
                return u32::from(p1::errno::IO as u16);
            }
        }
    };
    match result {
        Ok(()) => store.data_mut().take_requested_exit().unwrap_or(0),
        Err(error) => match store.data_mut().take_requested_exit() {
            Some(code) => code,
            None => {
                tracing::error!(tid, "wasix thread failed: {error:#}");
                u32::from(p1::errno::IO as u16)
            }
        },
    }
}

pub(super) async fn wasix_thread_join<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    tid: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if tid == caller.data().thread_id {
        return p1::errno::INVAL;
    }
    let Some(index) = caller.data().find_thread_index(tid) else {
        return p1::errno::SRCH;
    };
    if caller.data_mut().poll_thread_exit(index).is_some() {
        caller.data_mut().threads.swap_remove(index);
        return p1::errno::SUCCESS;
    }
    let Some(exit) = caller.data_mut().threads[index].exit.take() else {
        caller.data_mut().threads.swap_remove(index);
        return p1::errno::SUCCESS;
    };
    let code = match exit.await {
        Ok(code) => code,
        Err(_) => u32::from(p1::errno::IO as u16),
    };
    let Some(index) = caller.data().find_thread_index(tid) else {
        return p1::errno::SUCCESS;
    };
    if code == 0 {
        caller.data_mut().threads.swap_remove(index);
        p1::errno::SUCCESS
    } else {
        caller.data_mut().threads[index].completed = Some(code);
        p1::errno::SUCCESS
    }
}

pub(super) fn wasix_thread_parallelism<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    ret_parallelism: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let parallelism = match u32::try_from(caller.data().cpu.processor_count()) {
        Ok(parallelism) => parallelism,
        Err(_) => return p1::errno::OVERFLOW,
    };
    p1_write_u32(caller, memory, ret_parallelism, parallelism)
}

pub(super) fn wasix_thread_signal<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    tid: i32,
    signal: i32,
) -> wasmtime::Result<i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if !(0..=31).contains(&signal) {
        return Ok(p1::errno::INVAL);
    };
    let signal = signal as u32;
    if tid == 0 || u32::try_from(tid).ok() == Some(caller.data().thread_id) {
        caller
            .data_mut()
            .request_exit(128u32.saturating_add(signal));
        return Err(wasmtime::Error::new(Preview1Exit));
    }
    let Ok(tid) = u32::try_from(tid) else {
        return Ok(p1::errno::INVAL);
    };
    let Some(index) = caller.data().find_thread_index(tid) else {
        return Ok(p1::errno::SRCH);
    };
    if caller.data().threads[index].completed.is_some() {
        return Ok(p1::errno::SRCH);
    }
    caller.data().threads[index].signal_state.raise(signal);
    Ok(p1::errno::SUCCESS)
}

pub(super) fn wasix_thread_exit<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    code: u32,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    caller.data_mut().request_exit(code);
    Err(wasmtime::Error::new(Preview1Exit))
}

pub(super) fn wasix_stack_bounds_from_caller<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
) -> Result<(u32, u32, u32), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let stack_lower = wasix_global_u32_from_caller(caller, "__data_end")?;
    let stack_upper = wasix_global_u32_from_caller(caller, "__heap_base")?;
    let stack_pointer = wasix_global_u32_from_caller(caller, "__stack_pointer")?;
    Ok((stack_lower, stack_upper, stack_pointer))
}

pub(super) fn wasix_global_u32_from_caller<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    name: &str,
) -> Result<u32, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let global = caller
        .get_export(name)
        .and_then(|export| export.into_global())
        .ok_or(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::UnwindExportInvalid,
        })?;
    match global.get(&mut *caller) {
        Val::I32(value) => Ok(value as u32),
        _ => Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryTypeMismatch,
        }),
    }
}

pub(super) async fn wasix_call_asyncify_start_unwind<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    data: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(function) = caller
        .get_export("asyncify_start_unwind")
        .and_then(|export| export.into_func())
    else {
        return p1::errno::NOTSUP;
    };
    let Ok(function) = function.typed::<i32, ()>(&mut *caller) else {
        return p1::errno::NOTSUP;
    };
    function
        .call_async(&mut *caller, data as i32)
        .await
        .map_or(p1::errno::NOTSUP, |_| p1::errno::SUCCESS)
}

pub(super) async fn wasix_call_asyncify_stop_rewind<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(function) = caller
        .get_export("asyncify_stop_rewind")
        .and_then(|export| export.into_func())
    else {
        return p1::errno::NOTSUP;
    };
    let Ok(function) = function.typed::<(), ()>(&mut *caller) else {
        return p1::errno::NOTSUP;
    };
    function
        .call_async(&mut *caller, ())
        .await
        .map_or(p1::errno::NOTSUP, |_| p1::errno::SUCCESS)
}

pub(super) async fn wasix_stack_checkpoint<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    snapshot: u32,
    ret_value: u32,
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
        return p1_write_u64(caller, memory, ret_value, value);
    }

    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
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
    let status = p1_write_u32(caller, memory, stack_lower, unwind_stack_begin)
        .max(p1_write_u32(caller, memory, stack_lower + 4, stack_pointer))
        .max(p1_write_u64(caller, memory, ret_value, 0));
    if status != p1::errno::SUCCESS {
        return status;
    }
    caller.data_mut().asyncify.phase = WasixAsyncifyPhase::Capturing {
        snapshot,
        ret_value,
        stack_lower,
        stack_upper,
        unwind_stack_begin,
        memory_stack,
        stack_pointer,
    };
    wasix_call_asyncify_start_unwind(caller, stack_lower).await
}

pub(super) async fn wasix_stack_restore<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    snapshot: u32,
    value: u64,
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
    let hash_lower = p1_try_read_u64(caller, memory, snapshot + 8).map_err(wasmtime::Error::new)?;
    let hash_upper =
        p1_try_read_u64(caller, memory, snapshot + 16).map_err(wasmtime::Error::new)?;
    let Ok((stack_lower, stack_upper, stack_pointer)) = wasix_stack_bounds_from_caller(caller)
    else {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::StackBoundsInvalid,
        }));
    };
    if stack_lower >= stack_pointer || stack_pointer > stack_upper {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::StackBoundsInvalid,
        }));
    }
    let unwind_stack_begin = stack_lower
        .checked_add(WASIX_ASYNCIFY_DATA_SIZE)
        .ok_or_else(|| {
            wasmtime::Error::new(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: ProgramExecErrorDetail::StackBoundsInvalid,
            })
        })?;
    let status = p1_write_u32(caller, memory, stack_lower, unwind_stack_begin).max(p1_write_u32(
        caller,
        memory,
        stack_lower + 4,
        stack_pointer,
    ));
    if status != p1::errno::SUCCESS {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        }));
    }
    caller.data_mut().asyncify.phase = WasixAsyncifyPhase::Restoring {
        hash: (u128::from(hash_upper) << 64) | u128::from(hash_lower),
        value: if value == 0 { 1 } else { value },
        stack_lower,
    };
    if wasix_call_asyncify_start_unwind(caller, stack_lower).await != p1::errno::SUCCESS {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::UnwindExportInvalid,
        }));
    }
    Ok(())
}

pub(super) async fn wasix_futex_wait<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    futex: u32,
    expected: u32,
    timeout: u32,
    ret_woken: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let timeout = match wasix_read_optional_timestamp(caller, memory, timeout) {
        Ok(timeout) => timeout,
        Err(errno) => return errno,
    };
    let key = caller.data().futex_key(futex);
    let registration = caller.data().runtime_state.prepare_futex_wait(key);
    let current = match p1_try_read_u32(caller, memory, futex) {
        Ok(value) => value,
        Err(_) => {
            caller
                .data()
                .runtime_state
                .complete_futex_wait(registration);
            return p1::errno::FAULT;
        }
    };
    if current != expected {
        caller
            .data()
            .runtime_state
            .complete_futex_wait(registration);
        return p1::errno::INVAL;
    }

    let notify = registration.notify();
    let woken = match timeout {
        Some(0) => false,
        Some(timeout_nanos) => {
            let wake = notify.notified();
            let sleep = caller.data().sleep_for(Duration::from_nanos(timeout_nanos));
            futures::pin_mut!(wake);
            futures::pin_mut!(sleep);
            matches!(
                futures::future::select(wake, sleep).await,
                futures::future::Either::Left(_)
            )
        }
        None => {
            notify.notified().await;
            true
        }
    };
    caller
        .data()
        .runtime_state
        .complete_futex_wait(registration);
    p1_write_wasix_bool(caller, memory, ret_woken, woken)
}

pub(super) fn wasix_futex_wake<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    futex: u32,
    ret_woken: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    if p1_try_read_u32(caller, memory, futex).is_err() {
        return p1::errno::FAULT;
    }
    let key = caller.data().futex_key(futex);
    let woken = caller.data().runtime_state.wake_futex(key, 1) != 0;
    p1_write_wasix_bool(caller, memory, ret_woken, woken)
}

pub(super) fn wasix_futex_wake_all<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    futex: u32,
    ret_woken: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    if p1_try_read_u32(caller, memory, futex).is_err() {
        return p1::errno::FAULT;
    }
    let key = caller.data().futex_key(futex);
    let woken = caller.data().runtime_state.wake_all_futex(key) != 0;
    p1_write_wasix_bool(caller, memory, ret_woken, woken)
}
