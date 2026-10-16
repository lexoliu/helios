use super::*;

/// The threads of one WASIX process: its main thread's mailbox, every
/// thread any store of the process spawned and nobody has joined, the
/// next thread id, and the exit status a spawned thread chose for the
/// whole process.
///
/// The process owns its threads, not the store that spawned each one,
/// so any thread joins or signals any other and the process's end
/// ([`Self::end`]) reaches all of them, those whose spawner has itself
/// returned included. A spawned thread that exits or takes a fatal
/// signal ends the process the same way main does
/// ([`Self::end_from_thread`]).
///
/// # Concurrency
///
/// Every store of the process, each driven on whichever processor runs
/// it, admits, joins, signals and ends the process through one table.
/// Each of those takes the spin lock for one lookup, push, removal or
/// record and releases it before anything is awaited or raised; no
/// interrupt handler touches the table. A thread's end is published
/// lock-free through its own [`WasixThreadExit`], which the joiner and
/// the process's end wait on outside the lock.
#[derive(Clone)]
pub(super) struct WasixProcessThreads {
    table: Arc<Mutex<WasixThreadTable>>,
    /// The main thread's own mailbox, through which a spawned thread
    /// that ends the process ends main's image.
    main: WasixSignalState,
}

struct WasixThreadTable {
    next_tid: u32,
    threads: Vec<WasixThread>,
    /// Set by [`WasixProcessThreads::end`] and by
    /// [`WasixProcessThreads::end_from_thread`]; no thread is admitted
    /// after, and no second end is recorded.
    ended: bool,
    /// The exit status a spawned thread chose for the process,
    /// recorded before main's image is ended and taken by main once its
    /// call has stopped.
    process_exit: Option<u32>,
}

struct WasixThread {
    tid: u32,
    /// The thread's own mailbox: `thread_signal` raises on it and the
    /// process's end ends the thread through it.
    mailbox: WasixSignalState,
    exit: Arc<WasixThreadExit>,
}

impl WasixProcessThreads {
    /// The table of a new process, whose main thread is thread 0.
    pub(super) fn new() -> Self {
        Self {
            table: Arc::new(Mutex::new(WasixThreadTable {
                next_tid: 1,
                threads: Vec::new(),
                ended: false,
                process_exit: None,
            })),
            main: WasixSignalState::new(),
        }
    }

    /// The main thread's own mailbox, for its store to read.
    pub(super) fn main_mailbox(&self) -> WasixSignalState {
        self.main.clone()
    }

    /// Ends the whole process with status `code` from spawned thread
    /// `tid`, whose own call has ended, as POSIX `exit` and a fatal
    /// signal do from any thread.
    ///
    /// The status is recorded first, then main's image is ended through
    /// its mailbox, which stops main's call and makes main's completion
    /// take the status ([`Self::take_process_exit`]); every other thread is
    /// ended in the same pass, so none runs on after the process's end
    /// and they stop concurrently. Main's completion path then waits for
    /// all of them through [`Self::end`], as for any other end of the
    /// image.
    ///
    /// The first end wins. One that comes after the process has already
    /// ended, by main's own end or another thread's, is dropped: the
    /// thread raising it is being ended with the rest.
    pub(super) fn end_from_thread(&self, tid: u32, code: u32) {
        let others: Vec<WasixSignalState> = {
            let mut table = self.table.lock();
            if table.ended {
                return;
            }
            table.ended = true;
            table.process_exit = Some(code);
            table
                .threads
                .iter()
                .filter(|thread| thread.tid != tid)
                .map(|thread| thread.mailbox.clone())
                .collect()
        };
        // Recorded under the lock before main is stopped, so main finds
        // the end once its call meets the stop.
        self.main.end_thread();
        for mailbox in &others {
            mailbox.end_thread();
        }
    }

    /// The exit status a spawned thread recorded for the process, taken
    /// by main once its call has stopped on its mailbox's end.
    pub(super) fn take_process_exit(&self) -> Option<u32> {
        self.table.lock().process_exit.take()
    }

    /// Admits a thread whose mailbox is `mailbox` and whose end `exit`
    /// will publish, and returns its id. A process that has ended admits
    /// nothing: the caller is one of its threads, ended already, and it
    /// gets `AGAIN`, `pthread_create`'s answer for a thread it cannot
    /// have.
    pub(super) fn admit(
        &self,
        mailbox: WasixSignalState,
        exit: Arc<WasixThreadExit>,
    ) -> Result<u32, i32> {
        let mut table = self.table.lock();
        if table.ended {
            return Err(p1::errno::AGAIN);
        }
        let tid = table.next_tid;
        table.next_tid = tid.checked_add(1).ok_or(p1::errno::OVERFLOW)?;
        table.threads.push(WasixThread { tid, mailbox, exit });
        Ok(tid)
    }

    /// Removes thread `tid` from the table, if it is still there.
    pub(super) fn withdraw(&self, tid: u32) {
        let mut table = self.table.lock();
        if let Some(index) = table.threads.iter().position(|thread| thread.tid == tid) {
            table.threads.swap_remove(index);
        }
    }

    /// The end of thread `tid`, for a join to wait on.
    pub(super) fn exit_of(&self, tid: u32) -> Option<Arc<WasixThreadExit>> {
        self.table
            .lock()
            .threads
            .iter()
            .find(|thread| thread.tid == tid)
            .map(|thread| thread.exit.clone())
    }

    /// The mailbox of thread `tid`, while it is still running.
    pub(super) fn live_mailbox(&self, tid: u32) -> Option<WasixSignalState> {
        self.table
            .lock()
            .threads
            .iter()
            .find(|thread| thread.tid == tid && thread.exit.code().is_none())
            .map(|thread| thread.mailbox.clone())
    }

    /// Ends every thread the process still has and returns once each one
    /// has ended, its store, fiber stack and registry handle dropped.
    ///
    /// Called once, by the process's main thread after its own call has
    /// ended and before the process's completion is reported, whether
    /// main ended on its own or a spawned thread ended it
    /// ([`Self::end_from_thread`]). The table is closed and emptied
    /// under the lock, so a thread admitted later cannot exist; then
    /// every thread is ended (again, harmlessly, for those a spawned
    /// thread's end already reached), and only then is any
    /// awaited, so the threads stop concurrently and the wait is the
    /// slowest one's. A parked thread is reached through the wake its
    /// turn is armed on, which drops the call it is parked in, and a
    /// running one through its interrupt check (see
    /// [`WasixSignalState::end_thread`]).
    pub(super) async fn end(&self) {
        let threads = {
            let mut table = self.table.lock();
            table.ended = true;
            core::mem::take(&mut table.threads)
        };
        for thread in &threads {
            thread.mailbox.end_thread();
        }
        futures::future::join_all(threads.iter().map(|thread| thread.exit.wait())).await;
    }
}

/// The end of one WASIX thread: its exit status, published once, after
/// the thread's store is gone.
///
/// # Concurrency
///
/// The thread's task publishes from whichever processor ran it; any
/// number of waiters (a joiner, the process's end) wait on any other.
/// The status is one atomic and the broadcast stores nothing, so a
/// waiter arms its wait before it reads the status.
pub(super) struct WasixThreadExit {
    status: AtomicU64,
    ended: crate::Notify,
}

impl WasixThreadExit {
    const RUNNING: u64 = u64::MAX;

    pub(super) fn new() -> Self {
        Self {
            status: AtomicU64::new(Self::RUNNING),
            ended: crate::Notify::new(),
        }
    }

    /// Publishes the thread's exit status and wakes every waiter.
    pub(super) fn publish(&self, code: u32) {
        self.status.store(u64::from(code), AtomicOrdering::Release);
        self.ended.notify_all();
    }

    /// The thread's exit status, once it has ended.
    pub(super) fn code(&self) -> Option<u32> {
        match self.status.load(AtomicOrdering::Acquire) {
            Self::RUNNING => None,
            code => Some(code as u32),
        }
    }

    /// Waits for the thread's end and returns its exit status.
    pub(super) async fn wait(&self) -> u32 {
        loop {
            // Armed before the status is read, so a publish between the
            // read and the park still completes the wait.
            let ended = self.ended.notified();
            if let Some(code) = self.code() {
                return code;
            }
            ended.await;
        }
    }
}

pub(super) async fn wasix_thread_sleep<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    duration: i64,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

pub(super) fn wasix_thread_id<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    ret_tid: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_u32(caller, memory, ret_tid, caller.data().thread_id)
}

pub(super) async fn wasix_thread_spawn_v2<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    args: u32,
    ret_tid: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

    let cpu = caller.data().cpu.clone();
    let timer = caller.data().timer();
    let spawner = caller.data().spawner.clone();
    let runtime_state = caller.data().runtime_state.clone();
    let interrupts = caller.data().interrupts.clone();
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
    let threads = caller.data().threads.clone();
    let engine = compiled.module.engine().clone();

    let mut store_data = Preview1ProgramStore::new(
        cpu,
        timer,
        spawner.clone(),
        runtime_state,
        interrupts,
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
        signal_state,
        // The thread's own mailbox, which the table below holds.
        WasixSignalState::new(),
        caller.data().signal_dispositions.clone(),
        Some(compiled.clone()),
        wasix_abi,
        threads.clone(),
    );
    // The thread joins its process's table before its task exists, so
    // the process's end cannot miss it: an end that came first refuses
    // it here, and one that comes later finds it in the table and ends
    // it, whether or not its task has started.
    let exit = Arc::new(WasixThreadExit::new());
    let tid = match threads.admit(store_data.thread_signals.clone(), exit.clone()) {
        Ok(tid) => tid,
        Err(errno) => return errno,
    };
    store_data.set_thread_id(tid);
    let status = p1_write_u32(caller, memory, ret_tid, tid);
    if status != p1::errno::SUCCESS {
        threads.withdraw(tid);
        exit.publish(u32::from(status as u16));
        return status;
    }

    let thread_exit = exit.clone();
    if let Err(error) = spawner.try_spawn_detached(async move {
        let code = run_wasix_thread(engine, compiled, imported_memory, store_data, tid, args).await;
        // Published only now, with the thread's store, fiber stack and
        // registry handle already dropped inside `run_wasix_thread`.
        thread_exit.publish(code);
    }) {
        tracing::warn!(
            target: "helios_kernel::program",
            %error,
            "refused a wasix thread: the executor's instance share is full"
        );
        // The thread never ran. Leave no entry for `thread_join` to wait
        // on, and publish its end for a process end that already took
        // the entry out of the table.
        threads.withdraw(tid);
        exit.publish(u32::from(p1::errno::NOMEM as u16));
        return p1::errno::NOMEM;
    }
    p1::errno::SUCCESS
}

pub(super) async fn run_wasix_thread<CpuImpl, Net, HostFs>(
    engine: wasmtime::Engine,
    compiled: Arc<WasmtimeCompiledCoreModule>,
    imported_memory: SharedMemory,
    store_data: Preview1ProgramStore<CpuImpl, Net, HostFs>,
    tid: u32,
    args: u32,
) -> u32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let mut store = wasmtime::Store::new(&engine, store_data);
    let interrupts = configure_preview1_program_store(&mut store);

    let mut linker = CoreLinker::new(&engine);
    if add_preview1_program_imports(&mut linker).is_err() {
        return u32::from(p1::errno::IO as u16);
    }
    if define_imported_shared_memory(&mut linker, &store, &compiled.module, imported_memory)
        .is_err()
    {
        return u32::from(p1::errno::IO as u16);
    }
    let turns = store.data().guest_turns();
    let instance = match interrupts
        .drive(turns.drive(linker.instantiate_async(&mut store, &compiled.module)))
        .await
    {
        Ok(instance) => instance,
        Err(error) if WasixThreadEnded::is(&error) => return WasixThreadEnded::EXIT_CODE,
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
        let turns = store.data().guest_turns();
        let result = interrupts
            .drive(turns.drive(start.call_async(&mut store, (tid_i32, args_i32))))
            .await;
        if result.as_ref().is_err_and(WasixThreadEnded::is) {
            break result;
        }
        match handle_wasix_asyncify_completion(&mut store, &interrupts, &instance).await {
            Ok(true) => continue,
            Ok(false) => break result,
            // The process ended while the thread was between an unwind
            // and its rewind; the rewind's call met the end.
            Err(_) if store.data().thread_signals.thread_ended() => {
                return WasixThreadEnded::EXIT_CODE;
            }
            Err(error) => {
                tracing::error!(tid, "wasix thread asyncify completion failed: {error}");
                return u32::from(p1::errno::IO as u16);
            }
        }
    };
    let requested_exit = store.data_mut().take_requested_exit();
    match wasix_thread_call_end(result, requested_exit) {
        Ok(WasixThreadCallEnd::Thread(code)) => code,
        Ok(WasixThreadCallEnd::Process(code)) => {
            store.data().threads.end_from_thread(tid, code);
            code
        }
        Err(error) => {
            tracing::error!(tid, "wasix thread failed: {error:#}");
            u32::from(p1::errno::IO as u16)
        }
    }
}

/// How a spawned thread's call ends: the thread alone, or its process.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum WasixThreadCallEnd {
    /// The thread ends alone with this status.
    Thread(u32),
    /// The thread ends its whole process with this status
    /// ([`WasixProcessThreads::end_from_thread`]).
    Process(u32),
}

/// What a spawned thread's call that ended in `result` ends, given the
/// status the guest `requested`.
///
/// Returning from the start function, `thread_exit`, a signal raised on
/// this thread alone (`thread_signal`) and the end of the thread's
/// process end the thread alone. `proc_exit`, `proc_raise` and
/// a fatal signal's default action end the whole process, as POSIX
/// `exit` and a signal do from any thread, with the status main would
/// take from them (`run_core_module_image`): the signal's, then the
/// requested one. Anything else is the thread's failure, handed back.
pub(super) fn wasix_thread_call_end(
    result: wasmtime::Result<()>,
    requested_exit: Option<u32>,
) -> wasmtime::Result<WasixThreadCallEnd> {
    let error = match result {
        Ok(()) => return Ok(WasixThreadCallEnd::Thread(requested_exit.unwrap_or(0))),
        Err(error) => error,
    };
    if WasixThreadEnded::is(&error) {
        return Ok(WasixThreadCallEnd::Thread(WasixThreadEnded::EXIT_CODE));
    }
    if WasixThreadExitRequested::is(&error) {
        let code = requested_exit.expect("thread_exit records its status before it unwinds");
        return Ok(WasixThreadCallEnd::Thread(code));
    }
    if let Some(exit) = error.downcast_ref::<WasixThreadSignalExit>() {
        return Ok(WasixThreadCallEnd::Thread(exit.exit_code()));
    }
    match preview1_exit_status(&error, requested_exit) {
        Some(code) => Ok(WasixThreadCallEnd::Process(code)),
        None => Err(error),
    }
}

pub(super) async fn wasix_thread_join<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    tid: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    if tid == caller.data().thread_id {
        return p1::errno::INVAL;
    }
    let threads = caller.data().threads.clone();
    let Some(exit) = threads.exit_of(tid) else {
        return p1::errno::SRCH;
    };
    // No borrow of the store and no lock is held across this wait: the
    // exit cell is the thread's own, shared with the table.
    exit.wait().await;
    threads.withdraw(tid);
    p1::errno::SUCCESS
}

pub(super) fn wasix_thread_parallelism<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    ret_parallelism: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

pub(super) fn wasix_thread_signal<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    tid: i32,
    signal: i32,
) -> wasmtime::Result<i32>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    if !(0..=31).contains(&signal) {
        return Ok(p1::errno::INVAL);
    };
    let signal = signal as u32;
    // A signal to the calling thread itself ends that thread alone, as
    // one raised on its mailbox would.
    if u32::try_from(tid).ok() == Some(caller.data().thread_id) {
        return Err(wasmtime::Error::new(WasixThreadSignalExit { signal }));
    }
    // A spawned thread signalling main: main's end is its process's, so
    // the process ends with the signal's status.
    if tid == 0 {
        caller
            .data_mut()
            .request_exit(128u32.saturating_add(signal));
        return Err(wasmtime::Error::new(Preview1Exit));
    }
    let Ok(tid) = u32::try_from(tid) else {
        return Ok(p1::errno::INVAL);
    };
    let Some(mailbox) = caller.data().threads.live_mailbox(tid) else {
        return Ok(p1::errno::SRCH);
    };
    mailbox.raise(signal);
    Ok(p1::errno::SUCCESS)
}

pub(super) fn wasix_thread_exit<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    code: u32,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    caller.data_mut().request_exit(code);
    Err(wasmtime::Error::new(WasixThreadExitRequested))
}

pub(super) fn wasix_stack_bounds_from_caller<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
) -> Result<(u32, u32, u32), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let stack_lower = wasix_global_u32_from_caller(caller, "__data_end")?;
    let stack_upper = wasix_global_u32_from_caller(caller, "__heap_base")?;
    let stack_pointer = wasix_global_u32_from_caller(caller, "__stack_pointer")?;
    Ok((stack_lower, stack_upper, stack_pointer))
}

pub(super) fn wasix_global_u32_from_caller<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    name: &str,
) -> Result<u32, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

pub(super) async fn wasix_call_asyncify_start_unwind<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    data: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

pub(super) async fn wasix_call_asyncify_stop_rewind<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

pub(super) async fn wasix_stack_checkpoint<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    snapshot: u32,
    ret_value: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

pub(super) async fn wasix_stack_restore<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    snapshot: u32,
    value: u64,
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

pub(super) async fn wasix_futex_wait<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    futex: u32,
    expected: u32,
    timeout: u32,
    ret_woken: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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
    // The wait is armed before the guest word is compared. `futex_wake`
    // stores a permit this wait can still claim, but `futex_wake_all`
    // broadcasts to the waits that already exist and banks nothing, so a
    // wake-all landing between the comparison and the park is only
    // observed by a wait that was armed first.
    let wake = registration.notify().notified();
    let current = match p1_try_read_u32(caller, memory, futex) {
        Ok(value) => value,
        Err(_) => {
            drop(wake);
            caller
                .data()
                .runtime_state
                .complete_futex_wait(registration);
            return p1::errno::FAULT;
        }
    };
    if current != expected {
        drop(wake);
        caller
            .data()
            .runtime_state
            .complete_futex_wait(registration);
        return p1::errno::INVAL;
    }

    let woken = match timeout {
        Some(0) => {
            drop(wake);
            false
        }
        Some(timeout_nanos) => {
            let sleep = caller.data().sleep_for(Duration::from_nanos(timeout_nanos));
            futures::pin_mut!(wake);
            futures::pin_mut!(sleep);
            matches!(
                futures::future::select(wake, sleep).await,
                futures::future::Either::Left(_)
            )
        }
        None => {
            wake.await;
            true
        }
    };
    caller
        .data()
        .runtime_state
        .complete_futex_wait(registration);
    p1_write_wasix_bool(caller, memory, ret_woken, woken)
}

pub(super) fn wasix_futex_wake<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    futex: u32,
    ret_woken: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

pub(super) fn wasix_futex_wake_all<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    futex: u32,
    ret_woken: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
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

#[cfg(test)]
mod tests {
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::component::GuestTurns;
    use crate::test_support::{ManualClockCpu, TestCpu, TestRuntimeState};
    use crate::wasmtime_adapter::engine::build_component_engine_for_platform;
    use crate::{InstanceActivity, InstanceRegistry};

    /// Counts the wakes a task receives.
    #[derive(Default)]
    struct CountingWaker {
        wakes: AtomicUsize,
    }

    impl CountingWaker {
        fn wakes(&self) -> usize {
            self.wakes.load(Ordering::Acquire)
        }
    }

    impl std::task::Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.wakes.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// The turns of one thread's store: its own mailbox, then its
    /// process's.
    fn thread_turns(
        activity: &InstanceActivity,
        mailbox: &WasixSignalState,
        process: &WasixSignalState,
    ) -> GuestTurns<ManualClockCpu, TestRuntimeState, WasixStoreSignals> {
        GuestTurns::new(
            activity.turns(),
            ManualClockCpu::new(),
            TestRuntimeState::default(),
            WasixStoreSignals::new(mailbox.clone(), process.clone()),
        )
    }

    /// A process that ends with two threads still live, one parked on
    /// its own fiber stack the way `memory.atomic.wait` parks and one
    /// suspended in a host future that never resolves, ends both before
    /// it waits for either, and waits for both. Each thread's turn is
    /// woken by the end and drops its call, which unwinds the fiber; the
    /// fiber stacks, the stores and the registry entry all come back.
    #[test]
    fn a_process_end_stops_every_thread_before_it_waits_and_reclaims_them() {
        let engine = build_component_engine_for_platform(&TestCpu::without_entropy())
            .expect("the component engine builds on the test platform");
        let live_stacks = || {
            engine
                .pooling_allocator_metrics()
                .expect("the component engine uses the pooling allocator")
                .stacks()
        };
        let registry = InstanceRegistry::new();
        let instance = registry.register("threaded", 0);
        let process = WasixSignalState::new();
        let threads = WasixProcessThreads::new();

        let parked_mailbox = WasixSignalState::new();
        let parked_exit = Arc::new(WasixThreadExit::new());
        let parked_tid = threads
            .admit(parked_mailbox.clone(), parked_exit.clone())
            .expect("a live process admits a thread");
        let parked_activity = InstanceActivity::new(instance.clone(), 0);
        let parked_turns = thread_turns(&parked_activity, &parked_mailbox, &process);
        let mut parked_store = wasmtime::Store::new(&engine, parked_activity);
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let parked = wasmtime::Func::wrap(
            &mut parked_store,
            move |_: wasmtime::Caller<'_, InstanceActivity>| -> wasmtime::Result<()> {
                // SAFETY: this frame runs on the guest call's fiber, holds
                // no store borrow across the block, and the future borrows
                // nothing on the fiber.
                match unsafe { wasmtime::block_on_current_fiber(core::future::pending::<()>()) } {
                    Err(wasmtime::BlockOnCurrentFiberError::Cancelled(_)) => {
                        flag.store(true, Ordering::Release);
                        Err(wasmtime::Error::new(wasmtime::AtomicWaitCancelled))
                    }
                    other => panic!("the parked fiber is only ever cancelled, got {other:?}"),
                }
            },
        );

        let sleeping_mailbox = WasixSignalState::new();
        let sleeping_exit = Arc::new(WasixThreadExit::new());
        let sleeping_tid = threads
            .admit(sleeping_mailbox.clone(), sleeping_exit.clone())
            .expect("a live process admits a thread");
        assert_ne!(parked_tid, sleeping_tid, "thread ids are the process's");
        let sleeping_activity = InstanceActivity::new(instance.clone(), 0);
        let sleeping_turns = thread_turns(&sleeping_activity, &sleeping_mailbox, &process);
        let mut sleeping_store = wasmtime::Store::new(&engine, sleeping_activity);
        let sleeping = wasmtime::Func::wrap_async(
            &mut sleeping_store,
            |_: wasmtime::Caller<'_, InstanceActivity>, (): ()| {
                Box::new(core::future::pending::<()>())
            },
        );

        let parked_wakes = Arc::new(CountingWaker::default());
        let parked_waker = Waker::from(parked_wakes.clone());
        let mut parked_cx = Context::from_waker(&parked_waker);
        let sleeping_wakes = Arc::new(CountingWaker::default());
        let sleeping_waker = Waker::from(sleeping_wakes.clone());
        let mut sleeping_cx = Context::from_waker(&sleeping_waker);
        let end_wakes = Arc::new(CountingWaker::default());
        let end_waker = Waker::from(end_wakes.clone());
        let mut end_cx = Context::from_waker(&end_waker);

        // Boxed, so each call can be dropped before its store, as the
        // thread's task drops them, while the other thread's call lives.
        let mut parked_call =
            Box::pin(parked_turns.drive(parked.call_async(&mut parked_store, &[], &mut [])));
        let mut sleeping_call =
            Box::pin(sleeping_turns.drive(sleeping.call_async(&mut sleeping_store, &[], &mut [])));
        assert!(parked_call.as_mut().poll(&mut parked_cx).is_pending());
        assert!(sleeping_call.as_mut().poll(&mut sleeping_cx).is_pending());
        assert_eq!(live_stacks(), 2, "each live thread holds its fiber stack");

        let mut end = pin!(threads.end());
        assert!(end.as_mut().poll(&mut end_cx).is_pending());
        assert!(
            parked_mailbox.thread_ended() && sleeping_mailbox.thread_ended(),
            "the end reaches every thread before it waits for any"
        );
        assert!(parked_wakes.wakes() >= 1, "the end wakes the parked turn");
        assert!(
            sleeping_wakes.wakes() >= 1,
            "the end wakes the sleeping turn"
        );
        let stray_exit = Arc::new(WasixThreadExit::new());
        assert_eq!(
            threads.admit(WasixSignalState::new(), stray_exit).err(),
            Some(p1::errno::AGAIN),
            "an ended process admits no thread"
        );

        match parked_call.as_mut().poll(&mut parked_cx) {
            Poll::Ready(Err(error)) => assert!(
                WasixThreadEnded::is(&error),
                "the parked call reports its process's end, got {error:#}"
            ),
            other => panic!("the end stops the parked call, got {other:?}"),
        }
        assert!(
            cancelled.load(Ordering::Acquire),
            "the parked frame saw the cancellation and unwound"
        );
        // What the thread's task does once its call has ended: the call
        // and the store go, then the end is published.
        drop(parked_call);
        drop(parked_store);
        parked_exit.publish(WasixThreadEnded::EXIT_CODE);
        assert!(end_wakes.wakes() >= 1, "a thread's end wakes the process's");
        assert!(
            end.as_mut().poll(&mut end_cx).is_pending(),
            "the process's end waits for every thread"
        );

        match sleeping_call.as_mut().poll(&mut sleeping_cx) {
            Poll::Ready(Err(error)) => assert!(
                WasixThreadEnded::is(&error),
                "the sleeping call reports its process's end, got {error:#}"
            ),
            other => panic!("the end stops the sleeping call, got {other:?}"),
        }
        drop(sleeping_call);
        drop(sleeping_store);
        sleeping_exit.publish(WasixThreadEnded::EXIT_CODE);
        assert_eq!(end.as_mut().poll(&mut end_cx), Poll::Ready(()));

        assert_eq!(live_stacks(), 0, "the fiber stacks are back in the pool");
        assert_eq!(
            process.take_pending(),
            None,
            "the process mailbox is untouched"
        );
        drop(instance);
        assert_eq!(registry.snapshot(0), [], "the registry entry is reclaimed");
    }

    /// A thread that ended before its process waits for nothing: the
    /// process's end returns at once, and nothing it ends is woken.
    #[test]
    fn a_process_end_does_not_wait_for_a_thread_that_already_ended() {
        let threads = WasixProcessThreads::new();
        let mailbox = WasixSignalState::new();
        let exit = Arc::new(WasixThreadExit::new());
        threads
            .admit(mailbox.clone(), exit.clone())
            .expect("a live process admits a thread");
        exit.publish(0);

        let mut end = pin!(threads.end());
        assert_eq!(
            end.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(())
        );
    }

    /// The end of a thread is owed to its store at every turn from then
    /// on, ahead of a pending signal, which it neither takes nor loses.
    #[test]
    fn a_threads_end_is_owed_ahead_of_a_signal_and_stays_owed() {
        let process = WasixSignalState::new();
        let thread = WasixSignalState::new();
        let signals = WasixStoreSignals::new(thread.clone(), process.clone());
        thread.raise(15);
        thread.end_thread();

        for _ in 0..2 {
            let stop = signals.owed_stop().expect("an ended thread owes a stop");
            assert!(WasixThreadEnded::is(&stop));
        }
        assert_eq!(thread.take_pending(), Some(15));
    }

    /// A spawned thread that exits ends its whole process: the status is
    /// recorded, main's call, parked forever on its fiber the way
    /// `memory.atomic.wait` parks, is stopped through main's mailbox and
    /// finds the status, and every other thread is ended in the same
    /// pass, the exiting one excepted. No thread is admitted after, a
    /// second thread's exit does not replace the first, and the process's
    /// end then waits for every thread as it does for main's own end.
    #[test]
    fn a_threads_exit_ends_main_and_every_other_thread_with_its_status() {
        let engine = build_component_engine_for_platform(&TestCpu::without_entropy())
            .expect("the component engine builds on the test platform");
        let registry = InstanceRegistry::new();
        let instance = registry.register("threaded", 0);
        let process = WasixSignalState::new();
        let threads = WasixProcessThreads::new();

        let exiting_mailbox = WasixSignalState::new();
        let exiting_exit = Arc::new(WasixThreadExit::new());
        let exiting_tid = threads
            .admit(exiting_mailbox.clone(), exiting_exit.clone())
            .expect("a live process admits a thread");
        let other_mailbox = WasixSignalState::new();
        let other_exit = Arc::new(WasixThreadExit::new());
        let other_tid = threads
            .admit(other_mailbox.clone(), other_exit.clone())
            .expect("a live process admits a thread");

        let main_activity = InstanceActivity::new(instance.clone(), 0);
        let main_turns = thread_turns(&main_activity, &threads.main_mailbox(), &process);
        let mut main_store = wasmtime::Store::new(&engine, main_activity);
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let parked = wasmtime::Func::wrap(
            &mut main_store,
            move |_: wasmtime::Caller<'_, InstanceActivity>| -> wasmtime::Result<()> {
                // SAFETY: this frame runs on the guest call's fiber, holds
                // no store borrow across the block, and the future borrows
                // nothing on the fiber.
                match unsafe { wasmtime::block_on_current_fiber(core::future::pending::<()>()) } {
                    Err(wasmtime::BlockOnCurrentFiberError::Cancelled(_)) => {
                        flag.store(true, Ordering::Release);
                        Err(wasmtime::Error::new(wasmtime::AtomicWaitCancelled))
                    }
                    other => panic!("the parked fiber is only ever cancelled, got {other:?}"),
                }
            },
        );
        let main_wakes = Arc::new(CountingWaker::default());
        let main_waker = Waker::from(main_wakes.clone());
        let mut main_cx = Context::from_waker(&main_waker);
        let mut main_call =
            Box::pin(main_turns.drive(parked.call_async(&mut main_store, &[], &mut [])));
        assert!(main_call.as_mut().poll(&mut main_cx).is_pending());

        threads.end_from_thread(exiting_tid, 7);
        assert!(
            threads.main_mailbox().thread_ended(),
            "the exit ends main's image"
        );
        assert!(
            other_mailbox.thread_ended(),
            "the exit ends every other thread"
        );
        assert!(
            !exiting_mailbox.thread_ended(),
            "the exiting thread is ending on its own"
        );
        assert!(main_wakes.wakes() >= 1, "the exit wakes main's parked turn");
        assert_eq!(
            threads
                .admit(WasixSignalState::new(), Arc::new(WasixThreadExit::new()))
                .err(),
            Some(p1::errno::AGAIN),
            "an exiting process admits no thread"
        );
        threads.end_from_thread(other_tid, 3);

        match main_call.as_mut().poll(&mut main_cx) {
            Poll::Ready(Err(error)) => assert!(
                WasixThreadEnded::is(&error),
                "main's call reports its process's end, got {error:#}"
            ),
            other => panic!("the exit stops main's parked call, got {other:?}"),
        }
        assert!(
            cancelled.load(Ordering::Acquire),
            "main's parked frame saw the cancellation and unwound"
        );
        drop(main_call);
        drop(main_store);
        assert_eq!(
            threads.take_process_exit(),
            Some(7),
            "main takes the first exit's status"
        );
        assert_eq!(threads.take_process_exit(), None, "and takes it once");

        let end_wakes = Arc::new(CountingWaker::default());
        let end_waker = Waker::from(end_wakes.clone());
        let mut end_cx = Context::from_waker(&end_waker);
        let mut end = pin!(threads.end());
        assert!(
            end.as_mut().poll(&mut end_cx).is_pending(),
            "the process's end waits for every thread"
        );
        exiting_exit.publish(7);
        other_exit.publish(WasixThreadEnded::EXIT_CODE);
        assert_eq!(end.as_mut().poll(&mut end_cx), Poll::Ready(()));
        assert_eq!(
            process.take_pending(),
            None,
            "the process mailbox is untouched"
        );
    }

    /// Main's own end comes first: a thread's exit after it is dropped,
    /// and main's status stands.
    #[test]
    fn a_threads_exit_after_mains_end_is_dropped() {
        let threads = WasixProcessThreads::new();
        let mailbox = WasixSignalState::new();
        let exit = Arc::new(WasixThreadExit::new());
        let tid = threads
            .admit(mailbox.clone(), exit.clone())
            .expect("a live process admits a thread");
        let mut end = pin!(threads.end());
        let mut cx = Context::from_waker(Waker::noop());
        assert!(end.as_mut().poll(&mut cx).is_pending());

        threads.end_from_thread(tid, 7);
        assert!(
            !threads.main_mailbox().thread_ended(),
            "main has already ended on its own"
        );
        assert_eq!(threads.take_process_exit(), None);
        exit.publish(7);
        assert_eq!(end.as_mut().poll(&mut cx), Poll::Ready(()));
    }

    /// A spawned thread's call ends the thread alone when it returns,
    /// calls `thread_exit`, takes a signal raised on it alone or is ended
    /// with its process, and ends the
    /// whole process on `proc_exit`, `proc_raise` and a fatal signal,
    /// with the status main would take from the same end.
    #[test]
    fn a_threads_call_end_is_the_threads_or_the_processs() {
        let end = |result: wasmtime::Result<()>, requested| {
            wasix_thread_call_end(result, requested).expect("the end is a status")
        };
        assert_eq!(end(Ok(()), None), WasixThreadCallEnd::Thread(0));
        assert_eq!(
            end(Err(wasmtime::Error::new(WasixThreadEnded)), None),
            WasixThreadCallEnd::Thread(WasixThreadEnded::EXIT_CODE)
        );
        assert_eq!(
            end(Err(wasmtime::Error::new(WasixThreadExitRequested)), Some(5)),
            WasixThreadCallEnd::Thread(5)
        );
        assert_eq!(
            end(
                Err(wasmtime::Error::new(WasixThreadSignalExit { signal: 9 })),
                Some(7)
            ),
            WasixThreadCallEnd::Thread(128 + 9),
            "a signal raised on the thread alone ends the thread alone"
        );
        assert_eq!(
            end(Err(wasmtime::Error::new(Preview1Exit)), Some(7)),
            WasixThreadCallEnd::Process(7),
            "proc_exit ends the process"
        );
        assert_eq!(
            end(
                Err(wasmtime::Error::new(Preview1SignalExit { signal: 15 })),
                Some(7)
            ),
            WasixThreadCallEnd::Process(128 + 15),
            "a fatal signal ends the process with its status"
        );
        assert!(
            wasix_thread_call_end(Err(wasmtime::Error::msg("trap")), None).is_err(),
            "a failure is the thread's own"
        );
    }

    /// A joiner armed before a thread ends is woken by the end and gets
    /// its status; a thread's id leaves the table once it is withdrawn.
    #[test]
    fn a_join_returns_the_status_the_thread_published() {
        let threads = WasixProcessThreads::new();
        let mailbox = WasixSignalState::new();
        let exit = Arc::new(WasixThreadExit::new());
        let tid = threads
            .admit(mailbox, exit.clone())
            .expect("a live process admits a thread");
        let joined = threads.exit_of(tid).expect("a live thread can be joined");
        assert!(threads.live_mailbox(tid).is_some());

        let wakes = Arc::new(CountingWaker::default());
        let waker = Waker::from(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        let mut join = pin!(joined.wait());
        assert!(join.as_mut().poll(&mut cx).is_pending());
        exit.publish(3);
        assert!(wakes.wakes() >= 1, "the end wakes the joiner");
        assert_eq!(join.as_mut().poll(&mut cx), Poll::Ready(3));
        assert!(
            threads.live_mailbox(tid).is_none(),
            "an ended thread takes no signal"
        );

        threads.withdraw(tid);
        assert!(threads.exit_of(tid).is_none());
    }
}
