use super::*;

pub(super) struct Preview1ProgramStore<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    pub(super) cpu: CpuImpl,
    pub(super) timer: crate::Timer<CpuImpl>,
    pub(super) spawner: crate::InstanceSpawner<CpuImpl>,
    pub(super) runtime_state: HostRuntimeState<CpuImpl, Net, HostFs>,
    /// The interrupt routes of the engine this store runs on, which its
    /// threads' stores register with too.
    pub(super) interrupts: Arc<GuestInterrupts>,
    activity: crate::InstanceActivity,
    pub(super) parent_instance_id: Option<crate::InstanceId>,
    pub(super) filesystem: Namespace<HostFs>,
    pub(super) clock: crate::KernelClock<CpuImpl, HostRuntimeState<CpuImpl, Net, HostFs>>,
    pub(super) wall_clock_cap: Option<crate::SetWallClockCap>,
    pub(super) cwd: Option<Preview1Cwd>,
    pub(super) arguments: Vec<String>,
    pub(super) environment: Vec<(String, String)>,
    pub(super) output_mode: OutputMode,
    pub(super) read_serial: crate::SerialReader,
    pub(super) serial_read_buffer: Vec<u8>,
    pub(super) write_serial: crate::DebugSerialWriter,
    pub(super) imported_memory: Option<SharedMemory>,
    pub(super) current_core_module: Option<Arc<WasmtimeCompiledCoreModule>>,
    pub(super) wasix_abi: bool,
    pub(super) entropy: crate::EntropyPool,
    pub(super) authority: ProcessAuthority,
    pub(super) tty_state: WasixTtyState,
    pub(super) signal_callback: Option<String>,
    pub(super) signal_state: WasixSignalState,
    /// Signals raised on this store's thread alone (`thread_signal`),
    /// and the end of its process. Main's is its process table's
    /// [`WasixProcessThreads::main_mailbox`].
    pub(super) thread_signals: WasixSignalState,
    /// The `proc_raise_interval` timer this store asked for, if any.
    ///
    /// A repeating interval sleeps between raises, so nothing but its
    /// owner can end it. The store is that owner: dropping this handle
    /// cancels the task and returns its block to the executor's
    /// instance share, so the timer cannot outlive the instance that
    /// asked for it (#132).
    pub(super) signal_interval: Option<crate::JoinHandle<()>>,
    pub(super) signal_dispositions: Vec<WasixSignalDisposition>,
    pub(super) descriptors: Preview1DescriptorTable<Net>,
    pub(super) asyncify: WasixAsyncifyState<Net>,
    pub(super) children: Vec<WasixChildProcess>,
    pub(super) thread_id: u32,
    /// The threads of this store's process, shared by every store of it.
    pub(super) threads: WasixImageThreads<CpuImpl, Net, HostFs>,
    pub(super) requested_exit: Option<u32>,
    pub(super) exec_replacement: Option<WasixExecReplacement<CpuImpl, Net, HostFs>>,
}

impl<CpuImpl, Net, HostFs> Preview1ProgramStore<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        cpu: CpuImpl,
        timer: crate::Timer<CpuImpl>,
        spawner: crate::InstanceSpawner<CpuImpl>,
        runtime_state: HostRuntimeState<CpuImpl, Net, HostFs>,
        interrupts: Arc<GuestInterrupts>,
        instance: crate::RegisteredInstance,
        parent_instance_id: Option<crate::InstanceId>,
        arguments: Vec<String>,
        environment: Vec<(String, String)>,
        authority: ProcessAuthority,
        output_mode: OutputMode,
        read_serial: crate::SerialReader,
        write_serial: crate::DebugSerialWriter,
        imported_memory: Option<SharedMemory>,
        filesystem: Option<Namespace<HostFs>>,
        descriptors: Option<Preview1DescriptorTable<Net>>,
        signal_state: WasixSignalState,
        thread_signals: WasixSignalState,
        signal_dispositions: Vec<WasixSignalDisposition>,
        current_core_module: Option<Arc<WasmtimeCompiledCoreModule>>,
        wasix_abi: bool,
        threads: WasixImageThreads<CpuImpl, Net, HostFs>,
    ) -> Self {
        let filesystem = filesystem.unwrap_or_else(|| runtime_state.root_namespace().descend());
        let entropy = crate::EntropyPool::derive(runtime_state.root_entropy(), instance.id().raw());
        let descriptors =
            descriptors.unwrap_or_else(|| Preview1DescriptorTable::from_authority(&authority));
        let clock = crate::KernelClock::new(cpu.clone(), runtime_state.clone());
        let wall_clock_cap = authority.derive_set_wall_clock_cap().ok();
        let cwd = preview1_cwd_from_authority(&authority);
        let tty_state = WasixTtyState::from_authority(&authority);
        Self {
            cpu,
            timer,
            spawner,
            runtime_state,
            interrupts,
            activity: crate::InstanceActivity::new(
                instance,
                crate::store_kernel_heap_bytes(size_of::<Self>()),
            ),
            parent_instance_id,
            filesystem,
            clock,
            wall_clock_cap,
            cwd,
            arguments,
            environment,
            output_mode,
            read_serial,
            serial_read_buffer: Vec::new(),
            write_serial,
            imported_memory,
            current_core_module,
            wasix_abi,
            entropy,
            authority,
            tty_state,
            signal_callback: None,
            signal_state,
            thread_signals,
            signal_interval: None,
            signal_dispositions,
            descriptors,
            asyncify: WasixAsyncifyState::new(),
            children: Vec::new(),
            thread_id: 0,
            threads,
            requested_exit: None,
            exec_replacement: None,
        }
    }

    pub(super) fn now_nanos(&self) -> u64 {
        self.runtime_state.uptime_nanos(self.cpu.now().ticks())
    }

    pub(super) fn system_time_nanos(&self) -> u64 {
        self.clock.system_time_nanos()
    }

    pub(super) fn timer(&self) -> crate::Timer<CpuImpl> {
        self.timer.clone()
    }

    pub(super) fn sleep_for(&self, duration: Duration) -> crate::Sleep<CpuImpl> {
        self.timer.sleep_for(duration)
    }

    pub(super) fn futex_key(&self, address: u32) -> crate::FutexKey {
        crate::FutexKey::new(
            crate::ProcessMemoryIdentity::new(self.instance().id().raw()),
            GuestAddress::wasm32(address),
        )
    }

    pub(super) fn set_system_time_nanos(&mut self, nanos: u64) -> i32 {
        let Some(cap) = &self.wall_clock_cap else {
            return p1::errno::NOTCAPABLE;
        };
        self.clock.set_system_time_nanos(cap, nanos);
        p1::errno::SUCCESS
    }

    pub(super) fn require_tty_control(&self) -> i32 {
        self.authority
            .derive_tty_control_cap()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    pub(super) fn require_signal_authority(&self) -> i32 {
        self.authority
            .derive_signal_authority()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    pub(super) fn require_dns_authority(&self) -> i32 {
        self.authority
            .derive_dns_cap()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    pub(super) fn require_tcp_authority(&self) -> i32 {
        self.authority
            .derive_tcp_cap()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    pub(super) fn require_udp_authority(&self) -> i32 {
        self.authority
            .derive_udp_cap()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    pub(super) fn require_socket_authority(&self, authority: WasixSocketAuthority) -> i32 {
        match authority {
            WasixSocketAuthority::LocalOnly => p1::errno::SUCCESS,
            WasixSocketAuthority::Tcp => self.require_tcp_authority(),
            WasixSocketAuthority::Udp => self.require_udp_authority(),
        }
    }

    pub(super) fn require_multicast_authority(&self) -> i32 {
        self.authority
            .derive_multicast_cap()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    pub(super) fn require_hardlink_authority(&self) -> i32 {
        if self.authority.derive_link_source_cap().is_err()
            || self.authority.derive_link_target_directory_cap().is_err()
        {
            return p1::errno::NOTCAPABLE;
        }
        p1::errno::SUCCESS
    }

    pub(super) fn require_symlink_create_authority(&self) -> i32 {
        self.authority
            .derive_symlink_create_cap()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    pub(super) fn require_symlink_read_authority(&self) -> i32 {
        self.authority
            .derive_symlink_read_cap()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    pub(super) fn require_privileged_bind_authority(&self) -> i32 {
        self.authority
            .derive_privileged_bind_cap()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    pub(super) fn require_spawn_authority(&self) -> i32 {
        self.authority
            .derive_spawn_authority()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    pub(super) fn require_exec_authority(&self) -> i32 {
        self.authority
            .derive_exec_authority()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    pub(super) fn require_fork_authority(&self) -> i32 {
        self.authority
            .derive_fork_authority()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    pub(super) fn require_join_authority(&self) -> i32 {
        self.authority
            .derive_join_authority()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    pub(super) fn request_exit(&mut self, code: u32) {
        self.requested_exit = Some(code);
    }

    pub(super) fn request_exec_replacement(
        &mut self,
        replacement: WasixExecReplacement<CpuImpl, Net, HostFs>,
    ) {
        self.exec_replacement = Some(replacement);
    }

    pub(super) fn take_exec_replacement(
        &mut self,
    ) -> Option<WasixExecReplacement<CpuImpl, Net, HostFs>> {
        self.exec_replacement.take()
    }

    pub(super) fn set_thread_id(&mut self, thread_id: u32) {
        self.thread_id = thread_id;
    }

    pub(super) fn take_requested_exit(&mut self) -> Option<u32> {
        self.requested_exit.take()
    }

    /// How the process ends once this store's call ended in `error`: the
    /// signal that ended it, otherwise the status the guest asked for,
    /// otherwise the failure the error is ([`preview1_process_end`]).
    pub(super) fn take_process_end(
        &mut self,
        error: wasmtime::Error,
    ) -> Result<u32, ProgramExecError> {
        preview1_process_end(error, self.requested_exit.take())
    }

    pub(super) fn exec_context(&self) -> ProgramExecContext<CpuImpl, Net, HostFs> {
        ProgramExecContext {
            cpu: self.cpu.clone(),
            timer: self.timer.clone(),
            // A child instance is funded from its own share; what the
            // launch path takes from the parent is the processor.
            spawner: self.spawner.launch_spawner().clone(),
            runtime_state: self.runtime_state.clone(),
            instance_registry: self.runtime_state.instance_registry(),
            parent_instance_id: Some(self.instance().id()),
            read_serial: self.read_serial,
            write_serial: self.write_serial,
        }
    }

    pub(super) fn instance(&self) -> &crate::RegisteredInstance {
        self.activity.instance()
    }

    /// The handle the code driving this store accounts its processor
    /// turns through. Taken before the store is borrowed for the call;
    /// see [`crate::component::GuestTurns::drive`].
    pub(super) fn guest_turns(
        &self,
    ) -> crate::component::GuestTurns<
        CpuImpl,
        HostRuntimeState<CpuImpl, Net, HostFs>,
        WasixStoreSignals,
    > {
        crate::component::GuestTurns::new(
            self.activity.turns(),
            self.cpu.clone(),
            self.runtime_state.clone(),
            self.signals(),
        )
    }

    /// The signals this store answers to: its own thread's, ahead of its
    /// process's.
    pub(super) fn signals(&self) -> WasixStoreSignals {
        WasixStoreSignals::new(self.thread_signals.clone(), self.signal_state.clone())
    }

    /// The interrupt callback for this store: where the end of the
    /// thread's process, a pending WASIX signal and a condemnation reach
    /// a running guest, and where the end of a time slice makes it yield.
    ///
    /// Ending a thread, raising a signal and condemning an instance all
    /// interrupt every store of the instance, so a guest that is running
    /// meets this at its next function entry or loop header. One
    /// suspended in a host call meets none of them here: the turn driving
    /// the store is woken and ends the call, and a call that returns to
    /// the host first meets them where it completes (see
    /// [`crate::component::TurnEpilogue::take_stop`]). The thread's end
    /// comes first, then a signal, then a kill.
    fn on_interrupt(&mut self) -> wasmtime::Result<wasmtime::InterruptAction> {
        if let Some(stop) = self.signals().owed_stop() {
            return Err(stop);
        }
        if let Some(reason) = self.activity.pending_kill() {
            return Err(wasmtime::Error::from(crate::InstanceKilled { reason }));
        }
        Ok(wasmtime::InterruptAction::Yield)
    }

    /// Deliver stdout/stderr bytes to a kernel-served sink, or hand
    /// back the bounded child channel the caller has to push through.
    ///
    /// The split exists because the store is not `Sync`: holding `&self`
    /// across an `.await` would make every host-call future non-`Send`.
    /// Callers take the returned writer — a cheap handle clone — and then
    /// await or `try_write` it outside this borrow, and for a local sink
    /// they offer whatever it could not take again after a yield.
    pub(super) fn route_output(
        &self,
        stream: crate::ComponentOutputStreamKind,
        bytes: &[u8],
    ) -> RoutedOutput {
        if bytes.is_empty() {
            return RoutedOutput::Local(0);
        }
        match self.output_mode.sink(stream) {
            crate::ComponentOutputSink::Local(local) => RoutedOutput::Local(local.write(
                &self.cpu,
                &self.runtime_state,
                self.write_serial,
                bytes,
            )),
            crate::ComponentOutputSink::Child(writer) => RoutedOutput::Child(writer.clone()),
        }
    }

    pub(super) fn output_route(&self, stream: crate::ComponentOutputStreamKind) -> OutputRoute {
        match &self.output_mode {
            OutputMode::Serial => OutputRoute::Serial,
            OutputMode::Trace => OutputRoute::Trace,
            OutputMode::Child {
                stdout_tx,
                stderr_tx,
                ..
            } => match stream {
                crate::ComponentOutputStreamKind::Stdout => OutputRoute::Child(stdout_tx.clone()),
                crate::ComponentOutputStreamKind::Stderr => OutputRoute::Child(stderr_tx.clone()),
            },
            OutputMode::RoutedChild { stdout, stderr, .. } => match stream {
                crate::ComponentOutputStreamKind::Stdout => stdout.clone(),
                crate::ComponentOutputStreamKind::Stderr => stderr.clone(),
            },
        }
    }

    pub(super) fn insert_child(
        &mut self,
        pid: u32,
        signal_state: WasixSignalState,
        exit: futures::channel::oneshot::Receiver<Result<ChildExit, ProgramExecError>>,
    ) {
        self.children.push(WasixChildProcess {
            pid,
            signal_state,
            exit: Some(exit),
            completed: None,
        });
    }

    pub(super) fn poll_child_exit(&mut self, index: usize) -> Result<Option<u32>, i32> {
        if let Some(code) = self.children[index].completed {
            return Ok(Some(code));
        }
        let Some(exit) = self.children[index].exit.as_mut() else {
            return Ok(self.children[index].completed);
        };
        let waker = futures::task::noop_waker_ref();
        let mut context = Context::from_waker(waker);
        match Pin::new(exit).poll(&mut context) {
            Poll::Pending => Ok(None),
            Poll::Ready(Ok(Ok(exit))) => {
                let code = exit.exit_code;
                if let Some(filesystem) = exit.filesystem {
                    self.filesystem.adopt_root_layer(filesystem);
                }
                self.children[index].completed = Some(code);
                self.children[index].exit = None;
                Ok(Some(code))
            }
            Poll::Ready(Ok(Err(_))) | Poll::Ready(Err(_)) => {
                let code = u32::from(p1::errno::IO as u16);
                self.children[index].completed = Some(code);
                self.children[index].exit = None;
                Ok(Some(code))
            }
        }
    }

    pub(super) fn find_child_index(&self, pid: Option<u32>) -> Option<usize> {
        match pid {
            Some(pid) => self.children.iter().position(|child| child.pid == pid),
            None => {
                if let Some(index) = self
                    .children
                    .iter()
                    .position(|child| child.completed.is_some())
                {
                    return Some(index);
                }
                (!self.children.is_empty()).then_some(0)
            }
        }
    }

    pub(super) async fn read_stdin(&mut self, max_bytes: u32) -> Bytes {
        let descriptor = self.descriptors.get_mut(0);
        let Some(Preview1Descriptor::Stdin { carry }) = descriptor else {
            return Bytes::new();
        };
        if carry.is_empty() {
            match &self.output_mode {
                OutputMode::Serial => loop {
                    let wait = self.write_serial.wait_for_debug_serial_input();
                    (self.read_serial)(&mut self.serial_read_buffer, max_bytes);
                    if !self.serial_read_buffer.is_empty() {
                        *carry = Bytes::copy_from_slice(&self.serial_read_buffer);
                        break;
                    }
                    wait.await;
                },
                OutputMode::Trace => {}
                OutputMode::Child { stdin_rx, .. } | OutputMode::RoutedChild { stdin_rx, .. } => {
                    if let Some(bytes) = stdin_rx.read().await {
                        *carry = bytes;
                    }
                }
            }
        }
        take_preview1_carry(carry, max_bytes as usize)
    }

    /// Probe stdin for `poll_oneoff`/`epoll_wait` without blocking.
    ///
    /// Serial-backed stdin waits on the console's receive signal, so the
    /// probe pulls whatever the console already has into the descriptor's
    /// carry; a later `fd_read` drains that carry, so nothing is lost. A
    /// child's stdin is a byte channel that answers directly, and trace
    /// output never delivers input at all.
    pub(super) fn probe_stdin(&mut self, fd: i32) -> P1Readiness {
        match self.descriptors.get(fd) {
            Some(Preview1Descriptor::Stdin { carry }) if !carry.is_empty() => {
                return P1Readiness::Ready {
                    bytes: carry.len() as u64,
                };
            }
            Some(Preview1Descriptor::Stdin { .. }) => {}
            _ => return P1Readiness::Pending,
        }
        // Resolve the mode to a plain value first so the console read below
        // does not overlap the borrow of `output_mode`.
        let channel_readable = match &self.output_mode {
            OutputMode::Serial => None,
            OutputMode::Trace => return P1Readiness::Hangup,
            OutputMode::Child { stdin_rx, .. } | OutputMode::RoutedChild { stdin_rx, .. } => {
                Some(stdin_rx.is_readable())
            }
        };
        if let Some(readable) = channel_readable {
            return if readable {
                P1Readiness::Ready { bytes: 1 }
            } else {
                P1Readiness::Pending
            };
        }

        (self.read_serial)(&mut self.serial_read_buffer, P1_STDIN_PROBE_CAPACITY);
        if self.serial_read_buffer.is_empty() {
            return P1Readiness::Pending;
        }
        let bytes = Bytes::copy_from_slice(&self.serial_read_buffer);
        let len = bytes.len() as u64;
        let Some(Preview1Descriptor::Stdin { carry }) = self.descriptors.get_mut(fd) else {
            return P1Readiness::Pending;
        };
        *carry = bytes;
        P1Readiness::Ready { bytes: len }
    }

    pub(super) async fn read_pipe(&mut self, fd: i32, max_bytes: usize) -> Result<Bytes, i32> {
        let reader = match self.descriptors.get_mut(fd) {
            Some(Preview1Descriptor::PipeRead { reader, carry }) => {
                if !carry.is_empty() {
                    return Ok(take_preview1_carry(carry, max_bytes));
                }
                reader.clone()
            }
            Some(_) => return Err(p1::errno::BADF),
            None => return Err(p1::errno::BADF),
        };

        let bytes = reader.read().await.unwrap_or_default();
        let Some(Preview1Descriptor::PipeRead { carry, .. }) = self.descriptors.get_mut(fd) else {
            return Err(p1::errno::BADF);
        };
        *carry = bytes;
        Ok(take_preview1_carry(carry, max_bytes))
    }

    pub(super) async fn read_socket_pair(
        &mut self,
        fd: i32,
        max_bytes: usize,
    ) -> Result<Bytes, i32> {
        let reader = match self.descriptors.get_mut(fd) {
            Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair {
                reader, carry, ..
            })) => {
                if !carry.is_empty() {
                    return Ok(take_preview1_carry(carry, max_bytes));
                }
                reader.clone()
            }
            Some(Preview1Descriptor::Socket(_)) => return Err(p1::errno::INVAL),
            Some(_) => return Err(p1::errno::NOTSOCK),
            None => return Err(p1::errno::BADF),
        };

        let bytes = reader.read().await.unwrap_or_default();
        let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { carry, .. })) =
            self.descriptors.get_mut(fd)
        else {
            return Err(p1::errno::BADF);
        };
        *carry = bytes;
        Ok(take_preview1_carry(carry, max_bytes))
    }

    pub(super) fn try_read_socket_pair(
        &mut self,
        fd: i32,
        max_bytes: usize,
    ) -> Result<Option<Bytes>, i32> {
        let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { reader, carry, .. })) =
            self.descriptors.get_mut(fd)
        else {
            return match self.descriptors.get(fd) {
                Some(Preview1Descriptor::Socket(_)) => Err(p1::errno::INVAL),
                Some(_) => Err(p1::errno::NOTSOCK),
                None => Err(p1::errno::BADF),
            };
        };
        if !carry.is_empty() {
            return Ok(Some(take_preview1_carry(carry, max_bytes)));
        }
        match reader.try_read() {
            crate::TryRead::Ready(bytes) => {
                *carry = bytes;
                Ok(Some(take_preview1_carry(carry, max_bytes)))
            }
            crate::TryRead::Eof => Ok(Some(Bytes::new())),
            crate::TryRead::Pending => Ok(None),
        }
    }

    pub(super) fn getcwd(&self) -> Result<&str, i32> {
        self.cwd
            .as_ref()
            .map(|cwd| cwd.guest_name.as_str())
            .ok_or(p1::errno::NOTCAPABLE)
    }

    pub(super) async fn chdir(&mut self, path: &str) -> i32 {
        let cwd = match self.resolve_cwd_target(path).await {
            Ok(cwd) => cwd,
            Err(errno) => return errno,
        };
        let cap = match self.derive_cwd_cap(&cwd) {
            Ok(cap) => cap,
            Err(_) => return p1::errno::NOTCAPABLE,
        };
        self.authority.chdir(cap);
        self.cwd = Some(cwd);
        p1::errno::SUCCESS
    }

    pub(super) fn derive_cwd_cap(
        &self,
        cwd: &Preview1Cwd,
    ) -> Result<crate::DirectoryCap, crate::ProcessAuthorityError> {
        self.authority.derive_directory_cap(
            &cwd.descriptor.path,
            &cwd.guest_name,
            descriptor_flags_to_directory_authority(cwd.descriptor.flags),
        )
    }

    pub(super) async fn resolve_cwd_target(&self, path: &str) -> Result<Preview1Cwd, i32> {
        let (guest_name, base, suffix) = if path.starts_with('/') {
            let guest_name =
                crate::resolve_absolute_path(path).map_err(p1_errno_from_component_path)?;
            let (base, suffix) = self.resolve_absolute_guest_base(&guest_name)?;
            (guest_name, base, suffix)
        } else {
            let cwd = self.cwd.as_ref().ok_or(p1::errno::NOTCAPABLE)?;
            let guest_name = crate::resolve_child_path(&cwd.guest_name, path)
                .map_err(p1_errno_from_component_path)?;
            (guest_name, cwd.descriptor.clone(), path.to_owned())
        };
        let source_path =
            crate::resolve_child_path(&base.path, &suffix).map_err(p1_errno_from_component_path)?;
        let flags = base.flags;

        if !flags.contains(fs_types::DescriptorFlags::READ) {
            return Err(p1::errno::NOTCAPABLE);
        }
        // chdir follows a symbolic link to the directory it names, inside
        // the directory the path was resolved against.
        let metadata = wasi_fs::stat_path(
            &self.filesystem,
            &source_path,
            crate::Follow::Within(&base.path),
        )
        .await
        .map_err(p1_errno_from_fs)?;
        if metadata.kind != helios_hal::fs::NodeKind::Directory {
            return Err(p1::errno::NOTDIR);
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

    pub(super) fn resolve_absolute_guest_path(
        &self,
        guest_name: &str,
    ) -> Result<(String, fs_types::DescriptorFlags), i32> {
        let (descriptor, suffix) = self.resolve_absolute_guest_base(guest_name)?;
        let source_path = if suffix.is_empty() {
            descriptor.path.clone()
        } else {
            crate::resolve_child_path(&descriptor.path, &suffix)
                .map_err(p1_errno_from_component_path)?
        };
        Ok((source_path, descriptor.flags))
    }

    pub(super) fn resolve_absolute_guest_base(
        &self,
        guest_name: &str,
    ) -> Result<(FsDescriptor, String), i32> {
        let mut best: Option<(&str, &FsDescriptor)> = None;
        for entry in &self.descriptors.entries {
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
        if let Some(cwd) = self.cwd.as_ref()
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

    pub(super) fn resolve_wasix_path_base(
        &self,
        fd: i32,
        path: &str,
    ) -> Result<(FsDescriptor, String), i32> {
        if path.starts_with('/') {
            let guest_name =
                crate::resolve_absolute_path(path).map_err(p1_errno_from_component_path)?;
            return self.resolve_absolute_guest_base(&guest_name);
        }
        if let Some(cwd) = self.cwd.as_ref() {
            return Ok((cwd.descriptor.clone(), path.to_owned()));
        }
        let base = match self.descriptors.get(fd) {
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

    pub(super) fn resolve_preview1_path_base(
        &self,
        fd: i32,
        path: &str,
    ) -> Result<(FsDescriptor, String), i32> {
        if self.wasix_abi {
            return self.resolve_wasix_path_base(fd, path);
        }
        let Some(base) = p1_directory_descriptor(self.descriptors.get(fd)) else {
            return Err(p1::errno::BADF);
        };
        Ok((base.clone(), path.to_owned()))
    }

    pub(super) fn resolve_preview1_path(
        &self,
        fd: i32,
        path: &str,
    ) -> Result<(FsDescriptor, String, String), i32> {
        let (base, path) = self.resolve_preview1_path_base(fd, path)?;
        let absolute =
            crate::resolve_child_path(&base.path, &path).map_err(p1_errno_from_component_path)?;
        Ok((base, path, absolute))
    }
}

pub(super) fn add_wasi_p1_imports<CpuImpl, Net, HostFs>(
    linker: &mut CoreLinker<CompilerCoreStore<CpuImpl, Net, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "random_get",
            |caller: Caller<'_, CompilerCoreStore<CpuImpl, Net, HostFs>>,
             ptr: i32,
             len: i32|
             -> i32 {
                fill_random(
                    caller.data().memory(),
                    &caller.data().shared.entropy,
                    caller.data().runtime_state.root_entropy(),
                    GuestAddress::wasm32(ptr as u32),
                    len as u32,
                )
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap("wasi_snapshot_preview1", "sched_yield", || -> i32 {
            p1::errno::SUCCESS
        })
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "clock_time_get",
            |caller: Caller<'_, CompilerCoreStore<CpuImpl, Net, HostFs>>,
             _id: i32,
             _precision: i64,
             ptr: i32|
             -> i32 {
                write_u64(
                    caller.data().memory(),
                    GuestAddress::wasm32(ptr as u32),
                    crate::monotonic_nanos(&caller.data().cpu),
                )
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_write",
            |caller: Caller<'_, CompilerCoreStore<CpuImpl, Net, HostFs>>,
             fd: i32,
             iovs: i32,
             iovs_len: i32,
             nwritten: i32|
             -> i32 {
                fd_write(caller, fd, iovs as u32, iovs_len as u32, nwritten as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "path_open",
            |_fd: i32,
             _dirflags: i32,
             _path: i32,
             _path_len: i32,
             _oflags: i32,
             _fs_rights_base: i64,
             _fs_rights_inheriting: i64,
             _fdflags: i32,
             _opened_fd: i32|
             -> i32 { p1::errno::BADF },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "environ_get",
            |caller: Caller<'_, CompilerCoreStore<CpuImpl, Net, HostFs>>,
             environ: i32,
             buf: i32|
             -> i32 { compiler_environ_get(caller, environ as u32, buf as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "environ_sizes_get",
            |caller: Caller<'_, CompilerCoreStore<CpuImpl, Net, HostFs>>,
             count: i32,
             size: i32|
             -> i32 {
                let thread_count = compiler_plugin_worker_threads(&caller.data().cpu);
                let env_len = compiler_rayon_env_len(thread_count);
                let memory = caller.data().memory();
                let first = write_u32(memory, GuestAddress::wasm32(count as u32), 1);
                let second = write_u32(memory, GuestAddress::wasm32(size as u32), env_len);
                first.max(second)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_fdstat_get",
            |caller: Caller<'_, CompilerCoreStore<CpuImpl, Net, HostFs>>,
             fd: i32,
             stat: i32|
             -> i32 { compiler_fd_fdstat_get(caller, fd, stat as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_close",
            |mut caller: Caller<'_, CompilerCoreStore<CpuImpl, Net, HostFs>>, fd: i32| -> i32 {
                caller.data_mut().preview1_descriptors.close(fd)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_prestat_get",
            |_fd: i32, _buf: i32| -> i32 { p1::errno::BADF },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_prestat_dir_name",
            |_fd: i32, _path: i32, _len: i32| -> i32 { p1::errno::BADF },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap("wasi_snapshot_preview1", "proc_exit", |code: i32| -> () {
            panic!("compiler plugin called proc_exit({code})")
        })
        .map_err(map_program_runtime_error)?;
    Ok(())
}

pub(super) fn add_wasi_thread_spawn<CpuImpl, Net, HostFs>(
    linker: &mut CoreLinker<CompilerCoreStore<CpuImpl, Net, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap(
            "wasi",
            "thread-spawn",
            |caller: Caller<'_, CompilerCoreStore<CpuImpl, Net, HostFs>>, start_arg: i32| -> i32 {
                let store_data = caller.data().clone();
                let next = store_data.shared.next_thread_id.try_update(
                    AtomicOrdering::Relaxed,
                    AtomicOrdering::Relaxed,
                    |value| (value <= 0x1fff_fffe).then_some(value + 1),
                );
                let Ok(previous) = next else {
                    return -1;
                };
                let thread_id = previous + 1;
                let instance_pre = store_data
                    .shared
                    .instance_pre
                    .get()
                    .unwrap_or_else(|| panic!("compiler thread-spawn called before instance pre"))
                    .clone();
                let spawner = store_data.spawner.clone();
                let shared = store_data.shared.clone();
                let spawned_ns = crate::monotonic_nanos(&store_data.cpu);
                let task = spawner.spawn(async move {
                    let started_ns = crate::monotonic_nanos(&store_data.cpu);
                    let processor = helios_hal::cpu::current_processor().id();
                    let mut store =
                        wasmtime::Store::new(instance_pre.module().engine(), store_data);
                    let _interrupts = configure_compiler_core_store(&mut store);
                    let thread_started_ticks = store.data().cpu.now().ticks();
                    let (result, instantiated_ns) = match instance_pre
                        .instantiate_async(&mut store)
                        .await
                    {
                        Ok(instance) => {
                            let start = instance
                                .get_typed_func::<(i32, i32), ()>(&mut store, "wasi_thread_start");
                            let instantiated_ns = crate::monotonic_nanos(&store.data().cpu);
                            let result = match start {
                                Ok(start) => {
                                    start.call_async(&mut store, (thread_id, start_arg)).await
                                }
                                Err(error) => Err(error),
                            };
                            (result, instantiated_ns)
                        }
                        Err(error) => {
                            let instantiated_ns = crate::monotonic_nanos(&store.data().cpu);
                            (Err(error), instantiated_ns)
                        }
                    };
                    let finished_ns = crate::monotonic_nanos(&store.data().cpu);
                    let thread_elapsed = store
                        .data()
                        .cpu
                        .now()
                        .ticks()
                        .saturating_sub(thread_started_ticks);
                    store.data().record_user_ticks(thread_elapsed);
                    if let Err(error) = result {
                        tracing::error!(thread_id, "compiler plugin thread failed: {error:#}");
                    }
                    CompilerThreadReport {
                        thread_id,
                        processor,
                        spawned_ns,
                        started_ns,
                        instantiated_ns,
                        finished_ns,
                    }
                });
                shared.thread_tasks.lock().push(task);
                thread_id
            },
        )
        .map_err(map_program_runtime_error)?;
    Ok(())
}

/// Installs the store's interrupt callback, registers the store for
/// kills and makes it the target of its process's signals, for as long as
/// the returned registration lives.
pub(super) fn configure_preview1_program_store<CpuImpl, Net, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, Net, HostFs>>,
) -> StoreInterrupts<CpuImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    // No call hook: see `store_with_state`. Turns are accounted where
    // the call that drives this store is polled, and signals and kills
    // reach the guest at its interrupt checks.
    store.interrupt_callback(|mut caller| caller.data_mut().on_interrupt());
    let interrupts = store.data().interrupts.clone();
    let instance = store.data().instance().id();
    let registration = interrupts.register(instance, store, store.data().cpu.clone());
    // The thread's mailbox needs a target as well, so a running thread
    // meets a signal raised on it at its next interrupt check.
    store
        .data()
        .signal_state
        .attach(interrupts.clone(), instance);
    store.data().thread_signals.attach(interrupts, instance);
    registration
}

pub(super) fn preview1_program_linker<CpuImpl, Net, HostFs>(
    engine: &wasmtime::Engine,
) -> Result<CoreLinker<Preview1ProgramStore<CpuImpl, Net, HostFs>>, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let mut linker = CoreLinker::new(engine);
    add_preview1_program_imports(&mut linker)?;
    Ok(linker)
}

pub(super) fn add_preview1_program_imports<CpuImpl, Net, HostFs>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, Net, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
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
            "wasi_snapshot_preview1",
            "args_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             argv: i32,
             argv_buf: i32|
             -> i32 { p1_args_get(&mut caller, argv as u32, argv_buf as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "environ_sizes_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             count: i32,
             size: i32|
             -> i32 { p1_environ_sizes_get(&mut caller, count as u32, size as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "environ_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             environ: i32,
             environ_buf: i32|
             -> i32 { p1_environ_get(&mut caller, environ as u32, environ_buf as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "clock_res_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             id: i32,
             resolution: i32|
             -> i32 { p1_clock_res_get(&mut caller, id, resolution as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "clock_time_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             id: i32,
             _precision: i64,
             timestamp: i32|
             -> i32 { p1_clock_time_get(&mut caller, id, timestamp as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
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
            "wasi_snapshot_preview1",
            "random_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             ptr: i32,
             len: i32|
             -> i32 { p1_random_get(&mut caller, ptr as u32, len as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_write",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
            "wasi_snapshot_preview1",
            "fd_read",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, iovs, iovs_len, nread): (i32, i32, i32, i32)| {
                Box::new(async move {
                    p1_fd_read(&mut caller, fd, iovs as u32, iovs_len as u32, nread as u32).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_close",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>, fd: i32| -> i32 {
                caller.data_mut().descriptors.close(fd)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_prestat_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             fd: i32,
             buf: i32|
             -> i32 { p1_fd_prestat_get(&mut caller, fd, buf as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
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
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_fdstat_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             fd: i32,
             stat: i32|
             -> i32 { p1_fd_fdstat_get(&mut caller, fd, stat as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_fdstat_set_flags",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             fd: i32,
             fdflags: i32|
             -> i32 { p1_fd_fdstat_set_flags(&mut caller, fd, fdflags as u16) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_fdstat_set_rights",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             fd: i32,
             rights_base: i64,
             rights_inheriting: i64|
             -> i32 {
                p1_fd_fdstat_set_rights(
                    &mut caller,
                    fd,
                    rights_base as u64,
                    rights_inheriting as u64,
                )
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_filestat_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, stat): (i32, i32)| {
                Box::new(async move { p1_fd_filestat_get(&mut caller, fd, stat as u32).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_filestat_set_size",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, size): (i32, i64)| {
                Box::new(async move { p1_fd_filestat_set_size(&mut caller, fd, size as u64).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_filestat_set_times",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, atim, mtim, fstflags): (i32, i64, i64, i32)| {
                Box::new(async move {
                    p1_fd_filestat_set_times(
                        &mut caller,
                        fd,
                        atim as u64,
                        mtim as u64,
                        fstflags as u16,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_advise",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             fd: i32,
             _offset: i64,
             _len: i64,
             _advice: i32|
             -> i32 { p1_fd_advise(&mut caller, fd) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_allocate",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, offset, len): (i32, i64, i64)| {
                Box::new(
                    async move { p1_fd_allocate(&mut caller, fd, offset as u64, len as u64).await },
                )
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_datasync",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>, (fd,): (i32,)| {
                Box::new(async move { p1_fd_datasync(&mut caller, fd).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_sync",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>, (fd,): (i32,)| {
                Box::new(async move { p1_fd_sync(&mut caller, fd).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_pread",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, iovs, iovs_len, offset, nread): (i32, i32, i32, i64, i32)| {
                Box::new(async move {
                    p1_fd_pread(
                        &mut caller,
                        fd,
                        iovs as u32,
                        iovs_len as u32,
                        offset as u64,
                        nread as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_pwrite",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, iovs, iovs_len, offset, nwritten): (i32, i32, i32, i64, i32)| {
                Box::new(async move {
                    p1_fd_pwrite(
                        &mut caller,
                        fd,
                        iovs as u32,
                        iovs_len as u32,
                        offset as u64,
                        nwritten as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_readdir",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
            "wasi_snapshot_preview1",
            "fd_renumber",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             from: i32,
             to: i32|
             -> i32 { p1_fd_renumber(&mut caller, from, to) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_seek",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, offset, whence, new_offset): (i32, i64, i32, i32)| {
                Box::new(async move {
                    p1_fd_seek(&mut caller, fd, offset, whence as u8, new_offset as u32).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_tell",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             fd: i32,
             offset: i32|
             -> i32 { p1_fd_tell(&mut caller, fd, offset as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
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
                    p1_path_open(
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
    add_preview1_program_remaining_imports(linker)?;
    add_wasix_program_imports(linker)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
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
        .alias_module("wasi_snapshot_preview1", "wasi_unstable")
        .map_err(map_program_runtime_error)?;
    Ok(())
}

pub(super) fn add_preview1_program_remaining_imports<CpuImpl, Net, HostFs>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, Net, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "path_create_directory",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, path, path_len): (i32, i32, i32)| {
                Box::new(async move {
                    p1_path_create_directory(&mut caller, fd, path as u32, path_len as u32).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "path_filestat_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, flags, path, path_len, stat): (i32, i32, i32, i32, i32)| {
                Box::new(async move {
                    p1_path_filestat_get(
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
            "wasi_snapshot_preview1",
            "path_filestat_set_times",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, flags, path, path_len, atim, mtim, fstflags): (
                i32,
                i32,
                i32,
                i32,
                i64,
                i64,
                i32,
            )| {
                Box::new(async move {
                    p1_path_filestat_set_times(
                        &mut caller,
                        fd,
                        flags as u32,
                        path as u32,
                        path_len as u32,
                        atim as u64,
                        mtim as u64,
                        fstflags as u16,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "path_link",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (old_fd, old_flags, old_path, old_path_len, new_fd, new_path, new_path_len): (
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
            )| {
                Box::new(async move {
                    p1_path_link(
                        &mut caller,
                        old_fd,
                        old_flags as u32,
                        old_path as u32,
                        old_path_len as u32,
                        new_fd,
                        new_path as u32,
                        new_path_len as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "path_readlink",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, path, path_len, buf, buf_len, bufused): (
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
            )| {
                Box::new(async move {
                    p1_path_readlink(
                        &mut caller,
                        fd,
                        path as u32,
                        path_len as u32,
                        buf as u32,
                        buf_len as u32,
                        bufused as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "path_remove_directory",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, path, path_len): (i32, i32, i32)| {
                Box::new(async move {
                    p1_path_remove_directory(&mut caller, fd, path as u32, path_len as u32).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "path_rename",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (old_fd, old_path, old_path_len, new_fd, new_path, new_path_len): (
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
            )| {
                Box::new(async move {
                    p1_path_rename(
                        &mut caller,
                        old_fd,
                        old_path as u32,
                        old_path_len as u32,
                        new_fd,
                        new_path as u32,
                        new_path_len as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "path_symlink",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (old_path, old_path_len, fd, new_path, new_path_len): (
                i32,
                i32,
                i32,
                i32,
                i32,
            )| {
                Box::new(async move {
                    p1_path_symlink(
                        &mut caller,
                        old_path as u32,
                        old_path_len as u32,
                        fd,
                        new_path as u32,
                        new_path_len as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "path_unlink_file",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, path, path_len): (i32, i32, i32)| {
                Box::new(async move {
                    p1_path_unlink_file(&mut caller, fd, path as u32, path_len as u32).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "poll_oneoff",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (subscriptions, events, nsubscriptions, nevents): (i32, i32, i32, i32)| {
                Box::new(async move {
                    p1_poll_oneoff(
                        &mut caller,
                        subscriptions as u32,
                        events as u32,
                        nsubscriptions as u32,
                        nevents as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "proc_raise",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             signal: i32|
             -> wasmtime::Result<i32> { p1_proc_raise(&mut caller, signal as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "sock_accept",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, fdflags, fd_out): (i32, i32, i32)| {
                Box::new(
                    async move { p1_sock_accept(&mut caller, fd, fdflags, fd_out as u32).await },
                )
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "sock_recv",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, ri_data, ri_data_len, ri_flags, ro_datalen, ro_flags): (
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
            )| {
                Box::new(async move {
                    p1_sock_recv(
                        &mut caller,
                        fd,
                        ri_data as u32,
                        ri_data_len as u32,
                        ri_flags as u16,
                        ro_datalen as u32,
                        ro_flags as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "sock_send",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, si_data, si_data_len, si_flags, so_datalen): (
                i32,
                i32,
                i32,
                i32,
                i32,
            )| {
                Box::new(async move {
                    p1_sock_send(
                        &mut caller,
                        fd,
                        si_data as u32,
                        si_data_len as u32,
                        si_flags as u16,
                        so_datalen as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "sock_shutdown",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
             (fd, how): (i32, i32)| {
                Box::new(async move { p1_sock_shutdown(&mut caller, fd, how as u8).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    Ok(())
}

pub(super) fn p1_args_sizes_get<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    argc: u32,
    argv_buf_size: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let count = match u32::try_from(caller.data().arguments.len()) {
        Ok(count) => count,
        Err(_) => return p1::errno::OVERFLOW,
    };
    let size = match nul_terminated_list_size(caller.data().arguments.iter().map(String::as_str)) {
        Some(size) => size,
        None => return p1::errno::OVERFLOW,
    };
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_u32(caller, memory, GuestAddress::wasm32(argc), count).max(p1_write_u32(
        caller,
        memory,
        GuestAddress::wasm32(argv_buf_size),
        size,
    ))
}

pub(super) fn p1_args_get<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    argv: u32,
    argv_buf: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let values = caller.data().arguments.clone();
    p1_write_string_array(
        caller,
        GuestAddress::wasm32(argv),
        GuestAddress::wasm32(argv_buf),
        values.iter().map(String::as_str),
    )
}

pub(super) fn p1_environ_sizes_get<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    count: u32,
    size: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let env = p1_environment_strings(caller.data());
    let env_count = match u32::try_from(env.len()) {
        Ok(count) => count,
        Err(_) => return p1::errno::OVERFLOW,
    };
    let env_size = match nul_terminated_list_size(env.iter().map(String::as_str)) {
        Some(size) => size,
        None => return p1::errno::OVERFLOW,
    };
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_u32(caller, memory, GuestAddress::wasm32(count), env_count).max(p1_write_u32(
        caller,
        memory,
        GuestAddress::wasm32(size),
        env_size,
    ))
}

pub(super) fn p1_environ_get<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    environ: u32,
    environ_buf: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let env = p1_environment_strings(caller.data());
    p1_write_string_array(
        caller,
        GuestAddress::wasm32(environ),
        GuestAddress::wasm32(environ_buf),
        env.iter().map(String::as_str),
    )
}

pub(super) fn p1_clock_res_get<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    id: i32,
    resolution: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    match id {
        0 | 1 => {
            let Some(memory) = p1_memory(caller) else {
                return p1::errno::FAULT;
            };
            p1_write_u64(caller, memory, GuestAddress::wasm32(resolution), 1)
        }
        _ => p1::errno::INVAL,
    }
}

pub(super) fn p1_clock_time_get<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    id: i32,
    timestamp: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let value = match id {
        0 => caller.data().system_time_nanos(),
        1 => caller.data().now_nanos(),
        _ => return p1::errno::INVAL,
    };
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_u64(caller, memory, GuestAddress::wasm32(timestamp), value)
}

pub(super) fn p1_random_get<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    ptr: u32,
    len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let store = caller.data_mut();
    p1_fill_random(memory, GuestAddress::wasm32(ptr), len, |chunk| {
        store
            .entropy
            .fill_secure(store.runtime_state.root_entropy(), chunk);
    })
}

pub(super) fn p1_record_kernel_profile<CpuImpl, Net, HostFs>(
    store: &Preview1ProgramStore<CpuImpl, Net, HostFs>,
    syscall: &'static str,
    started_ticks: u64,
) where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    if store.runtime_state.profiling_enabled() {
        store.runtime_state.record_profile_stack_parts(
            ProfileScope::Kernel,
            "kernel;preview1;",
            syscall,
            store.cpu.now().ticks().saturating_sub(started_ticks),
        );
    }
}

pub(super) fn p1_kernel_profile_start<CpuImpl, Net, HostFs>(
    store: &Preview1ProgramStore<CpuImpl, Net, HostFs>,
) -> Option<u64>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    store
        .runtime_state
        .profiling_enabled()
        .then(|| store.cpu.now().ticks())
}

pub(super) fn p1_record_optional_kernel_profile<CpuImpl, Net, HostFs>(
    store: &Preview1ProgramStore<CpuImpl, Net, HostFs>,
    syscall: &'static str,
    started_ticks: Option<u64>,
) where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    if let Some(started_ticks) = started_ticks {
        p1_record_kernel_profile(store, syscall, started_ticks);
    }
}

pub(super) async fn p1_fd_write<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    nwritten: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let started = caller.data().cpu.now().ticks();
    let result = p1_fd_write_inner(
        caller,
        fd,
        GuestAddress::wasm32(iovs),
        iovs_len,
        GuestAddress::wasm32(nwritten),
    )
    .await;
    p1_record_kernel_profile(caller.data(), "fd_write", started);
    result
}

pub(super) async fn p1_fd_write_inner<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    iovs: GuestAddress,
    iovs_len: u32,
    nwritten: GuestAddress,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let layout = match p1_read_iovs_with_byte_len(memory, iovs, iovs_len) {
        Ok(layout) => layout,
        Err(errno) => return errno,
    };
    let written = match p1_write_descriptor(caller, fd, memory, &layout).await {
        Ok(written) => written,
        Err(errno) => return errno,
    };
    p1_write_u32(caller, memory, nwritten, written)
}

pub(super) async fn p1_fd_read<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    nread: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let started = caller.data().cpu.now().ticks();
    let result = p1_fd_read_inner(
        caller,
        fd,
        GuestAddress::wasm32(iovs),
        iovs_len,
        GuestAddress::wasm32(nread),
    )
    .await;
    p1_record_kernel_profile(caller.data(), "fd_read", started);
    result
}

pub(super) async fn p1_fd_read_inner<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    iovs: GuestAddress,
    iovs_len: u32,
    nread: GuestAddress,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let layout = match p1_read_iovs_with_byte_len(memory, iovs, iovs_len) {
        Ok(layout) => layout,
        Err(errno) => return errno,
    };
    let capacity = layout.byte_len;
    match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Stdin { .. }) => {
            let bytes = caller.data_mut().read_stdin(capacity).await;
            return p1_write_iovs_from_bytes(caller, memory, layout.iovs, &bytes, nread);
        }
        Some(Preview1Descriptor::PipeRead { .. }) => {
            let bytes = match caller.data_mut().read_pipe(fd, capacity as usize).await {
                Ok(bytes) => bytes,
                Err(errno) => return errno,
            };
            return p1_write_iovs_from_bytes(caller, memory, layout.iovs, &bytes, nread);
        }
        Some(Preview1Descriptor::File {
            descriptor, offset, ..
        }) => {
            let descriptor = descriptor.clone();
            let offset = *offset;
            let namespace = p1_namespace(caller);
            let file = match p1_grant(&descriptor) {
                Ok(file) => file,
                Err(errno) => return errno,
            };
            let bytes = match wasi_fs::read(&namespace, file, offset, capacity as usize).await {
                Ok(bytes) => bytes,
                Err(error) => return p1_errno_from_fs(error),
            };
            p1_set_file_offset(caller, fd, offset.saturating_add(bytes.len() as u64));
            return p1_write_iovs_from_bytes(caller, memory, layout.iovs, &bytes, nread);
        }
        _ => {}
    }
    let bytes = match p1_read_descriptor(caller, fd, capacity).await {
        Ok(bytes) => bytes,
        Err(errno) => return errno,
    };
    let mut copied = 0usize;
    for (ptr, len) in layout.iovs {
        if copied >= bytes.len() {
            break;
        }
        let len = (len as usize).min(bytes.len() - copied);
        let status = p1_write_memory(
            caller,
            memory,
            GuestAddress::wasm32(ptr),
            &bytes[copied..copied + len],
        );
        if status != p1::errno::SUCCESS {
            return status;
        }
        copied += len;
    }
    let copied = match u32::try_from(copied) {
        Ok(copied) => copied,
        Err(_) => return p1::errno::OVERFLOW,
    };
    p1_write_u32(caller, memory, nread, copied)
}

pub(super) fn p1_fd_prestat_get<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    buf: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(Preview1Descriptor::Preopen { guest_name, .. }) = caller.data().descriptors.get(fd)
    else {
        return p1::errno::BADF;
    };
    let len = match u32::try_from(guest_name.len()) {
        Ok(len) => len,
        Err(_) => return p1::errno::OVERFLOW,
    };
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let buf = GuestAddress::wasm32(buf);
    p1_write_u8(caller, memory, buf, 0).max(p1_write_u32(caller, memory, buf.offset(4), len))
}

pub(super) fn p1_fd_prestat_dir_name<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    path: u32,
    len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let Some(Preview1Descriptor::Preopen { guest_name, .. }) = caller.data().descriptors.get(fd)
    else {
        return p1::errno::BADF;
    };
    let bytes = guest_name.as_bytes();
    if bytes.len() != len as usize {
        return p1::errno::INVAL;
    }
    preview1_write_memory(memory, GuestAddress::wasm32(path), bytes)
}

pub(super) fn p1_fdstat_bytes(filetype: u8, fdflags: u16, rights: u64) -> [u8; 24] {
    let mut bytes = [0; 24];
    bytes[0] = filetype;
    bytes[2..4].copy_from_slice(&fdflags.to_le_bytes());
    bytes[8..16].copy_from_slice(&rights.to_le_bytes());
    bytes[16..24].copy_from_slice(&rights.to_le_bytes());
    bytes
}

pub(super) fn p1_fd_fdstat_get<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    stat: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(descriptor) = caller.data().descriptors.get(fd) else {
        return p1::errno::BADF;
    };
    let filetype = match descriptor {
        Preview1Descriptor::Stdin { .. } => 2,
        Preview1Descriptor::Stdout | Preview1Descriptor::Stderr => 2,
        Preview1Descriptor::PipeRead { .. } | Preview1Descriptor::PipeWrite { .. } => 2,
        Preview1Descriptor::Event(_) | Preview1Descriptor::Epoll(_) => 2,
        Preview1Descriptor::Preopen { .. } => 3,
        Preview1Descriptor::File { descriptor, .. } => p1_filetype(descriptor.kind),
        Preview1Descriptor::NullDevice => 2,
        Preview1Descriptor::Socket(_) => 6,
    };
    let fdflags = caller.data().descriptors.fdflags(fd).unwrap_or(0);
    let rights = p1_descriptor_rights(descriptor);
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    preview1_write_memory(
        memory,
        GuestAddress::wasm32(stat),
        &p1_fdstat_bytes(filetype, fdflags, rights),
    )
}

pub(super) fn p1_fd_fdstat_set_flags<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    fdflags: u16,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::File { .. } | Preview1Descriptor::NullDevice)
            if p1_file_fdflags_supported(fdflags) =>
        {
            caller.data_mut().descriptors.set_fdflags(fd, fdflags)
        }
        Some(Preview1Descriptor::Socket(_)) if p1_socket_fdflags_supported(fdflags) => {
            caller.data_mut().descriptors.set_fdflags(fd, fdflags)
        }
        Some(_) if fdflags == 0 => caller.data_mut().descriptors.set_fdflags(fd, fdflags),
        Some(_) => p1::errno::INVAL,
        None => p1::errno::BADF,
    }
}

pub(super) fn p1_fd_fdstat_set_rights<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    rights_base: u64,
    rights_inheriting: u64,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let requested = rights_base | rights_inheriting;
    let Some(descriptor) = caller.data().descriptors.get(fd) else {
        return p1::errno::BADF;
    };
    let current = p1_descriptor_rights(descriptor);
    if requested & !current != 0 {
        return p1::errno::NOTCAPABLE;
    }
    let lowered_flags = p1_descriptor_flags(requested, 0);
    match caller.data_mut().descriptors.get_mut(fd) {
        Some(Preview1Descriptor::Preopen { descriptor, .. })
        | Some(Preview1Descriptor::File { descriptor, .. }) => {
            descriptor.flags &= lowered_flags;
            p1::errno::SUCCESS
        }
        Some(_) => p1::errno::SUCCESS,
        None => p1::errno::BADF,
    }
}

pub(super) async fn p1_fd_filestat_get<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    stat: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let stat = GuestAddress::wasm32(stat);
    if matches!(
        caller.data().descriptors.get(fd),
        Some(Preview1Descriptor::NullDevice)
    ) {
        return p1_write_filestat(
            caller,
            stat,
            p1_null_device_identity(),
            p1_null_device_stat(),
        );
    }
    let Some(path) = p1_descriptor_path(caller.data().descriptors.get(fd)).map(ToOwned::to_owned)
    else {
        return p1::errno::BADF;
    };
    let (identity, stat_value) = match p1_stat_absolute_path(caller, &path, crate::Follow::No).await
    {
        Ok(stat) => stat,
        Err(errno) => return errno,
    };
    p1_write_filestat(caller, stat, identity, stat_value)
}

/// Resolves a guest-absolute path through the store's namespace to its
/// object identity and descriptor stat, following a final symbolic link
/// when `follow` is set.
pub(super) async fn p1_stat_absolute_path<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    path: &str,
    follow: crate::Follow<'_>,
) -> Result<(crate::ObjectIdentity, fs_types::DescriptorStat), i32>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let namespace = p1_namespace(caller);
    let metadata = wasi_fs::stat_path(&namespace, path, follow)
        .await
        .map_err(p1_errno_from_fs)?;
    Ok((
        crate::object_identity(&metadata),
        wasi_fs::descriptor_stat(&metadata),
    ))
}

/// The namespace the store's filesystem calls resolve through, owned so a
/// call can await on it while the store stays borrowable.
pub(super) fn p1_namespace<CpuImpl, Net, HostFs>(
    caller: &Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
) -> Namespace<HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    caller.data().filesystem.clone()
}

/// The filesystem descriptor behind a preview1 descriptor: an open file or
/// directory, or a preopen.
pub(super) fn p1_fs_descriptor<Net: ComponentHostNetwork>(
    descriptor: Option<&Preview1Descriptor<Net>>,
) -> Option<&FsDescriptor> {
    match descriptor {
        Some(Preview1Descriptor::Preopen { descriptor, .. })
        | Some(Preview1Descriptor::File { descriptor, .. }) => Some(descriptor),
        _ => None,
    }
}

/// Moves the offset of the open file `fd` after one of its own reads or
/// writes.
fn p1_set_file_offset<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    next: u64,
) where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(Preview1Descriptor::File { offset, .. }) = caller.data_mut().descriptors.get_mut(fd)
    else {
        panic!("Preview1 file descriptor {fd} disappeared during its own operation");
    };
    *offset = next;
}

pub(super) async fn p1_fd_filestat_set_size<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    size: u64,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(Preview1Descriptor::File { descriptor, .. }) = caller.data().descriptors.get(fd)
    else {
        return p1::errno::BADF;
    };
    let descriptor = descriptor.clone();
    let namespace = p1_namespace(caller);
    let now_nanos = caller.data().now_nanos();
    let file = match p1_grant(&descriptor) {
        Ok(file) => file,
        Err(errno) => return errno,
    };
    wasi_fs::set_size(&namespace, file, size, now_nanos)
        .await
        .map_or_else(p1_errno_from_fs, |()| p1::errno::SUCCESS)
}

pub(super) async fn p1_fd_filestat_set_times<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    atim: u64,
    mtim: u64,
    fstflags: u16,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(descriptor) = p1_fs_descriptor(caller.data().descriptors.get(fd)).cloned() else {
        return p1::errno::BADF;
    };
    let target = match descriptor.grant_touch() {
        Ok(target) => target,
        Err(error) => return p1_errno_from_fs(error),
    };
    let (access, modified, now_nanos) = p1_set_times_arguments(caller, atim, mtim, fstflags);
    let namespace = p1_namespace(caller);
    wasi_fs::set_times(&namespace, target, access, modified, now_nanos)
        .await
        .map_or_else(p1_errno_from_fs, |()| p1::errno::SUCCESS)
}

/// The access time, the modification time and the current time, in
/// nanoseconds, that `fstflags` asks `fd_filestat_set_times` and
/// `path_filestat_set_times` to set.
fn p1_set_times_arguments<CpuImpl, Net, HostFs>(
    caller: &Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    atim: u64,
    mtim: u64,
    fstflags: u16,
) -> (Option<u64>, Option<u64>, u64)
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let now_nanos = caller.data().system_time_nanos();
    let access = p1_timestamp_from_fstflags(
        fstflags,
        P1_FSTFLAG_ATIM,
        P1_FSTFLAG_ATIM_NOW,
        atim,
        now_nanos,
    );
    let modified = p1_timestamp_from_fstflags(
        fstflags,
        P1_FSTFLAG_MTIM,
        P1_FSTFLAG_MTIM_NOW,
        mtim,
        now_nanos,
    );
    (access, modified, now_nanos)
}

pub(super) fn p1_fd_advise<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    caller
        .data()
        .descriptors
        .get(fd)
        .map_or(p1::errno::BADF, |_| p1::errno::SUCCESS)
}

pub(super) async fn p1_fd_allocate<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    offset: u64,
    len: u64,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let end = match offset.checked_add(len) {
        Some(end) => end,
        None => return p1::errno::OVERFLOW,
    };
    let Some(Preview1Descriptor::File { descriptor, .. }) = caller.data().descriptors.get(fd)
    else {
        return p1::errno::BADF;
    };
    let descriptor = descriptor.clone();
    let namespace = p1_namespace(caller);
    let current = match wasi_fs::stat_path(&namespace, &descriptor.path, crate::Follow::No).await {
        Ok(metadata) => metadata.size,
        Err(error) => return p1_errno_from_fs(error),
    };
    if end <= current {
        return p1::errno::SUCCESS;
    }
    let now_nanos = caller.data().now_nanos();
    let file = match p1_grant(&descriptor) {
        Ok(file) => file,
        Err(errno) => return errno,
    };
    wasi_fs::set_size(&namespace, file, end, now_nanos)
        .await
        .map_or_else(p1_errno_from_fs, |()| p1::errno::SUCCESS)
}

pub(super) async fn p1_fd_datasync<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    p1_fd_sync_impl(caller, fd, true).await
}

pub(super) async fn p1_fd_sync<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    p1_fd_sync_impl(caller, fd, false).await
}

/// Backs `fd_sync` and `fd_datasync`: a filesystem object is made durable
/// by the filesystem that serves it. Stdio, pipes and `/dev/null` have no
/// write-back stage to flush, so success there is an accurate answer and
/// not a silent skip.
async fn p1_fd_sync_impl<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    data_only: bool,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(descriptor) = caller.data().descriptors.get(fd) else {
        return p1::errno::BADF;
    };
    let Some(descriptor) = p1_fs_descriptor(Some(descriptor)).cloned() else {
        return p1::errno::SUCCESS;
    };
    let namespace = p1_namespace(caller);
    wasi_fs::sync(&namespace, &descriptor, data_only)
        .await
        .map_or_else(p1_errno_from_fs, |()| p1::errno::SUCCESS)
}

pub(super) async fn p1_fd_pread<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    offset: u64,
    nread: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let layout = match p1_read_iovs_with_byte_len(memory, GuestAddress::wasm32(iovs), iovs_len) {
        Ok(layout) => layout,
        Err(errno) => return errno,
    };
    let Some(Preview1Descriptor::File { descriptor, .. }) = caller.data().descriptors.get(fd)
    else {
        return p1::errno::BADF;
    };
    let descriptor = descriptor.clone();
    let namespace = p1_namespace(caller);
    let file = match p1_grant(&descriptor) {
        Ok(file) => file,
        Err(errno) => return errno,
    };
    let bytes = match wasi_fs::read(&namespace, file, offset, layout.byte_len as usize).await {
        Ok(bytes) => bytes,
        Err(error) => return p1_errno_from_fs(error),
    };
    p1_write_iovs_from_bytes(
        caller,
        memory,
        layout.iovs,
        &bytes,
        GuestAddress::wasm32(nread),
    )
}

pub(super) async fn p1_fd_pwrite<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    offset: u64,
    nwritten: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let nwritten = GuestAddress::wasm32(nwritten);
    let layout = match p1_read_iovs_with_byte_len(memory, GuestAddress::wasm32(iovs), iovs_len) {
        Ok(layout) => layout,
        Err(errno) => return errno,
    };
    let written = match p1_write_file(caller, fd, memory, &layout, Some(offset)).await {
        Ok(written) => written,
        Err(errno) => return errno,
    };
    p1_write_u32(caller, memory, nwritten, written)
}

/// Writes the bytes `layout` names in guest memory to the file `fd`,
/// gathered straight into the filesystem's destination: at `at` when
/// given (`fd_pwrite`, which leaves the offset alone), else at the
/// descriptor's offset or, in append mode, at the end of the file, moving
/// the offset past them.
async fn p1_write_file<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    memory: Preview1Memory,
    layout: &Preview1IovLayout,
    at: Option<u64>,
) -> Result<u32, i32>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(Preview1Descriptor::File {
        descriptor,
        offset,
        fdflags,
    }) = caller.data().descriptors.get(fd)
    else {
        return Err(p1::errno::BADF);
    };
    let position = match at {
        Some(at) => WritePosition::At(at),
        None if fdflags & P1_FDFLAG_APPEND != 0 => WritePosition::End,
        None => WritePosition::At(*offset),
    };
    let descriptor = descriptor.clone();
    let mut gather = Preview1Gather::new(memory, layout)?;
    let namespace = p1_namespace(caller);
    let now_nanos = caller.data().now_nanos();
    let (end, written) = {
        let file =
            wasi_fs::writable_file(&namespace, p1_grant(&descriptor)?).map_err(p1_errno_from_fs)?;
        let mut file = ResolvedFileChunks {
            file: &file,
            now_nanos,
        };
        p1_write_gathered(&mut gather, position, &mut file)
            .await
            .map_err(p1_errno_from_fs)?
    };
    if at.is_none() {
        // After an append the offset is the end of the file the write
        // reached, which another writer may have moved meanwhile.
        p1_set_file_offset(caller, fd, end);
    }
    Ok(written)
}

/// A resolved file as what a gathered write lands in: each chunk goes
/// straight from guest memory into the file system's own destination.
struct ResolvedFileChunks<'file, 'namespace, HostFs: crate::HostFileSystem> {
    file: &'file crate::vfs::Resolved<'namespace, HostFs>,
    now_nanos: u64,
}

impl<HostFs: crate::HostFileSystem> Preview1ChunkedFile for ResolvedFileChunks<'_, '_, HostFs> {
    type Refusal = fs_types::ErrorCode;

    async fn land(
        &mut self,
        gather: &mut Preview1Gather,
        position: WritePosition,
        len: usize,
    ) -> Result<u64, fs_types::ErrorCode> {
        wasi_fs::write_resolved_with(self.file, position, len, self.now_nanos, |destination| {
            gather.fill(destination)
        })
        .await
    }
}

pub(super) async fn p1_fd_readdir<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    buf: u32,
    buf_len: u32,
    cookie: u64,
    bufused: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let (buf, bufused) = (GuestAddress::wasm32(buf), GuestAddress::wasm32(bufused));
    let Some(descriptor) = p1_fs_descriptor(caller.data().descriptors.get(fd)).cloned() else {
        return p1::errno::BADF;
    };
    let namespace = p1_namespace(caller);
    let entries = match wasi_fs::read_directory(&namespace, &descriptor).await {
        Ok(entries) => entries,
        Err(error) => return p1_errno_from_fs(error),
    };
    p1_fd_readdir_entries(caller, entries, buf, buf_len, cookie, bufused)
}

pub(super) fn p1_fd_readdir_entries<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    entries: Vec<fs_types::DirectoryEntry>,
    buf: GuestAddress,
    buf_len: u32,
    cookie: u64,
    bufused: GuestAddress,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let start_index = match usize::try_from(cookie) {
        Ok(index) => index,
        Err(_) => return p1::errno::OVERFLOW,
    };
    let mut used = 0usize;
    let capacity = buf_len as usize;
    for (index, entry) in entries.iter().enumerate().skip(start_index) {
        let dirent_len = 24usize;
        if used >= capacity {
            break;
        }
        let next = match u64::try_from(index + 1) {
            Ok(next) => next,
            Err(_) => return p1::errno::OVERFLOW,
        };
        let name = entry.name.as_bytes();
        if capacity - used < dirent_len {
            break;
        }
        let dirent_ptr = buf.offset(used as u64);
        let status = p1_write_u64(caller, memory, dirent_ptr, next)
            .max(p1_write_u64(caller, memory, dirent_ptr.offset(8), next))
            .max(p1_write_u32(
                caller,
                memory,
                dirent_ptr.offset(16),
                name.len().try_into().unwrap_or(u32::MAX),
            ))
            .max(p1_write_u8(
                caller,
                memory,
                dirent_ptr.offset(20),
                p1_filetype_from_descriptor_type(entry.type_.clone()),
            ));
        if status != p1::errno::SUCCESS {
            return status;
        }
        used += dirent_len;
        let remaining = capacity - used;
        let copied = remaining.min(name.len());
        let name_ptr = buf.offset(used as u64);
        let status = p1_write_memory(caller, memory, name_ptr, &name[..copied]);
        if status != p1::errno::SUCCESS {
            return status;
        }
        used += copied;
        if copied < name.len() {
            break;
        }
    }
    let used = match u32::try_from(used) {
        Ok(used) => used,
        Err(_) => return p1::errno::OVERFLOW,
    };
    p1_write_u32(caller, memory, bufused, used)
}

pub(super) fn p1_fd_renumber<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    from: i32,
    to: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    caller.data_mut().descriptors.renumber(from, to)
}

pub(super) async fn p1_fd_seek<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    offset: i64,
    whence: u8,
    new_offset: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let (descriptor, current) = match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::File {
            descriptor, offset, ..
        }) => (descriptor.clone(), *offset),
        Some(_) => return p1::errno::SPIPE,
        None => return p1::errno::BADF,
    };
    let base = match whence {
        0 => 0,
        1 => current,
        2 => {
            let namespace = p1_namespace(caller);
            match wasi_fs::stat_path(&namespace, &descriptor.path, crate::Follow::No).await {
                Ok(metadata) => metadata.size,
                Err(error) => return p1_errno_from_fs(error),
            }
        }
        _ => return p1::errno::INVAL,
    };
    let next = if offset >= 0 {
        base.checked_add(offset as u64)
    } else {
        base.checked_sub(offset.unsigned_abs())
    };
    let Some(next) = next else {
        return p1::errno::INVAL;
    };
    let Some(Preview1Descriptor::File { offset, .. }) = caller.data_mut().descriptors.get_mut(fd)
    else {
        return p1::errno::BADF;
    };
    *offset = next;
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_u64(caller, memory, GuestAddress::wasm32(new_offset), next)
}

pub(super) fn p1_fd_tell<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    offset_out: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let offset = match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::File { offset, .. }) => *offset,
        Some(_) => return p1::errno::SPIPE,
        None => return p1::errno::BADF,
    };
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_u64(caller, memory, GuestAddress::wasm32(offset_out), offset)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn p1_path_open<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
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
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let path = match p1_read_path(caller, memory, GuestAddress::wasm32(path), path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let (base, path) = if caller.data().wasix_abi {
        match caller.data().resolve_wasix_path_base(fd, &path) {
            Ok(resolved) => resolved,
            Err(errno) => return errno,
        }
    } else {
        let base = match caller.data().descriptors.get(fd) {
            Some(Preview1Descriptor::Preopen { descriptor, .. })
            | Some(Preview1Descriptor::File { descriptor, .. })
                if descriptor.kind == FsNodeKind::Directory =>
            {
                descriptor.clone()
            }
            Some(_) => return p1::errno::NOTDIR,
            None => return p1::errno::BADF,
        };
        (base, path)
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

#[allow(clippy::too_many_arguments)]
pub(super) async fn p1_path_open_resolved<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    memory: Preview1Memory,
    base: FsDescriptor,
    path: String,
    path_flags: fs_types::PathFlags,
    open_flags: fs_types::OpenFlags,
    descriptor_flags: fs_types::DescriptorFlags,
    fdflags: u16,
    opened_fd: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    match p1_open_null_device(&base, &path, open_flags) {
        Ok(true) => {
            let fd = match caller
                .data_mut()
                .descriptors
                .insert(Preview1Descriptor::NullDevice)
            {
                Ok(fd) => fd,
                Err(errno) => return errno,
            };
            return p1_write_u32(caller, memory, GuestAddress::wasm32(opened_fd), fd);
        }
        Ok(false) => {}
        Err(errno) => return errno,
    }
    let opened = match p1_open_descriptor_resolved(
        caller,
        &base,
        path_flags,
        &path,
        open_flags,
        descriptor_flags,
    )
    .await
    {
        Ok(descriptor) => descriptor,
        Err(errno) => return errno,
    };
    let fd = match caller
        .data_mut()
        .descriptors
        .insert(Preview1Descriptor::File {
            descriptor: opened,
            offset: 0,
            fdflags,
        }) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    p1_write_u32(caller, memory, GuestAddress::wasm32(opened_fd), fd)
}

pub(super) fn p1_open_null_device(
    base: &FsDescriptor,
    path: &str,
    open_flags: fs_types::OpenFlags,
) -> Result<bool, i32> {
    let absolute =
        crate::resolve_child_path(&base.path, path).map_err(p1_errno_from_component_path)?;
    if absolute != WASIX_NULL_DEVICE_PATH {
        return Ok(false);
    }
    if open_flags.contains(fs_types::OpenFlags::DIRECTORY) {
        return Err(p1::errno::NOTDIR);
    }
    if open_flags.contains(fs_types::OpenFlags::CREATE)
        && open_flags.contains(fs_types::OpenFlags::EXCLUSIVE)
    {
        return Err(p1::errno::EXIST);
    }
    Ok(true)
}

pub(super) async fn p1_open_descriptor_resolved<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    base: &FsDescriptor,
    path_flags: fs_types::PathFlags,
    path: &str,
    open_flags: fs_types::OpenFlags,
    descriptor_flags: fs_types::DescriptorFlags,
) -> Result<FsDescriptor, i32>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let namespace = p1_namespace(caller);
    let now_nanos = caller.data().now_nanos();
    wasi_fs::open_at(
        &namespace,
        base,
        path_flags,
        path,
        open_flags,
        descriptor_flags,
        now_nanos,
    )
    .await
    .map_err(p1_errno_from_fs)
}

pub(super) async fn p1_path_create_directory<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
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
    let path = match p1_read_path(caller, memory, GuestAddress::wasm32(path), path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let (base, path, _) = match caller.data().resolve_preview1_path(fd, &path) {
        Ok(resolved) => resolved,
        Err(errno) => return errno,
    };
    let namespace = p1_namespace(caller);
    let now_nanos = caller.data().now_nanos();
    let granted = match p1_grant(&base) {
        Ok(granted) => granted,
        Err(errno) => return errno,
    };
    wasi_fs::create_directory_at(&namespace, granted, &path, now_nanos)
        .await
        .map_or_else(p1_errno_from_fs, |()| p1::errno::SUCCESS)
}

pub(super) async fn p1_path_filestat_get<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    flags: u32,
    path: u32,
    path_len: u32,
    stat: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let stat = GuestAddress::wasm32(stat);
    let path = match p1_read_path(caller, memory, GuestAddress::wasm32(path), path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let (base, _, absolute) = match caller.data().resolve_preview1_path(fd, &path) {
        Ok(resolved) => resolved,
        Err(errno) => return errno,
    };
    if absolute == WASIX_NULL_DEVICE_PATH {
        return p1_write_filestat(
            caller,
            stat,
            p1_null_device_identity(),
            p1_null_device_stat(),
        );
    }
    let follow = wasi_fs::follow_within(&base, p1_path_flags(flags));
    let (identity, stat_value) = match p1_stat_absolute_path(caller, &absolute, follow).await {
        Ok(stat) => stat,
        Err(errno) => return errno,
    };
    p1_write_filestat(caller, stat, identity, stat_value)
}

#[expect(
    clippy::too_many_arguments,
    reason = "the parameter list is the guest ABI of this call, so grouping it would hide the contract and break the one-to-one match with the linker registration"
)]
pub(super) async fn p1_path_filestat_set_times<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    _flags: u32,
    path: u32,
    path_len: u32,
    atim: u64,
    mtim: u64,
    fstflags: u16,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let path = match p1_read_path(caller, memory, GuestAddress::wasm32(path), path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let (base, path, _) = match caller.data().resolve_preview1_path(fd, &path) {
        Ok(resolved) => resolved,
        Err(errno) => return errno,
    };
    let granted = match p1_grant(&base) {
        Ok(granted) => granted,
        Err(errno) => return errno,
    };
    let (access, modified, now_nanos) = p1_set_times_arguments(caller, atim, mtim, fstflags);
    let namespace = p1_namespace(caller);
    wasi_fs::set_times_at(&namespace, granted, &path, access, modified, now_nanos)
        .await
        .map_or_else(p1_errno_from_fs, |()| p1::errno::SUCCESS)
}

#[expect(
    clippy::too_many_arguments,
    reason = "the parameter list is the guest ABI of this call, so grouping it would hide the contract and break the one-to-one match with the linker registration"
)]
pub(super) async fn p1_path_link<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    old_fd: i32,
    _old_flags: u32,
    old_path: u32,
    old_path_len: u32,
    new_fd: i32,
    new_path: u32,
    new_path_len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let old_path = match p1_read_path(caller, memory, GuestAddress::wasm32(old_path), old_path_len)
    {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let new_path = match p1_read_path(caller, memory, GuestAddress::wasm32(new_path), new_path_len)
    {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let status = caller.data().require_hardlink_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let (source_base, old_path, _) = match caller.data().resolve_preview1_path(old_fd, &old_path) {
        Ok(resolved) => resolved,
        Err(errno) => return errno,
    };
    let (destination_base, new_path, _) =
        match caller.data().resolve_preview1_path(new_fd, &new_path) {
            Ok(resolved) => resolved,
            Err(errno) => return errno,
        };
    let namespace = p1_namespace(caller);
    let now_nanos = caller.data().now_nanos();
    let destination = match p1_grant(&destination_base) {
        Ok(destination) => destination,
        Err(errno) => return errno,
    };
    wasi_fs::link_at(
        &namespace,
        &source_base,
        &old_path,
        destination,
        &new_path,
        now_nanos,
    )
    .await
    .map_or_else(p1_errno_from_fs, |()| p1::errno::SUCCESS)
}

pub(super) async fn p1_path_readlink<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    path: u32,
    path_len: u32,
    buf: u32,
    buf_len: u32,
    bufused: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let path = match p1_read_path(caller, memory, GuestAddress::wasm32(path), path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let status = caller.data().require_symlink_read_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let (base, path, _) = match caller.data().resolve_preview1_path(fd, &path) {
        Ok(resolved) => resolved,
        Err(errno) => return errno,
    };
    let namespace = p1_namespace(caller);
    let target = match wasi_fs::readlink_at(&namespace, &base, &path).await {
        Ok(target) => target,
        Err(error) => return p1_errno_from_fs(error),
    };
    let bytes = target.as_bytes();
    let copied = (buf_len as usize).min(bytes.len());
    let status = p1_write_memory(caller, memory, GuestAddress::wasm32(buf), &bytes[..copied]);
    if status != p1::errno::SUCCESS {
        return status;
    }
    let copied = match u32::try_from(copied) {
        Ok(copied) => copied,
        Err(_) => return p1::errno::OVERFLOW,
    };
    p1_write_u32(caller, memory, GuestAddress::wasm32(bufused), copied)
}

pub(super) async fn p1_path_remove_directory<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
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
    let path = match p1_read_path(caller, memory, GuestAddress::wasm32(path), path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let (base, path, _) = match caller.data().resolve_preview1_path(fd, &path) {
        Ok(resolved) => resolved,
        Err(errno) => return errno,
    };
    let namespace = p1_namespace(caller);
    let granted = match p1_grant(&base) {
        Ok(granted) => granted,
        Err(errno) => return errno,
    };
    wasi_fs::remove_directory_at(&namespace, granted, &path)
        .await
        .map_or_else(p1_errno_from_fs, |()| p1::errno::SUCCESS)
}

pub(super) async fn p1_path_rename<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    old_fd: i32,
    old_path: u32,
    old_path_len: u32,
    new_fd: i32,
    new_path: u32,
    new_path_len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let old_path = match p1_read_path(caller, memory, GuestAddress::wasm32(old_path), old_path_len)
    {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let new_path = match p1_read_path(caller, memory, GuestAddress::wasm32(new_path), new_path_len)
    {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let (source_base, old_path, _) = match caller.data().resolve_preview1_path(old_fd, &old_path) {
        Ok(resolved) => resolved,
        Err(errno) => return errno,
    };
    let (destination_base, new_path, _) =
        match caller.data().resolve_preview1_path(new_fd, &new_path) {
            Ok(resolved) => resolved,
            Err(errno) => return errno,
        };
    let namespace = p1_namespace(caller);
    let now_nanos = caller.data().now_nanos();
    let (source, destination) = match (p1_grant(&source_base), p1_grant(&destination_base)) {
        (Ok(source), Ok(destination)) => (source, destination),
        (Err(errno), _) | (_, Err(errno)) => return errno,
    };
    wasi_fs::rename_at(
        &namespace,
        source,
        &old_path,
        destination,
        &new_path,
        now_nanos,
    )
    .await
    .map_or_else(p1_errno_from_fs, |()| p1::errno::SUCCESS)
}

pub(super) async fn p1_path_symlink<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    old_path: u32,
    old_path_len: u32,
    fd: i32,
    new_path: u32,
    new_path_len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let target = match p1_read_path(caller, memory, GuestAddress::wasm32(old_path), old_path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let new_path = match p1_read_path(caller, memory, GuestAddress::wasm32(new_path), new_path_len)
    {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let status = caller.data().require_symlink_create_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let (base, new_path, _) = match caller.data().resolve_preview1_path(fd, &new_path) {
        Ok(resolved) => resolved,
        Err(errno) => return errno,
    };
    let namespace = p1_namespace(caller);
    let now_nanos = caller.data().now_nanos();
    let granted = match p1_grant(&base) {
        Ok(granted) => granted,
        Err(errno) => return errno,
    };
    wasi_fs::symlink_at(&namespace, granted, &target, &new_path, now_nanos)
        .await
        .map_or_else(p1_errno_from_fs, |()| p1::errno::SUCCESS)
}

pub(super) async fn p1_path_unlink_file<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
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
    let path = match p1_read_path(caller, memory, GuestAddress::wasm32(path), path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let (base, path, _) = match caller.data().resolve_preview1_path(fd, &path) {
        Ok(resolved) => resolved,
        Err(errno) => return errno,
    };
    let namespace = p1_namespace(caller);
    let granted = match p1_grant(&base) {
        Ok(granted) => granted,
        Err(errno) => return errno,
    };
    wasi_fs::unlink_file_at(&namespace, granted, &path)
        .await
        .map_or_else(p1_errno_from_fs, |()| p1::errno::SUCCESS)
}

/// How much console input one stdin readiness probe pulls into the carry.
const P1_STDIN_PROBE_CAPACITY: u32 = 4096;

/// `eventrwflags`: the read end of the descriptor has hung up.
const P1_EVENT_FD_READWRITE_HANGUP: u16 = 1;

/// One decoded `poll_oneoff` subscription.
enum P1Subscription {
    /// A deadline in `clock_id`'s timebase.
    Clock {
        userdata: u64,
        monotonic: bool,
        deadline_nanos: u64,
    },
    Fd {
        userdata: u64,
        event_type: u8,
        fd: i32,
    },
    /// Malformed subscription. It is reported immediately, like `POLLNVAL`.
    Failed {
        userdata: u64,
        event_type: u8,
        error: u16,
    },
}

/// One event that `poll_oneoff` will hand back to the guest.
struct P1ReadyEvent {
    userdata: u64,
    error: u16,
    event_type: u8,
    nbytes: u64,
    fd_flags: u16,
}

/// `poll_oneoff` is `select`: block until at least one subscription is ready
/// or the earliest clock deadline expires, then report only what is ready.
///
/// The previous implementation walked the subscriptions sequentially, slept
/// the full duration of every clock subscription regardless of whether a
/// descriptor had become ready, and emitted an event for every subscription
/// whether or not it was ready. A guest polling "socket readable, or 5s
/// timeout" therefore always waited the whole 5 seconds, and then could not
/// tell which of the two events had actually fired.
pub(super) async fn p1_poll_oneoff<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    subscriptions: u32,
    events: u32,
    nsubscriptions: u32,
    nevents: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    if nsubscriptions == 0 {
        return p1::errno::INVAL;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };

    let (monotonic_now, system_now) =
        (caller.data().now_nanos(), caller.data().system_time_nanos());
    let max_subscriptions = p1_subscription_limit(caller.data().descriptors.limit);
    let parsed = match p1_read_subscriptions(
        memory,
        GuestAddress::wasm32(subscriptions),
        nsubscriptions,
        max_subscriptions,
        monotonic_now,
        system_now,
    ) {
        Ok(parsed) => parsed,
        Err(errno) => return errno,
    };

    let ready = loop {
        // Arm every subscribed descriptor before testing any of them: a
        // waiter's notification generation is sampled when it is built,
        // so a byte landing between a descriptor's probe and its waiter's
        // creation would be invisible to the parked wait.
        let mut wait = P1WaitSet::new();
        for subscription in &parsed {
            if let P1Subscription::Fd { fd, event_type, .. } = subscription {
                p1_add_wait_target(caller.data(), *fd, *event_type, &mut wait);
            }
        }

        let mut ready = Vec::new();
        let mut earliest: Option<Duration> = None;

        for subscription in &parsed {
            match subscription {
                P1Subscription::Failed {
                    userdata,
                    event_type,
                    error,
                } => ready.push(P1ReadyEvent {
                    userdata: *userdata,
                    error: *error,
                    event_type: *event_type,
                    nbytes: 0,
                    fd_flags: 0,
                }),
                P1Subscription::Fd {
                    userdata,
                    event_type,
                    fd,
                } => {
                    let readiness = p1_descriptor_readiness(caller, *fd, *event_type).await;
                    if let Some(event) = p1_fd_event(*userdata, *event_type, readiness) {
                        ready.push(event);
                    }
                }
                P1Subscription::Clock {
                    userdata,
                    monotonic,
                    deadline_nanos,
                } => {
                    let now = if *monotonic {
                        caller.data().now_nanos()
                    } else {
                        caller.data().system_time_nanos()
                    };
                    match p1_clock_progress(*userdata, *deadline_nanos, now) {
                        P1ClockProgress::Elapsed(event) => ready.push(event),
                        P1ClockProgress::Waiting(remaining) => {
                            earliest = Some(
                                earliest
                                    .map_or(remaining, |current: Duration| current.min(remaining)),
                            );
                        }
                    }
                }
            }
        }

        if !ready.is_empty() {
            break ready;
        }

        // Nothing is ready and no deadline has expired: sleep until one
        // of the armed descriptors makes progress or the earliest
        // deadline arrives.
        let timer = caller.data().timer();
        p1_wait_step(&timer, &mut wait, earliest).await;
    };

    let events = GuestAddress::wasm32(events);
    let mut event_count = 0u32;
    for event in &ready {
        let event_ptr = events.offset(u64::from(event_count) * u64::from(P1_EVENT_SIZE));
        let status = p1_write_u64(caller, memory, event_ptr, event.userdata)
            .max(p1_write_u16(
                caller,
                memory,
                event_ptr.offset(8),
                event.error,
            ))
            .max(p1_write_u8(
                caller,
                memory,
                event_ptr.offset(10),
                event.event_type,
            ))
            .max(p1_write_u64(
                caller,
                memory,
                event_ptr.offset(16),
                event.nbytes,
            ))
            .max(p1_write_u16(
                caller,
                memory,
                event_ptr.offset(24),
                event.fd_flags,
            ));
        if status != p1::errno::SUCCESS {
            return status;
        }
        event_count += 1;
    }
    p1_write_u32(caller, memory, GuestAddress::wasm32(nevents), event_count)
}

/// Whether a clock subscription's deadline has arrived.
enum P1ClockProgress {
    Elapsed(P1ReadyEvent),
    Waiting(Duration),
}

/// Turn a descriptor's readiness into an event, or nothing when it would
/// still block.
///
/// Returning `None` for `Pending` is what makes `poll_oneoff` behave like
/// `select`: only subscriptions that are actually ready are reported, so the
/// guest can tell which of its subscriptions fired.
fn p1_fd_event(
    userdata: u64,
    event_type: u8,
    readiness: Result<P1Readiness, i32>,
) -> Option<P1ReadyEvent> {
    match readiness {
        Ok(readiness) if !readiness.is_ready() => None,
        Ok(readiness) => Some(P1ReadyEvent {
            userdata,
            error: p1::errno::SUCCESS as u16,
            event_type,
            nbytes: readiness.bytes(),
            fd_flags: if readiness.is_hangup() {
                P1_EVENT_FD_READWRITE_HANGUP
            } else {
                0
            },
        }),
        Err(errno) => Some(P1ReadyEvent {
            userdata,
            error: errno as u16,
            event_type,
            nbytes: 0,
            fd_flags: 0,
        }),
    }
}

/// Evaluate a clock subscription against the current time in its timebase.
///
/// A deadline that already passed — including a zero timeout — is ready on
/// the first pass, which is what makes a zero-timeout `poll_oneoff` a
/// non-blocking readiness snapshot.
fn p1_clock_progress(userdata: u64, deadline_nanos: u64, now: u64) -> P1ClockProgress {
    if now >= deadline_nanos {
        return P1ClockProgress::Elapsed(P1ReadyEvent {
            userdata,
            error: p1::errno::SUCCESS as u16,
            event_type: P1_EVENTTYPE_CLOCK,
            nbytes: 0,
            fd_flags: 0,
        });
    }
    P1ClockProgress::Waiting(Duration::from_nanos(deadline_nanos - now))
}

/// The most subscriptions one `poll_oneoff` takes for a process whose
/// descriptor table admits `limit` descriptors. Linux `poll` refuses an
/// `nfds` above `RLIMIT_NOFILE` with `EINVAL`, and wasi-libc's `poll` turns
/// each `pollfd` into at most two subscriptions (read and write) plus one
/// clock for the timeout.
fn p1_subscription_limit(limit: DescriptorLimit) -> usize {
    limit.get().saturating_mul(2).saturating_add(1)
}

/// Decodes the `nsubscriptions` subscriptions at `subscriptions`, anchoring
/// clock timeouts at `monotonic_now` / `system_now`. A count above
/// `max_subscriptions` is `EINVAL`, and the array must lie inside `memory`;
/// both are checked before anything is collected for it.
fn p1_read_subscriptions(
    memory: Preview1Memory,
    subscriptions: GuestAddress,
    nsubscriptions: u32,
    max_subscriptions: usize,
    monotonic_now: u64,
    system_now: u64,
) -> Result<Vec<P1Subscription>, i32> {
    if nsubscriptions as usize > max_subscriptions {
        return Err(p1::errno::INVAL);
    }
    let mut parsed = Vec::with_capacity(nsubscriptions as usize);
    for index in 0..nsubscriptions {
        let subscription_ptr =
            subscriptions.offset(u64::from(index) * u64::from(P1_SUBSCRIPTION_SIZE));
        let userdata = u64::from_le_bytes(p1_read_array(memory, subscription_ptr)?);
        let [event_type] = p1_read_array(memory, subscription_ptr.offset(8))?;
        parsed.push(match event_type {
            P1_EVENTTYPE_CLOCK => {
                let clock_id =
                    u32::from_le_bytes(p1_read_array(memory, subscription_ptr.offset(16))?);
                let timeout =
                    u64::from_le_bytes(p1_read_array(memory, subscription_ptr.offset(24))?);
                let flags = u16::from_le_bytes(p1_read_array(memory, subscription_ptr.offset(40))?);
                if !matches!(clock_id, 0 | 1) {
                    P1Subscription::Failed {
                        userdata,
                        event_type,
                        error: p1::errno::INVAL as u16,
                    }
                } else {
                    let monotonic = clock_id == 1;
                    let now = if monotonic { monotonic_now } else { system_now };
                    // Relative timeouts are anchored once, here, so a
                    // subscription cannot restart its countdown every time
                    // the wait loop re-probes.
                    let deadline_nanos = if flags & P1_SUBSCRIPTION_CLOCK_ABSTIME != 0 {
                        timeout
                    } else {
                        now.saturating_add(timeout)
                    };
                    P1Subscription::Clock {
                        userdata,
                        monotonic,
                        deadline_nanos,
                    }
                }
            }
            P1_EVENTTYPE_FD_READ | P1_EVENTTYPE_FD_WRITE => {
                let fd = i32::from_le_bytes(p1_read_array(memory, subscription_ptr.offset(16))?);
                P1Subscription::Fd {
                    userdata,
                    event_type,
                    fd,
                }
            }
            _ => P1Subscription::Failed {
                userdata,
                event_type,
                error: p1::errno::INVAL as u16,
            },
        });
    }
    Ok(parsed)
}

pub(super) fn p1_proc_raise<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    signal: u32,
) -> wasmtime::Result<i32>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    caller
        .data_mut()
        .request_exit(128u32.saturating_add(signal));
    Err(wasmtime::Error::new(Preview1Exit))
}

pub(super) fn p1_errno_from_wasmtime_error(error: &wasmtime::Error) -> i32 {
    error
        .downcast_ref::<ProgramExecError>()
        .map_or(p1::errno::IO, p1_errno_from_program_exec_error)
}

pub(super) fn p1_errno_from_program_exec_error(error: &ProgramExecError) -> i32 {
    match error.kind {
        ProgramExecErrorKind::InvalidBinary
        | ProgramExecErrorKind::MissingEntry
        | ProgramExecErrorKind::UnsupportedImport
        | ProgramExecErrorKind::InvalidSignature
        | ProgramExecErrorKind::InvalidHint => p1::errno::NOENT,
        ProgramExecErrorKind::InvalidPath => p1::errno::INVAL,
        ProgramExecErrorKind::PermissionDenied => p1::errno::NOTCAPABLE,
        ProgramExecErrorKind::OutOfMemory => p1::errno::NOMEM,
        ProgramExecErrorKind::Unavailable => p1::errno::NOTSUP,
        ProgramExecErrorKind::Internal => p1::errno::IO,
    }
}

pub(super) async fn p1_sock_accept<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    fdflags: i32,
    fd_out: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let fdflags = match u16::try_from(fdflags) {
        Ok(fdflags) if p1_socket_fdflags_supported(fdflags) => fdflags,
        _ => return p1::errno::INVAL,
    };
    let status = caller.data().require_tcp_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let (listener, family) = match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Listening {
                listener, family, ..
            },
        ))) => (listener.id(), *family),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(_))) => {
            return p1::errno::INVAL;
        }
        Some(Preview1Descriptor::Socket(_)) => return p1::errno::INVAL,
        Some(_) => return p1::errno::NOTSOCK,
        None => return p1::errno::BADF,
    };
    let Some(service) = caller.data().runtime_state.network_service() else {
        return p1::errno::NETDOWN;
    };
    let timeout = if p1_fdflags_nonblocking(fdflags) {
        0
    } else {
        u64::MAX
    };
    let accepted = match service.tcp_accept(listener, timeout).await {
        Ok(accepted) => accepted,
        Err(error) => return p1_errno_from_tcp_error_for_fdflags(error, fdflags),
    };
    let descriptor =
        Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(WasixTcpSocket::Connected {
            family,
            stream: WasixOwnedTcpStream::new(service.clone(), accepted.stream),
            peer_address: accepted.address,
            peer_port: accepted.port,
            options: WasixSocketOptions::default(),
        }));
    let accepted_fd = match caller
        .data_mut()
        .descriptors
        .insert_with_fdflags(descriptor, false, fdflags)
    {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    let Some(memory) = p1_memory(caller) else {
        let _ = caller.data_mut().descriptors.close(accepted_fd as i32);
        return p1::errno::FAULT;
    };
    let status = p1_write_u32(caller, memory, GuestAddress::wasm32(fd_out), accepted_fd);
    if status != p1::errno::SUCCESS {
        let _ = caller.data_mut().descriptors.close(accepted_fd as i32);
        return status;
    }
    p1::errno::SUCCESS
}

pub(super) fn p1_connected_tcp_stream<CpuImpl, Net, HostFs>(
    caller: &Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
) -> Result<Net::TcpStream, i32>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected { stream, .. },
        ))) => Ok(stream.id()),
        Some(Preview1Descriptor::Socket(_)) => Err(p1::errno::INVAL),
        Some(_) => Err(p1::errno::NOTSOCK),
        None => Err(p1::errno::BADF),
    }
}

pub(super) async fn p1_sock_recv<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    ri_data: u32,
    ri_data_len: u32,
    _ri_flags: u16,
    ro_datalen: u32,
    ro_flags: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let started = caller
        .data()
        .runtime_state
        .profiling_enabled()
        .then(|| caller.data().cpu.now().ticks());
    let result = p1_sock_recv_inner(
        caller,
        fd,
        ri_data,
        ri_data_len,
        _ri_flags,
        ro_datalen,
        ro_flags,
    )
    .await;
    if let Some(started) = started {
        p1_record_kernel_profile(caller.data(), "sock_recv", started);
    }
    result
}

pub(super) async fn p1_sock_recv_inner<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    ri_data: u32,
    ri_data_len: u32,
    _ri_flags: u16,
    ro_datalen: u32,
    ro_flags: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let (ri_data, ro_datalen, ro_flags) = (
        GuestAddress::wasm32(ri_data),
        GuestAddress::wasm32(ro_datalen),
        GuestAddress::wasm32(ro_flags),
    );
    let iovs_started = p1_kernel_profile_start(caller.data());
    let layout = match p1_read_iovs_with_byte_len(memory, ri_data, ri_data_len) {
        Ok(layout) => layout,
        Err(errno) => return errno,
    };
    p1_record_optional_kernel_profile(caller.data(), "sock_recv_iovs", iovs_started);
    let capacity = layout.byte_len;
    let fdflags = match caller.data().descriptors.fdflags(fd) {
        Ok(fdflags) => fdflags,
        Err(errno) => return errno,
    };
    if matches!(
        caller.data().descriptors.get(fd),
        Some(Preview1Descriptor::Socket(
            WasixSocketDescriptor::Pair { .. }
        ))
    ) {
        if capacity == 0 {
            let status = p1_write_iovs_from_bytes(caller, memory, layout.iovs, &[], ro_datalen);
            if status != p1::errno::SUCCESS {
                return status;
            }
            return p1_write_u16(caller, memory, ro_flags, 0);
        }
        if p1_fdflags_nonblocking(fdflags) {
            let bytes = match caller
                .data_mut()
                .try_read_socket_pair(fd, capacity as usize)
            {
                Ok(Some(bytes)) => bytes,
                Ok(None) => return p1::errno::AGAIN,
                Err(errno) => return errno,
            };
            let status = p1_write_iovs_from_bytes(caller, memory, layout.iovs, &bytes, ro_datalen);
            if status != p1::errno::SUCCESS {
                return status;
            }
            return p1_write_u16(caller, memory, ro_flags, 0);
        }
        let bytes = match caller
            .data_mut()
            .read_socket_pair(fd, capacity as usize)
            .await
        {
            Ok(bytes) => bytes,
            Err(errno) => return errno,
        };
        let status = p1_write_iovs_from_bytes(caller, memory, layout.iovs, &bytes, ro_datalen);
        if status != p1::errno::SUCCESS {
            return status;
        }
        return p1_write_u16(caller, memory, ro_flags, 0);
    }
    let stream = match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected { stream, .. },
        ))) => stream.id(),
        Some(Preview1Descriptor::Socket(_)) => return p1::errno::INVAL,
        Some(_) => return p1::errno::NOTSOCK,
        None => return p1::errno::BADF,
    };
    let status = caller.data().require_tcp_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(service) = caller.data().runtime_state.network_service() else {
        return p1::errno::NETDOWN;
    };
    let timeout = if p1_fdflags_nonblocking(fdflags) {
        0
    } else {
        u64::MAX
    };
    let ranges = match p1_iovs_memory_ranges(memory, &layout.iovs) {
        Ok(ranges) => ranges,
        Err(errno) => return errno,
    };
    let buffer = crate::RegisteredTcpReadBuffer::new(memory.base, &ranges);
    let service_started = p1_kernel_profile_start(caller.data());
    let bytes = match service.tcp_read_into(stream, buffer, timeout).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => 0,
        Err(error) => return p1_errno_from_tcp_error_for_fdflags(error, fdflags),
    };
    p1_record_optional_kernel_profile(caller.data(), "sock_recv_tcp_read", service_started);
    let write_started = p1_kernel_profile_start(caller.data());
    let status = p1_write_u32(
        caller,
        memory,
        ro_datalen,
        u32::try_from(bytes).unwrap_or_else(|_| panic!("TCP receive byte count exceeds u32")),
    );
    if status != p1::errno::SUCCESS {
        return status;
    }
    let status = p1_write_u16(caller, memory, ro_flags, 0);
    p1_record_optional_kernel_profile(caller.data(), "sock_recv_write_iovs", write_started);
    status
}

pub(super) async fn p1_sock_send<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    si_data: u32,
    si_data_len: u32,
    _si_flags: u16,
    so_datalen: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let started = caller
        .data()
        .runtime_state
        .profiling_enabled()
        .then(|| caller.data().cpu.now().ticks());
    let result = p1_sock_send_inner(caller, fd, si_data, si_data_len, _si_flags, so_datalen).await;
    if let Some(started) = started {
        p1_record_kernel_profile(caller.data(), "sock_send", started);
    }
    result
}

pub(super) async fn p1_sock_send_inner<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    si_data: u32,
    si_data_len: u32,
    _si_flags: u16,
    so_datalen: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let (si_data, so_datalen) = (
        GuestAddress::wasm32(si_data),
        GuestAddress::wasm32(so_datalen),
    );
    let mut gather = match p1_read_iovs_with_byte_len(memory, si_data, si_data_len)
        .and_then(|layout| Preview1Gather::new(memory, &layout))
    {
        Ok(gather) => gather,
        Err(errno) => return errno,
    };
    let descriptor = caller.data().descriptors.get(fd).cloned();
    if let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { writer, .. })) = descriptor
    {
        let fdflags = match caller.data().descriptors.fdflags(fd) {
            Ok(fdflags) => fdflags,
            Err(errno) => return errno,
        };
        let written = match p1_send_gathered(&mut gather, |chunk| {
            p1_send_to_socketpair(&writer, chunk, fdflags)
        })
        .await
        {
            Ok(written) => written,
            Err(errno) => return errno,
        };
        return p1_write_u32(caller, memory, so_datalen, written);
    }
    let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(WasixTcpSocket::Connected {
        stream,
        ..
    }))) = &descriptor
    else {
        return p1_connected_tcp_stream(caller, fd)
            .err()
            .unwrap_or(p1::errno::INVAL);
    };
    let status = caller.data().require_tcp_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(service) = caller.data().runtime_state.network_service() else {
        return p1::errno::NETDOWN;
    };
    let fdflags = match caller.data().descriptors.fdflags(fd) {
        Ok(fdflags) => fdflags,
        Err(errno) => return errno,
    };
    let timeout = if p1_fdflags_nonblocking(fdflags) {
        0
    } else {
        u64::MAX
    };
    let written = match p1_send_gathered(&mut gather, |chunk| {
        let (service, stream) = (&service, stream.id());
        async move {
            service
                .tcp_write_all_bytes(stream, Bytes::from(chunk), timeout)
                .await
                .map_err(|error| p1_errno_from_tcp_error_for_fdflags(error, fdflags))
        }
    })
    .await
    {
        Ok(written) => written,
        Err(errno) => return errno,
    };
    p1_write_u32(caller, memory, so_datalen, written)
}

pub(super) async fn p1_sock_shutdown<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    how: u8,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    if how == 0 || how & !P1_SDFLAGS_SUPPORTED != 0 {
        return p1::errno::INVAL;
    }
    let descriptor = caller.data().descriptors.get(fd).cloned();
    match descriptor {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected { .. },
        ))) => {
            let status = caller.data().require_tcp_authority();
            if status != p1::errno::SUCCESS {
                return status;
            }
            if caller.data().runtime_state.network_service().is_none() {
                return p1::errno::NETDOWN;
            }
            let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(slot))) =
                caller.data_mut().descriptors.get_mut(fd)
            else {
                return p1::errno::BADF;
            };
            // Replacing the slot drops this descriptor's handle, and the
            // stream is retired once the last one is gone.
            let options = *slot.options();
            *slot = WasixTcpSocket::Unconnected {
                family: slot.family(),
                options,
            };
            p1::errno::SUCCESS
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Unconnected { .. },
        ))) => p1::errno::INVAL,
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(_))) => p1::errno::INVAL,
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Bound {
            ..
        }))) => {
            let status = caller.data().require_udp_authority();
            if status != p1::errno::SUCCESS {
                return status;
            }
            if caller.data().runtime_state.network_service().is_none() {
                return p1::errno::NETDOWN;
            }
            let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(slot))) =
                caller.data_mut().descriptors.get_mut(fd)
            else {
                return p1::errno::BADF;
            };
            // Replacing the slot drops this descriptor's handle, and the
            // socket is retired once the last one is gone.
            let options = *slot.options();
            *slot = WasixUdpSocket::Unbound {
                family: slot.family(),
                options,
            };
            p1::errno::SUCCESS
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Unbound {
            ..
        }))) => p1::errno::INVAL,
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair {
            reader, writer, ..
        })) => {
            if how & P1_SDFLAGS_RD != 0 {
                reader.close();
            }
            if how & P1_SDFLAGS_WR != 0 {
                writer.close();
            }
            p1::errno::SUCCESS
        }
        Some(_) => p1::errno::NOTSOCK,
        None => p1::errno::BADF,
    }
}

pub(super) fn p1_environment_strings<CpuImpl, Net, HostFs>(
    store: &Preview1ProgramStore<CpuImpl, Net, HostFs>,
) -> Vec<String>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    store
        .environment
        .iter()
        .map(|(name, value)| {
            let mut entry = String::with_capacity(name.len() + value.len() + 1);
            entry.push_str(name);
            entry.push('=');
            entry.push_str(value);
            entry
        })
        .collect()
}

/// Push one datagram into the peer half of a socketpair.
///
/// The pair is a bounded byte channel like every other child pipe, so a
/// blocking socket waits for the peer to drain and a non-blocking one
/// reports `EAGAIN` while keeping its bytes. Nothing is dropped.
pub(super) async fn p1_send_to_socketpair(
    writer: &crate::ByteWriter,
    bytes: Vec<u8>,
    fdflags: u16,
) -> Result<(), i32> {
    if p1_fdflags_nonblocking(fdflags) {
        return match writer.try_write(bytes) {
            crate::TryWrite::Written => Ok(()),
            crate::TryWrite::Full(_) => Err(p1::errno::AGAIN),
            crate::TryWrite::Closed => Err(p1::errno::IO),
        };
    }
    writer.write(bytes).await.map_err(|_| p1::errno::IO)
}

/// Write one chunk to stdout/stderr, respecting the descriptor's
/// blocking mode.
///
/// A serial or trace route takes the bytes inside `route_output`. A child
/// pipe is bounded: a blocking descriptor waits for the parent to drain,
/// a non-blocking one reports `EAGAIN` and keeps its bytes. A reader that
/// has gone away is not an error — the bytes go nowhere, exactly like a
/// POSIX write to a closed pipe with SIGPIPE suppressed.
async fn p1_write_stdio<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    stream: crate::ComponentOutputStreamKind,
    bytes: &[u8],
    nonblocking: bool,
) -> Result<u32, i32>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let written = u32::try_from(bytes.len()).map_err(|_| p1::errno::OVERFLOW)?;
    let serial = caller.data().write_serial;
    let mut placed = 0;
    let writer = loop {
        // Armed before the offer so a transmit release that lands
        // between the two is not lost.
        let wait = serial.wait_for_debug_serial_output_ready();
        match caller.data().route_output(stream, &bytes[placed..]) {
            RoutedOutput::Child(writer) => break writer,
            RoutedOutput::Local(taken) => {
                placed += taken;
                if placed == bytes.len() {
                    return Ok(written);
                }
                // The debug UART is owned by another processor right
                // now. A non-blocking descriptor reports what it did
                // place; a blocking one waits on the console's transmit
                // signal and offers the rest.
                if taken == 0 {
                    if nonblocking {
                        // POSIX: a non-blocking write that placed
                        // nothing reports `EAGAIN` and keeps its bytes;
                        // one that placed some reports what it placed.
                        return if placed == 0 {
                            Err(p1::errno::AGAIN)
                        } else {
                            u32::try_from(placed).map_err(|_| p1::errno::OVERFLOW)
                        };
                    }
                    wait.await;
                }
            }
        }
    };
    if nonblocking {
        match writer.try_write(Bytes::copy_from_slice(bytes)) {
            crate::TryWrite::Written | crate::TryWrite::Closed => {}
            crate::TryWrite::Full(_) => return Err(p1::errno::AGAIN),
        }
    } else {
        let _: Result<(), crate::ClosedPeer> = writer.write(Bytes::copy_from_slice(bytes)).await;
    }
    Ok(written)
}

/// Where one chunk of guest stdio went.
///
/// A kernel-served sink reports how many bytes it took, because the
/// debug UART it may be is owned by one processor at a time (#103) and
/// takes a guest stream a line at a time. A child pipe hands back its
/// writer for the caller to push through outside the store borrow.
pub(super) enum RoutedOutput {
    Local(usize),
    Child(crate::ByteWriter),
}

pub(super) async fn p1_write_descriptor<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    memory: Preview1Memory,
    layout: &Preview1IovLayout,
) -> Result<u32, i32>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    // A file takes the guest's bytes straight into its own destination;
    // every other sink takes them gathered, one bounded chunk at a time.
    if let Some(Preview1Descriptor::File { .. }) = caller.data().descriptors.get(fd) {
        return p1_write_file(caller, fd, memory, layout, None).await;
    }
    // A descriptor whose sink is a bounded channel blocks the guest while
    // that channel is full, unless the guest asked for non-blocking IO —
    // then it gets `EAGAIN` and keeps its bytes, as POSIX requires.
    let nonblocking = caller
        .data()
        .descriptors
        .fdflags(fd)
        .is_ok_and(p1_fdflags_nonblocking);
    let mut gather = Preview1Gather::new(memory, layout)?;
    let mut written = 0_u32;
    // An empty write still reaches the sink once.
    loop {
        let chunk = gather.next_chunk().unwrap_or_default();
        let taken = match p1_write_sink_chunk(caller, fd, &chunk, nonblocking).await {
            Ok(taken) => taken,
            // What an earlier chunk wrote stands; the guest meets the
            // refusal on its next call.
            Err(_) if written > 0 => break,
            Err(errno) => return Err(errno),
        };
        // A count above `u32::MAX` cannot arise: the iovecs' sum fits it.
        written += taken;
        if gather.remaining() == 0 || (taken as usize) < chunk.len() {
            break;
        }
    }
    Ok(written)
}

/// One chunk of a gathered write to the non-file descriptor `fd`, and how
/// many of its bytes the sink took.
async fn p1_write_sink_chunk<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    bytes: &[u8],
    nonblocking: bool,
) -> Result<u32, i32>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Stdout) => {
            p1_write_stdio(
                caller,
                crate::ComponentOutputStreamKind::Stdout,
                bytes,
                nonblocking,
            )
            .await
        }
        Some(Preview1Descriptor::Stderr) => {
            p1_write_stdio(
                caller,
                crate::ComponentOutputStreamKind::Stderr,
                bytes,
                nonblocking,
            )
            .await
        }
        Some(Preview1Descriptor::PipeWrite { writer }) => {
            let writer = writer.clone();
            if nonblocking {
                match writer.try_write(Bytes::copy_from_slice(bytes)) {
                    crate::TryWrite::Written => {}
                    crate::TryWrite::Full(_) => return Err(p1::errno::AGAIN),
                    crate::TryWrite::Closed => return Err(p1::errno::IO),
                }
            } else {
                writer
                    .write(Bytes::copy_from_slice(bytes))
                    .await
                    .map_err(|_| p1::errno::IO)?;
            }
            u32::try_from(bytes.len()).map_err(|_| p1::errno::OVERFLOW)
        }
        Some(Preview1Descriptor::Event(event)) => {
            if bytes.len() != 8 {
                return Err(p1::errno::INVAL);
            }
            let increment = u64::from_le_bytes(
                bytes
                    .try_into()
                    .unwrap_or_else(|_| panic!("eventfd write length was checked")),
            );
            event.write(increment)?;
            Ok(8)
        }
        Some(Preview1Descriptor::NullDevice) => {
            u32::try_from(bytes.len()).map_err(|_| p1::errno::OVERFLOW)
        }
        Some(_) => Err(p1::errno::BADF),
        None => Err(p1::errno::BADF),
    }
}

pub(super) async fn p1_read_descriptor<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    capacity: u32,
) -> Result<Bytes, i32>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Stdin { .. }) => Ok(caller.data_mut().read_stdin(capacity).await),
        Some(Preview1Descriptor::PipeRead { .. }) => {
            caller.data_mut().read_pipe(fd, capacity as usize).await
        }
        Some(Preview1Descriptor::Event(event)) => {
            if capacity < 8 {
                return Err(p1::errno::INVAL);
            }
            Ok(Bytes::copy_from_slice(&event.read().await.to_le_bytes()))
        }
        Some(Preview1Descriptor::NullDevice) => Ok(Bytes::new()),
        Some(Preview1Descriptor::File {
            descriptor, offset, ..
        }) => {
            let descriptor = descriptor.clone();
            let offset = *offset;
            let namespace = p1_namespace(caller);
            let bytes = wasi_fs::read(
                &namespace,
                p1_grant(&descriptor)?,
                offset,
                capacity as usize,
            )
            .await
            .map_err(p1_errno_from_fs)?;
            p1_set_file_offset(caller, fd, offset.saturating_add(bytes.len() as u64));
            Ok(bytes)
        }
        Some(_) => Err(p1::errno::BADF),
        None => Err(p1::errno::BADF),
    }
}

pub(super) fn p1_path_flags(flags: u32) -> fs_types::PathFlags {
    let mut result = fs_types::PathFlags::empty();
    if flags & 1 != 0 {
        result |= fs_types::PathFlags::SYMLINK_FOLLOW;
    }
    result
}

pub(super) fn p1_open_flags(flags: u16) -> fs_types::OpenFlags {
    let mut result = fs_types::OpenFlags::empty();
    if flags & 1 != 0 {
        result |= fs_types::OpenFlags::CREATE;
    }
    if flags & 2 != 0 {
        result |= fs_types::OpenFlags::DIRECTORY;
    }
    if flags & 4 != 0 {
        result |= fs_types::OpenFlags::EXCLUSIVE;
    }
    if flags & 8 != 0 {
        result |= fs_types::OpenFlags::TRUNCATE;
    }
    result
}

pub(super) fn p1_descriptor_flags(rights: u64, fdflags: u16) -> fs_types::DescriptorFlags {
    let mut flags = fs_types::DescriptorFlags::empty();
    if rights & P1_RIGHT_FD_READ != 0 || rights & P1_RIGHT_FD_READDIR != 0 {
        flags |= fs_types::DescriptorFlags::READ;
    }
    if rights & (P1_RIGHT_FD_WRITE | P1_RIGHT_FD_FILESTAT_SET_SIZE | P1_RIGHT_FD_FILESTAT_SET_TIMES)
        != 0
    {
        flags |= fs_types::DescriptorFlags::WRITE;
    }
    if rights & P1_RIGHT_PATH_MUTATE_MASK != 0 {
        flags |= fs_types::DescriptorFlags::MUTATE_DIRECTORY;
    }
    let _ = fdflags;
    flags
}

pub(super) fn p1_file_fdflags_supported(fdflags: u16) -> bool {
    fdflags & !P1_FILE_FDFLAGS == 0
}

pub(super) fn p1_socket_fdflags_supported(fdflags: u16) -> bool {
    fdflags & !P1_SOCKET_FDFLAGS == 0
}

pub(super) fn p1_fdflags_nonblocking(fdflags: u16) -> bool {
    fdflags & P1_FDFLAG_NONBLOCK != 0
}

/// Writes a preview1 `filestat` record.
///
/// `identity` supplies `st_dev`/`st_ino`: the authority domain is the device
/// (one per mount — bootfs, the 9p host share, synthetic devices) and the
/// local id is the inode. Programs that de-duplicate files by `(dev, ino)` —
/// `cp -r`, `find`, `rsync`, tar — need both to be real and stable, so no
/// field here may be a placeholder zero.
pub(super) fn p1_write_filestat<CpuImpl, Net, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    stat: GuestAddress,
    identity: crate::ObjectIdentity,
    value: fs_types::DescriptorStat,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let atim = value
        .data_access_timestamp
        .map(|datetime| {
            u64::try_from(datetime.seconds)
                .unwrap_or(0)
                .saturating_mul(1_000_000_000)
                .saturating_add(u64::from(datetime.nanoseconds))
        })
        .unwrap_or(0);
    let mtim = value
        .data_modification_timestamp
        .map(|datetime| {
            u64::try_from(datetime.seconds)
                .unwrap_or(0)
                .saturating_mul(1_000_000_000)
                .saturating_add(u64::from(datetime.nanoseconds))
        })
        .unwrap_or(0);
    let ctim = value
        .status_change_timestamp
        .map(|datetime| {
            u64::try_from(datetime.seconds)
                .unwrap_or(0)
                .saturating_mul(1_000_000_000)
                .saturating_add(u64::from(datetime.nanoseconds))
        })
        .unwrap_or(0);
    p1_write_u64(caller, memory, stat, identity.domain().raw())
        .max(p1_write_u64(
            caller,
            memory,
            stat.offset(8),
            identity.local(),
        ))
        .max(p1_write_u8(
            caller,
            memory,
            stat.offset(16),
            p1_filetype_from_descriptor_type(value.type_),
        ))
        .max(p1_write_u64(
            caller,
            memory,
            stat.offset(24),
            value.link_count,
        ))
        .max(p1_write_u64(caller, memory, stat.offset(32), value.size))
        .max(p1_write_u64(caller, memory, stat.offset(40), atim))
        .max(p1_write_u64(caller, memory, stat.offset(48), mtim))
        .max(p1_write_u64(caller, memory, stat.offset(56), ctim))
}

pub(super) fn p1_timestamp_from_fstflags(
    fstflags: u16,
    value_flag: u16,
    now_flag: u16,
    value: u64,
    now_nanos: u64,
) -> Option<u64> {
    if fstflags & now_flag != 0 {
        Some(now_nanos)
    } else if fstflags & value_flag != 0 {
        Some(value)
    } else {
        None
    }
}

pub(super) fn p1_errno_from_component_path(error: crate::ComponentFsPathError) -> i32 {
    match error {
        crate::ComponentFsPathError::InvalidBasePath => p1::errno::INVAL,
        crate::ComponentFsPathError::NotPermitted => p1::errno::PERM,
    }
}

/// The proof an operation demands of `descriptor`, or the errno that
/// refuses it.
pub(super) fn p1_grant<K: wasi_fs::Kind, R: wasi_fs::Right>(
    descriptor: &FsDescriptor,
) -> Result<wasi_fs::Granted<'_, K, R>, i32> {
    descriptor.grant().map_err(p1_errno_from_fs)
}

pub(super) fn p1_errno_from_fs(error: fs_types::ErrorCode) -> i32 {
    match error {
        fs_types::ErrorCode::Access => p1::errno::ACCES,
        fs_types::ErrorCode::Already => p1::errno::EXIST,
        fs_types::ErrorCode::Invalid => p1::errno::INVAL,
        fs_types::ErrorCode::Io => p1::errno::IO,
        fs_types::ErrorCode::IsDirectory => p1::errno::ISDIR,
        fs_types::ErrorCode::Loop => p1::errno::LOOP,
        fs_types::ErrorCode::NameTooLong => p1::errno::NAMETOOLONG,
        fs_types::ErrorCode::NoEntry => p1::errno::NOENT,
        fs_types::ErrorCode::NotDirectory => p1::errno::NOTDIR,
        fs_types::ErrorCode::NotEmpty => p1::errno::NOTEMPTY,
        fs_types::ErrorCode::Unsupported => p1::errno::NOTSUP,
        fs_types::ErrorCode::Overflow => p1::errno::OVERFLOW,
        fs_types::ErrorCode::NotPermitted => p1::errno::PERM,
        fs_types::ErrorCode::ReadOnly => p1::errno::ROFS,
        fs_types::ErrorCode::CrossDevice => p1::errno::XDEV,
        _ => p1::errno::IO,
    }
}

pub(super) fn p1_errno_from_dns_error(error: crate::DnsError) -> i32 {
    match error.kind {
        crate::DnsErrorKind::UnresolvedHost => p1::errno::HOSTUNREACH,
        crate::DnsErrorKind::Timeout => p1::errno::TIMEDOUT,
        crate::DnsErrorKind::Unavailable => p1::errno::NETDOWN,
        crate::DnsErrorKind::Internal => p1::errno::IO,
    }
}

pub(super) fn p1_errno_from_tcp_error(error: crate::TcpError) -> i32 {
    match error.kind {
        crate::TcpErrorKind::UnresolvedHost => p1::errno::HOSTUNREACH,
        crate::TcpErrorKind::Timeout => p1::errno::TIMEDOUT,
        crate::TcpErrorKind::ConnectionReset => p1::errno::CONNRESET,
        crate::TcpErrorKind::ConnectionAborted => p1::errno::CONNABORTED,
        crate::TcpErrorKind::PermissionDenied => p1::errno::NOTCAPABLE,
        crate::TcpErrorKind::Unavailable => p1::errno::NETDOWN,
        crate::TcpErrorKind::Internal => p1::errno::IO,
    }
}

pub(super) fn p1_errno_from_tcp_error_for_fdflags(error: crate::TcpError, fdflags: u16) -> i32 {
    if p1_fdflags_nonblocking(fdflags) && matches!(error.kind, crate::TcpErrorKind::Timeout) {
        return p1::errno::AGAIN;
    }
    p1_errno_from_tcp_error(error)
}

pub(super) fn p1_errno_from_udp_error(error: crate::UdpError) -> i32 {
    match error.kind {
        crate::UdpErrorKind::UnresolvedHost => p1::errno::HOSTUNREACH,
        crate::UdpErrorKind::Unsupported => p1::errno::NOTSUP,
        crate::UdpErrorKind::Timeout => p1::errno::TIMEDOUT,
        crate::UdpErrorKind::PermissionDenied => p1::errno::NOTCAPABLE,
        crate::UdpErrorKind::Unavailable => p1::errno::NETDOWN,
        crate::UdpErrorKind::Internal => p1::errno::IO,
    }
}

pub(super) fn p1_errno_from_udp_error_for_fdflags(error: crate::UdpError, fdflags: u16) -> i32 {
    if p1_fdflags_nonblocking(fdflags) && matches!(error.kind, crate::UdpErrorKind::Timeout) {
        return p1::errno::AGAIN;
    }
    p1_errno_from_udp_error(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regular_file() -> Preview1Descriptor<crate::test_support::TestNetworkService> {
        Preview1Descriptor::File {
            descriptor: FsDescriptor {
                path: "/data".into(),
                kind: FsNodeKind::File,
                flags: fs_types::DescriptorFlags::READ,
                identity: None,
                mount: None,
            },
            offset: 0,
            fdflags: 0,
        }
    }

    /// `path_open(O_TRUNC)` against a preopen that may read and change its
    /// directory but not write file data is refused, whatever rights the
    /// guest asks for, and leaves the file as it was.
    #[test]
    fn path_open_truncate_without_write_authority_leaves_the_file_untouched() {
        use futures_lite::future::block_on;

        let namespace: crate::Namespace<crate::UnsupportedHostFileSystem> =
            crate::Namespace::new(crate::MountTable::new(
                crate::BootFs::new(crate::EmbeddedBootFs::new(&[], &[]), crate::BOOT_DEVICE),
                1,
            ));
        let owner = FsDescriptor {
            path: "/".into(),
            kind: FsNodeKind::Directory,
            flags: fs_types::DescriptorFlags::READ
                | fs_types::DescriptorFlags::WRITE
                | fs_types::DescriptorFlags::MUTATE_DIRECTORY,
            identity: None,
            mount: None,
        };
        let file = block_on(wasi_fs::open_at(
            &namespace,
            &owner,
            fs_types::PathFlags::empty(),
            "notes.txt",
            fs_types::OpenFlags::CREATE,
            fs_types::DescriptorFlags::READ | fs_types::DescriptorFlags::WRITE,
            1,
        ))
        .expect("the owner creates the file");
        block_on(wasi_fs::write(
            &namespace,
            file.grant().expect("the owner may write the file"),
            helios_hal::fs::WritePosition::At(0),
            "日本語 한국어".as_bytes(),
            2,
        ))
        .expect("the owner writes the file");

        let preopen = FsDescriptor {
            flags: fs_types::DescriptorFlags::READ | fs_types::DescriptorFlags::MUTATE_DIRECTORY,
            ..owner
        };
        for rights in [P1_RIGHT_FD_READ | P1_RIGHT_FD_WRITE, P1_RIGHT_FD_READ] {
            let opened = block_on(wasi_fs::open_at(
                &namespace,
                &preopen,
                fs_types::PathFlags::empty(),
                "notes.txt",
                p1_open_flags(8),
                p1_descriptor_flags(rights, 0),
                3,
            ));
            assert!(matches!(opened, Err(fs_types::ErrorCode::ReadOnly)));
            let contents = block_on(wasi_fs::read(
                &namespace,
                file.grant().expect("the owner may read the file"),
                0,
                64,
            ))
            .expect("the owner reads back");
            assert_eq!(contents.as_ref(), "日本語 한국어".as_bytes());
        }
    }

    /// A namespace with one file, `/data`, and the time it was last
    /// modified.
    fn namespace_with_file() -> crate::Namespace<crate::UnsupportedHostFileSystem> {
        use futures_lite::future::block_on;

        let namespace = crate::Namespace::new(crate::MountTable::new(
            crate::BootFs::new(crate::EmbeddedBootFs::new(&[], &[]), crate::BOOT_DEVICE),
            1,
        ));
        let owner = FsDescriptor {
            path: "/".into(),
            kind: FsNodeKind::Directory,
            flags: fs_types::DescriptorFlags::READ
                | fs_types::DescriptorFlags::WRITE
                | fs_types::DescriptorFlags::MUTATE_DIRECTORY,
            identity: None,
            mount: None,
        };
        block_on(wasi_fs::open_at(
            &namespace,
            &owner,
            fs_types::PathFlags::empty(),
            "data",
            fs_types::OpenFlags::CREATE,
            fs_types::DescriptorFlags::READ,
            1,
        ))
        .expect("the owner creates the file");
        namespace
    }

    fn modified_nanos(namespace: &crate::Namespace<crate::UnsupportedHostFileSystem>) -> u64 {
        futures_lite::future::block_on(wasi_fs::stat_path(namespace, "/data", crate::Follow::No))
            .expect("the file exists")
            .modified_nanos
    }

    /// `fd_filestat_set_times` is decided by the rights the descriptor was
    /// opened with: `FD_FILESTAT_SET_TIMES` gives the file `write`, and
    /// reading alone gives it nothing to retime with.
    #[test]
    fn fd_filestat_set_times_needs_the_set_times_right() {
        use futures_lite::future::block_on;

        let namespace = namespace_with_file();
        let before = modified_nanos(&namespace);
        let file = |rights| FsDescriptor {
            flags: p1_descriptor_flags(rights, 0),
            ..match regular_file() {
                Preview1Descriptor::File { descriptor, .. } => descriptor,
                _ => unreachable!("regular_file is a file"),
            }
        };

        let reader = file(P1_RIGHT_FD_READ);
        assert!(matches!(
            reader.grant_touch().err(),
            Some(fs_types::ErrorCode::ReadOnly)
        ));
        assert_eq!(modified_nanos(&namespace), before);

        let retimer = file(P1_RIGHT_FD_FILESTAT_SET_TIMES);
        block_on(wasi_fs::set_times(
            &namespace,
            retimer
                .grant_touch()
                .expect("FD_FILESTAT_SET_TIMES gives write"),
            None,
            Some(77),
            78,
        ))
        .expect("the descriptor with the right sets the times");
        assert_eq!(modified_nanos(&namespace), 77);
    }

    /// `path_filestat_set_times` is decided by the base: reading a
    /// directory gives no right to retime below it, and the right to create
    /// below it does.
    #[test]
    fn path_filestat_set_times_is_decided_by_the_base() {
        use futures_lite::future::block_on;

        let namespace = namespace_with_file();
        let before = modified_nanos(&namespace);
        let base = |rights| FsDescriptor {
            path: "/".into(),
            kind: FsNodeKind::Directory,
            flags: p1_descriptor_flags(rights, 0),
            identity: None,
            mount: None,
        };

        let reader = base(P1_RIGHT_FD_READDIR);
        assert_eq!(
            p1_grant::<wasi_fs::Directory, wasi_fs::Retime>(&reader).err(),
            Some(p1::errno::ROFS)
        );
        assert_eq!(modified_nanos(&namespace), before);

        let creator = base(P1_RIGHT_PATH_CREATE_FILE);
        block_on(wasi_fs::set_times_at(
            &namespace,
            p1_grant(&creator).expect("PATH_CREATE_FILE gives mutate-directory"),
            "data",
            None,
            Some(88),
            89,
        ))
        .expect("the base with the right sets the times");
        assert_eq!(modified_nanos(&namespace), 88);
    }

    fn connected_socket_entry<Net: ComponentHostNetwork>(
        service: Net,
        stream: Net::TcpStream,
    ) -> Preview1DescriptorEntry<Net> {
        Preview1DescriptorEntry {
            descriptor: Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
                WasixTcpSocket::Connected {
                    family: WasixSocketFamily::Ipv4,
                    stream: WasixOwnedTcpStream::new(service, stream),
                    peer_address: crate::NetworkIpAddress::Ipv4(crate::Ipv4Address::new([
                        127, 0, 0, 1,
                    ])),
                    peer_port: 80,
                    options: WasixSocketOptions::default(),
                },
            )),
            close_on_exec: false,
            fdflags: 0,
        }
    }

    fn listening_socket_entry<Net: ComponentHostNetwork>(
        service: Net,
        listener: Net::TcpListener,
    ) -> Preview1DescriptorEntry<Net> {
        Preview1DescriptorEntry {
            descriptor: Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
                WasixTcpSocket::Listening {
                    family: WasixSocketFamily::Ipv4,
                    listener: WasixOwnedTcpListener::new(service, listener),
                    local_port: 8080,
                    options: WasixSocketOptions::default(),
                },
            )),
            close_on_exec: false,
            fdflags: 0,
        }
    }

    /// A program that exits with a listener open retires it.
    ///
    /// Nothing on any preview1 path touched a listener before #191:
    /// `fd_close` dropped the table entry and the network service had no
    /// `tcp_listener_close` to call, so every listener a program opened
    /// stayed installed on every shard for the rest of the boot, holding
    /// the local port that `is_tcp_local_port_free` consults.
    #[test]
    fn a_preview1_listening_descriptor_retires_its_listener_when_its_table_goes_away() {
        let (service, closed) = crate::test_support::recording_listener_network_service();
        let table =
            Preview1DescriptorTable::from_entries(vec![Some(listening_socket_entry(service, 21))]);

        assert_eq!(
            closed.count(),
            0,
            "a live descriptor holds its listener open"
        );
        drop(table);
        assert_eq!(
            closed.count(),
            1,
            "the descriptor table takes its listeners with it"
        );
        assert_eq!(closed.last(), 21);
    }

    /// A listener two descriptors share is retired when the second one
    /// goes, not the first: `exec` hands the child a copy of the table,
    /// and the slab slot a retirement frees is handed to the next
    /// listen.
    #[test]
    fn a_preview1_listener_outlives_every_descriptor_but_the_last() {
        let (service, closed) = crate::test_support::recording_listener_network_service();
        let mut table =
            Preview1DescriptorTable::from_entries(vec![Some(listening_socket_entry(service, 21))]);
        let inherited = table.clone_for_exec();

        assert_eq!(table.close(0), p1::errno::SUCCESS);
        assert_eq!(
            closed.count(),
            0,
            "the inherited descriptor still holds the listener"
        );
        drop(table);
        assert_eq!(closed.count(), 0);
        drop(inherited);
        assert_eq!(
            closed.count(),
            1,
            "the last descriptor retires the listener"
        );
    }

    /// A program that exits with a socket open retires it.
    ///
    /// `fd_close` used to be the only thing that touched a preview1
    /// socket's netstack stream, and it only dropped the descriptor —
    /// so a program that exited, whether or not it had closed the fd,
    /// left the connection in its shard. In #184 eleven of them were
    /// still there, one per iteration of the workload that had opened
    /// them, each advertising a full receive window with nobody left to
    /// read it.
    #[test]
    fn a_preview1_socket_descriptor_retires_its_stream_when_its_table_goes_away() {
        let (service, closed) = crate::test_support::recording_network_service();
        let table =
            Preview1DescriptorTable::from_entries(vec![Some(connected_socket_entry(service, 11))]);

        assert_eq!(closed.count(), 0, "a live descriptor holds its stream open");
        drop(table);
        assert_eq!(
            closed.count(),
            1,
            "the descriptor table takes its sockets with it"
        );
        assert_eq!(closed.last(), 11);
    }

    /// The retirement reaches the wire: the last descriptor holding a
    /// connection puts its FIN on it.
    ///
    /// #184 gave the descriptor the duty of retiring its stream, and
    /// the tests above prove it discharges it. What retirement meant
    /// inside the stack was dropping the socket, clearing its timers
    /// and freeing its slot with nothing sent, so a program that
    /// exited with a connection open was invisible to its peer, which
    /// kept an established connection until its own timeout (#224).
    #[test]
    fn a_preview1_socket_descriptor_puts_its_fin_on_the_wire_when_its_table_goes_away() {
        use crate::network::EstablishedTcpFixture;
        use helios_netstack::TcpFlags;

        let fixture = EstablishedTcpFixture::new();
        let table = Preview1DescriptorTable::from_entries(vec![Some(connected_socket_entry(
            fixture.service(),
            fixture.stream(),
        ))]);

        assert!(
            fixture.drive().is_empty(),
            "a live descriptor owes its peer nothing"
        );

        drop(table);
        let segments = fixture.drive();
        let fin = segments
            .iter()
            .find(|segment| segment.flags.contains(TcpFlags::FIN))
            .expect("the descriptor table takes its connection down with a FIN");
        assert_eq!(
            fin.sequence,
            EstablishedTcpFixture::LOCAL_SEQUENCE.wrapping_add(1),
            "the FIN carries this side's send sequence"
        );
        assert!(
            !fin.flags.contains(TcpFlags::RST),
            "a connection with nothing unread is closed, not aborted"
        );
    }

    /// The FIN leaves on the next executor turn, not on the next
    /// protocol timer.
    ///
    /// #224 put the segment on the outbound queue and #232 kicked the
    /// packet pump from the retirement drain, but a preview1 descriptor
    /// closes its stream straight from `WasixOwnedTcpStream::drop` and
    /// never went through that drain. Its FIN therefore sat in the
    /// queue until the pump's park expired, which on a guest with
    /// nothing else to send is `DHCP_RETRANSMIT_NANOS` — a second of
    /// silence the peer spends holding an established connection
    /// (#231). This asks the park, not the wire: the previous test
    /// already proves the FIN is queued, and what was wrong was when it
    /// left.
    #[test]
    fn a_preview1_socket_descriptor_wakes_the_packet_pump_when_its_table_goes_away() {
        use crate::network::EstablishedTcpFixture;

        let fixture = EstablishedTcpFixture::new();
        let table = Preview1DescriptorTable::from_entries(vec![Some(connected_socket_entry(
            fixture.service(),
            fixture.stream(),
        ))]);

        let mut park = fixture.pump_park();
        assert!(
            !park.released(),
            "a live descriptor gives the pump nothing to publish"
        );

        drop(table);
        assert!(
            park.released(),
            "the close that queued the FIN must release the pump's park \
             rather than leave it to the next protocol timer"
        );
    }

    /// A stream two descriptors share is retired when the second one
    /// goes, not the first. `exec` hands the child a copy of the table,
    /// so closing on the first drop would take the connection out from
    /// under the process that inherited it — and the slab slot it frees
    /// is handed straight to the next connection.
    #[test]
    fn a_preview1_socket_stream_outlives_every_descriptor_but_the_last() {
        let (service, closed) = crate::test_support::recording_network_service();
        let mut table =
            Preview1DescriptorTable::from_entries(vec![Some(connected_socket_entry(service, 11))]);
        let inherited = table.clone_for_exec();

        assert_eq!(table.close(0), p1::errno::SUCCESS);
        assert_eq!(
            closed.count(),
            0,
            "the inherited descriptor still holds the stream"
        );
        drop(table);
        assert_eq!(closed.count(), 0);
        drop(inherited);
        assert_eq!(closed.count(), 1, "the last descriptor retires the stream");
    }

    fn bound_udp_socket_entry<Net: ComponentHostNetwork>(
        service: Net,
        socket: Net::UdpSocket,
    ) -> Preview1DescriptorEntry<Net> {
        Preview1DescriptorEntry {
            descriptor: Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(
                WasixUdpSocket::Bound {
                    family: WasixSocketFamily::Ipv4,
                    socket: WasixOwnedUdpSocket::new(service, socket),
                    local_port: 5353,
                    options: WasixSocketOptions::default(),
                },
            )),
            close_on_exec: false,
            fdflags: 0,
        }
    }

    /// A program that exits with a datagram socket open retires it.
    ///
    /// Nothing on the preview1 path touched a bound UDP socket's
    /// netstack handle: `fd_close` dropped the descriptor and left the
    /// socket installed on every shard with its `udp_slots` entry held,
    /// for the rest of the boot (#190).
    #[test]
    fn a_preview1_udp_descriptor_retires_its_socket_when_its_table_goes_away() {
        let (service, closed) = crate::test_support::recording_udp_network_service();
        let table =
            Preview1DescriptorTable::from_entries(vec![Some(bound_udp_socket_entry(service, 21))]);

        assert_eq!(closed.count(), 0, "a live descriptor holds its socket open");
        drop(table);
        assert_eq!(
            closed.count(),
            1,
            "the descriptor table takes its sockets with it"
        );
        assert_eq!(closed.last(), 21);
    }

    /// A datagram socket two descriptors share is retired when the
    /// second one goes, not the first. `exec` hands the child a copy of
    /// the table, and the slab slot a retired handle frees is handed
    /// straight to the next bind.
    #[test]
    fn a_preview1_udp_socket_outlives_every_descriptor_but_the_last() {
        let (service, closed) = crate::test_support::recording_udp_network_service();
        let mut table =
            Preview1DescriptorTable::from_entries(vec![Some(bound_udp_socket_entry(service, 21))]);
        let inherited = table.clone_for_exec();

        assert_eq!(table.close(0), p1::errno::SUCCESS);
        assert_eq!(
            closed.count(),
            0,
            "the inherited descriptor still holds the socket"
        );
        drop(table);
        assert_eq!(closed.count(), 0);
        drop(inherited);
        assert_eq!(closed.count(), 1, "the last descriptor retires the socket");
    }

    fn connected_socket() -> Preview1Descriptor<crate::test_support::TestNetworkService> {
        Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(WasixTcpSocket::Connected {
            family: WasixSocketFamily::Ipv4,
            stream: WasixOwnedTcpStream::new(crate::test_support::test_network_service(), 9),
            peer_address: crate::NetworkIpAddress::Ipv4(crate::Ipv4Address::new([127, 0, 0, 1])),
            peer_port: 80,
            options: WasixSocketOptions::default(),
        }))
    }

    /// `poll_oneoff` used to emit an event for every subscription, so a guest
    /// could not tell which one fired. Only ready subscriptions are reported.
    #[test]
    fn poll_oneoff_reports_only_ready_subscriptions() {
        assert!(p1_fd_event(1, P1_EVENTTYPE_FD_READ, Ok(P1Readiness::Pending)).is_none());

        let ready = p1_fd_event(
            2,
            P1_EVENTTYPE_FD_READ,
            Ok(P1Readiness::Ready { bytes: 12 }),
        )
        .expect("a readable descriptor produces an event");
        assert_eq!(ready.userdata, 2);
        assert_eq!(ready.error, p1::errno::SUCCESS as u16);
        assert_eq!(ready.nbytes, 12);
        assert_eq!(ready.fd_flags, 0);

        let hangup = p1_fd_event(3, P1_EVENTTYPE_FD_READ, Ok(P1Readiness::Hangup))
            .expect("an ended stream is ready");
        assert_eq!(hangup.nbytes, 0);
        assert_eq!(hangup.fd_flags, P1_EVENT_FD_READWRITE_HANGUP);

        let bad = p1_fd_event(4, P1_EVENTTYPE_FD_READ, Err(p1::errno::BADF))
            .expect("a bad descriptor is reported immediately");
        assert_eq!(bad.error, p1::errno::BADF as u16);
    }

    /// A clock subscription is only ready once its deadline actually passed;
    /// until then it contributes the remaining time to the sleep budget.
    #[test]
    fn poll_oneoff_clock_is_ready_only_after_its_deadline() {
        match p1_clock_progress(1, 100, 40) {
            P1ClockProgress::Waiting(remaining) => {
                assert_eq!(remaining, Duration::from_nanos(60));
            }
            P1ClockProgress::Elapsed(_) => panic!("the deadline has not arrived yet"),
        }
        match p1_clock_progress(1, 100, 100) {
            P1ClockProgress::Elapsed(event) => {
                assert_eq!(event.event_type, P1_EVENTTYPE_CLOCK);
                assert_eq!(event.userdata, 1);
                assert_eq!(event.error, p1::errno::SUCCESS as u16);
            }
            P1ClockProgress::Waiting(_) => panic!("an arrived deadline is ready"),
        }
    }

    /// A zero timeout (deadline == now) is ready on the first pass, so
    /// `poll_oneoff` degrades to a non-blocking readiness snapshot.
    #[test]
    fn poll_oneoff_zero_timeout_never_sleeps() {
        let now = 5_000;
        // A relative timeout of zero anchors its deadline at `now`.
        assert!(matches!(
            p1_clock_progress(7, now, now),
            P1ClockProgress::Elapsed(_)
        ));
        // An absolute deadline already in the past behaves the same way.
        assert!(matches!(
            p1_clock_progress(7, now - 1, now),
            P1ClockProgress::Elapsed(_)
        ));
    }

    /// The clock and the descriptors race: a descriptor that is ready ends
    /// the wait immediately, and the clock event is not reported because its
    /// deadline never arrived.
    #[test]
    fn poll_oneoff_returns_early_when_a_descriptor_becomes_ready() {
        let mut ready = Vec::new();
        let mut earliest: Option<Duration> = None;

        if let Some(event) = p1_fd_event(
            11,
            P1_EVENTTYPE_FD_READ,
            Ok(P1Readiness::Ready { bytes: 4 }),
        ) {
            ready.push(event);
        }
        match p1_clock_progress(12, 1_000_000, 0) {
            P1ClockProgress::Elapsed(event) => ready.push(event),
            P1ClockProgress::Waiting(remaining) => {
                earliest =
                    Some(earliest.map_or(remaining, |current: Duration| current.min(remaining)));
            }
        }

        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].userdata, 11);
        assert_eq!(ready[0].event_type, P1_EVENTTYPE_FD_READ);
        // The clock only ever bounded the sleep; it never became an event.
        assert_eq!(earliest, Some(Duration::from_nanos(1_000_000)));
    }

    /// The sleep is bounded by the earliest of several clock deadlines.
    #[test]
    fn poll_oneoff_sleeps_until_the_earliest_deadline() {
        let mut earliest: Option<Duration> = None;
        for deadline in [900u64, 300, 700] {
            if let P1ClockProgress::Waiting(remaining) = p1_clock_progress(0, deadline, 100) {
                earliest =
                    Some(earliest.map_or(remaining, |current: Duration| current.min(remaining)));
            }
        }
        assert_eq!(earliest, Some(Duration::from_nanos(200)));
    }

    /// Regular files never block, in either direction. They used to fall
    /// through to `Ok(0)`, which `epoll` read as "not ready".
    #[test]
    fn regular_files_are_always_ready() {
        let file = regular_file();
        for event_type in [P1_EVENTTYPE_FD_READ, P1_EVENTTYPE_FD_WRITE] {
            match p1_probe_descriptor(Some(&file), event_type) {
                Ok(P1Probe::Local(readiness)) => {
                    assert!(readiness.is_ready(), "a regular file never blocks");
                    assert!(!readiness.is_hangup());
                }
                Ok(P1Probe::Network(_)) => panic!("a file is not a socket"),
                Err(errno) => panic!("probing a regular file failed with {errno}"),
            }
        }
    }

    /// A connected socket's readiness is only knowable through the network
    /// service, so the probe says so instead of silently reporting zero.
    #[test]
    fn connected_sockets_defer_to_the_network_service() {
        let socket = connected_socket();
        assert!(matches!(
            p1_probe_descriptor(Some(&socket), P1_EVENTTYPE_FD_READ),
            Ok(P1Probe::Network(P1NetworkProbe::TcpStream(9)))
        ));
        assert!(matches!(
            p1_probe_descriptor(Some(&socket), P1_EVENTTYPE_FD_WRITE),
            Ok(P1Probe::Network(P1NetworkProbe::TcpStream(9)))
        ));
    }

    /// Drive one call into a WASIX store's guest through its turns, the
    /// way the program driver does.
    fn drive_wasix_call(
        activity: &crate::InstanceActivity,
        signal_state: &WasixSignalState,
        call: impl core::future::Future<Output = wasmtime::Result<()>>,
    ) -> wasmtime::Result<()> {
        let turns = wasix_turns(activity, store_signals(signal_state));
        futures_lite::future::block_on(turns.drive(call))
    }

    /// The signals of a store whose process mailbox is `process`, with a
    /// thread mailbox of its own.
    fn store_signals(process: &WasixSignalState) -> WasixStoreSignals {
        WasixStoreSignals::new(WasixSignalState::new(), process.clone())
    }

    /// The turns of a WASIX store that answers to `signals`.
    fn wasix_turns(
        activity: &crate::InstanceActivity,
        signals: WasixStoreSignals,
    ) -> crate::component::GuestTurns<
        crate::test_support::ManualClockCpu,
        crate::test_support::TestRuntimeState,
        WasixStoreSignals,
    > {
        crate::component::GuestTurns::new(
            activity.turns(),
            crate::test_support::ManualClockCpu::new(),
            crate::test_support::TestRuntimeState::default(),
            signals,
        )
    }

    /// A signal raised while the guest is in its last host call ends the
    /// program with `128 + signal`, although the guest then returns from
    /// its export without meeting another interrupt check. The call meets
    /// the signal where it completes, and the signal takes precedence
    /// over the status the guest asked for, as it did when a call hook
    /// delivered it on the way back from that host call.
    #[test]
    fn a_signal_raised_in_the_last_host_call_exits_128_plus_the_signal() {
        let registry = crate::InstanceRegistry::new();
        let activity = crate::InstanceActivity::new(registry.register("signalled", 0), 0);
        let signal_state = WasixSignalState::new();
        let sender = signal_state.clone();
        // The last host call is where the program is signalled; it
        // returns without suspending, and so does the export.
        let call = core::future::poll_fn(move |_| {
            sender.raise(15);
            core::task::Poll::Ready(Ok(()))
        });

        let error =
            drive_wasix_call(&activity, &signal_state, call).expect_err("the signal ends the call");
        assert_eq!(preview1_exit_status(&error, Some(0)), Some(143));
        assert_eq!(
            signal_state.take_pending(),
            None,
            "the signal is delivered once"
        );
    }

    /// Counts the wakes a turn receives.
    #[derive(Default)]
    struct CountingWaker {
        wakes: core::sync::atomic::AtomicUsize,
    }

    impl std::task::Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.wakes.fetch_add(1, AtomicOrdering::AcqRel);
        }
    }

    /// A signal raised while the guest is suspended in a host call that
    /// never resolves wakes the turn, which ends the call with
    /// `128 + signal` instead of waiting for the host call; a
    /// condemnation raised in the same wait comes second.
    #[test]
    fn a_signal_ends_a_call_suspended_in_a_host_call_that_never_resolves() {
        let registry = crate::InstanceRegistry::new();
        let instance = registry.register("sleeping", 0);
        let activity = crate::InstanceActivity::new(instance.clone(), 0);
        let signal_state = WasixSignalState::new();
        let turns = wasix_turns(&activity, store_signals(&signal_state));
        let call = core::future::pending::<wasmtime::Result<()>>();
        let wakes = Arc::new(CountingWaker::default());
        let waker = core::task::Waker::from(wakes.clone());
        let mut cx = core::task::Context::from_waker(&waker);
        let mut driven = core::pin::pin!(turns.drive(call));

        assert!(driven.as_mut().poll(&mut cx).is_pending());
        signal_state.raise(9);
        assert_eq!(
            registry.request_kill(instance.id(), crate::KillReason::Operator, 0),
            crate::KillOutcome::Requested
        );
        assert!(
            wakes.wakes.load(AtomicOrdering::Acquire) >= 1,
            "the raise wakes the turn"
        );
        let core::task::Poll::Ready(Err(error)) = driven.as_mut().poll(&mut cx) else {
            panic!("the signal ends the suspended call");
        };
        assert_eq!(preview1_exit_status(&error, None), Some(137));
        assert_eq!(signal_state.take_pending(), None, "delivered once");
    }

    /// A signal raised on one thread of a process reaches that thread's
    /// store alone: it ends the call the thread is suspended in with
    /// `128 + signal`, and a sibling store of the same process, suspended
    /// too, is neither woken nor handed the signal.
    #[test]
    fn a_thread_signal_ends_only_the_target_threads_suspended_call() {
        let registry = crate::InstanceRegistry::new();
        let instance = registry.register("threaded", 0);
        let process = WasixSignalState::new();
        let caller_thread = WasixSignalState::new();
        let target_thread = WasixSignalState::new();
        let caller_signals = WasixStoreSignals::new(caller_thread.clone(), process.clone());
        let caller_activity = crate::InstanceActivity::new(instance.clone(), 0);
        let target_activity = crate::InstanceActivity::new(instance, 0);
        let caller_turns = wasix_turns(&caller_activity, caller_signals.clone());
        let target_turns = wasix_turns(
            &target_activity,
            WasixStoreSignals::new(target_thread.clone(), process.clone()),
        );
        let caller_wakes = Arc::new(CountingWaker::default());
        let caller_waker = core::task::Waker::from(caller_wakes.clone());
        let mut caller_cx = core::task::Context::from_waker(&caller_waker);
        let target_wakes = Arc::new(CountingWaker::default());
        let target_waker = core::task::Waker::from(target_wakes.clone());
        let mut target_cx = core::task::Context::from_waker(&target_waker);
        let mut caller_call =
            core::pin::pin!(caller_turns.drive(core::future::pending::<wasmtime::Result<()>>()));
        let mut target_call =
            core::pin::pin!(target_turns.drive(core::future::pending::<wasmtime::Result<()>>()));

        assert!(caller_call.as_mut().poll(&mut caller_cx).is_pending());
        assert!(target_call.as_mut().poll(&mut target_cx).is_pending());
        target_thread.raise(9);

        assert_eq!(
            caller_wakes.wakes.load(AtomicOrdering::Acquire),
            0,
            "the raise does not wake a sibling thread's turn"
        );
        assert!(
            target_wakes.wakes.load(AtomicOrdering::Acquire) >= 1,
            "the raise wakes the target thread's turn"
        );
        assert!(
            caller_call.as_mut().poll(&mut caller_cx).is_pending(),
            "the sibling's suspended call goes on"
        );
        assert!(
            caller_signals.owed_stop().is_none(),
            "the sibling is owed nothing"
        );
        let core::task::Poll::Ready(Err(error)) = target_call.as_mut().poll(&mut target_cx) else {
            panic!("the signal ends the target thread's suspended call");
        };
        assert_eq!(preview1_exit_status(&error, None), Some(137));
        assert_eq!(target_thread.take_pending(), None, "delivered once");
        assert_eq!(
            process.take_pending(),
            None,
            "the process mailbox is untouched"
        );
    }

    /// A store owes its guest a signal raised on its own thread ahead of
    /// one raised on its process, and the process's after that. The stop
    /// says which it was: a thread-directed signal ends the thread alone,
    /// a process-directed one the whole process.
    #[test]
    fn a_store_takes_its_threads_signal_before_its_processs() {
        let process = WasixSignalState::new();
        let thread = WasixSignalState::new();
        let signals = WasixStoreSignals::new(thread.clone(), process.clone());
        process.raise(15);
        thread.raise(9);

        let stop = signals.owed_stop().expect("a raised thread owes a stop");
        assert_eq!(
            stop.downcast_ref::<WasixThreadSignalExit>()
                .map(|exit| exit.signal),
            Some(9)
        );
        let stop = signals.owed_stop().expect("a raised process owes a stop");
        assert_eq!(
            stop.downcast_ref::<Preview1SignalExit>()
                .map(|exit| exit.signal),
            Some(15)
        );
        assert!(signals.owed_stop().is_none());
    }

    /// A call that returns with no signal pending keeps its own result.
    #[test]
    fn a_call_with_no_signal_pending_keeps_its_result() {
        let registry = crate::InstanceRegistry::new();
        let activity = crate::InstanceActivity::new(registry.register("quiet", 0), 0);
        let signal_state = WasixSignalState::new();
        let call = core::future::ready(Ok(()));

        assert!(drive_wasix_call(&activity, &signal_state, call).is_ok());
    }

    /// A signal and a condemnation that both reach the guest in its last
    /// host call end the call with the signal, the precedence the
    /// interrupt callback gives them.
    #[test]
    fn a_signal_met_at_completion_is_delivered_ahead_of_a_kill() {
        let registry = crate::InstanceRegistry::new();
        let instance = registry.register("signalled-and-condemned", 0);
        let activity = crate::InstanceActivity::new(instance.clone(), 0);
        let signal_state = WasixSignalState::new();
        let sender = signal_state.clone();
        let call = core::future::poll_fn(move |_| {
            sender.raise(2);
            assert_eq!(
                registry.request_kill(instance.id(), crate::KillReason::Operator, 0),
                crate::KillOutcome::Requested
            );
            core::task::Poll::Ready(Ok(()))
        });

        let error =
            drive_wasix_call(&activity, &signal_state, call).expect_err("the call is stopped");
        assert!(error.downcast_ref::<crate::InstanceKilled>().is_none());
        assert_eq!(preview1_exit_status(&error, None), Some(130));
    }

    fn memory_of(bytes: &[u8]) -> Preview1Memory {
        Preview1Memory {
            base: bytes.as_ptr() as usize,
            len: bytes.len(),
        }
    }

    fn iovec_table(entries: &[(u32, u32)]) -> Vec<u8> {
        entries
            .iter()
            .flat_map(|(ptr, len)| ptr.to_le_bytes().into_iter().chain(len.to_le_bytes()))
            .collect()
    }

    /// `random_get` with a length no memory could hold is refused before
    /// anything is drawn or allocated for it, and a length that fits is
    /// filled in full through the fixed-size chunk.
    #[test]
    fn random_get_refuses_a_length_beyond_guest_memory_before_drawing() {
        let mut guest = vec![0_u8; 3 * P1_RANDOM_CHUNK_BYTES];
        let memory = Preview1Memory {
            base: guest.as_mut_ptr() as usize,
            len: guest.len(),
        };
        let mut draws = 0;
        for (ptr, len) in [(0, u32::MAX), (16, u32::MAX), (u32::MAX, 1), (u32::MAX, 0)] {
            let status = p1_fill_random(memory, GuestAddress::wasm32(ptr), len, |_| draws += 1);
            assert_eq!(status, p1::errno::FAULT, "random_get({ptr}, {len})");
        }
        assert_eq!(draws, 0, "a refused length draws no entropy");

        let wanted = guest.len() - 5;
        let mut drawn = Vec::new();
        let status = p1_fill_random(memory, GuestAddress::wasm32(5), wanted as u32, |chunk| {
            drawn.push(chunk.len());
            chunk.fill(0xA5);
        });
        assert_eq!(status, p1::errno::SUCCESS);
        assert_eq!(
            drawn,
            [
                P1_RANDOM_CHUNK_BYTES,
                P1_RANDOM_CHUNK_BYTES,
                P1_RANDOM_CHUNK_BYTES - 5
            ],
            "each pass draws exactly the bytes it writes"
        );
        assert!(guest[..5].iter().all(|byte| *byte == 0));
        assert!(guest[5..].iter().all(|byte| *byte == 0xA5));
    }

    /// `poll_oneoff` refuses a subscription count above what the process's
    /// descriptor limit admits (`EINVAL`) before anything is collected for
    /// it, and an array that does not lie in guest memory (`EFAULT`).
    #[test]
    fn poll_oneoff_refuses_a_subscription_count_beyond_its_bounds() {
        assert_eq!(p1_subscription_limit(DescriptorLimit::DEFAULT), 2049);
        let guest = vec![0_u8; 4 * P1_SUBSCRIPTION_SIZE as usize];
        let memory = memory_of(&guest);
        let max = p1_subscription_limit(DescriptorLimit::new(1));
        assert_eq!(max, 3);
        for count in [u32::MAX, 4] {
            assert_eq!(
                p1_read_subscriptions(memory, GuestAddress::wasm32(0), count, max, 0, 0).err(),
                Some(p1::errno::INVAL),
                "poll_oneoff count {count} over a bound of {max}",
            );
        }
        let max = p1_subscription_limit(DescriptorLimit::new(1 << 20));
        for (ptr, count) in [(0, 5), (16, 4), (u32::MAX, 1)] {
            assert_eq!(
                p1_read_subscriptions(memory, GuestAddress::wasm32(ptr), count, max, 0, 0).err(),
                Some(p1::errno::FAULT),
                "poll_oneoff subscriptions at {ptr}, count {count}",
            );
        }
        let subscriptions = p1_read_subscriptions(memory, GuestAddress::wasm32(0), 4, max, 0, 0)
            .expect("four subscriptions fit");
        assert_eq!(subscriptions.len(), 4);
    }

    /// Vectored I/O refuses an iovec count above `IOV_MAX` (`EINVAL`) and
    /// validates the iovec array and every buffer against guest memory
    /// (`EFAULT`) before anything is gathered.
    #[test]
    fn vectored_io_refuses_iovecs_guest_memory_cannot_hold() {
        let guest = vec![0_u8; 4096];
        let memory = memory_of(&guest);
        let table_at = |table: &[u8]| {
            let mut guest = guest.clone();
            guest[..table.len()].copy_from_slice(table);
            guest
        };
        // More iovecs than `IOV_MAX`.
        for count in [u32::MAX, P1_IOV_MAX as u32 + 1] {
            assert_eq!(
                p1_read_iovs_with_byte_len(memory, GuestAddress::wasm32(0), count).err(),
                Some(p1::errno::INVAL)
            );
        }
        // The array does not fit.
        assert_eq!(
            p1_read_iovs_with_byte_len(memory, GuestAddress::wasm32(0), P1_IOV_MAX as u32).err(),
            Some(p1::errno::FAULT)
        );
        // A buffer lies outside memory.
        let table = table_at(&iovec_table(&[(0, 16), (4090, u32::MAX)]));
        assert_eq!(
            p1_read_iovs_with_byte_len(memory_of(&table), GuestAddress::wasm32(0), 2).err(),
            Some(p1::errno::FAULT)
        );
        // Overlapping buffers may add up to more than memory holds: nothing
        // is sized by the total, so that is a valid transfer.
        let table = table_at(&iovec_table(&[(0, 4096), (0, 4096)]));
        let layout = p1_read_iovs_with_byte_len(memory_of(&table), GuestAddress::wasm32(0), 2)
            .expect("overlapping buffers are valid");
        assert_eq!(layout.byte_len, 8192);
        // Overlapping buffers that fit are a valid transfer.
        let table = table_at(&iovec_table(&[(64, 2048), (1024, 1024)]));
        let layout = p1_read_iovs_with_byte_len(memory_of(&table), GuestAddress::wasm32(0), 2)
            .expect("buffers that fit are valid");
        assert_eq!(layout.byte_len, 3072);
    }

    /// Iovecs that overlap may sum past what a 32-bit byte count holds
    /// (#806): that is refused with `EINVAL` as `readv` does, so the layout
    /// a read takes its capacity from always fits the `u32` `read_stdin`
    /// takes.
    #[test]
    fn iovecs_summing_past_u32_are_refused_not_converted() {
        let table = iovec_table(&[(0, 0x8000_0000), (0, 0x8000_0000)]);
        let memory = Preview1Memory {
            base: table.as_ptr() as usize,
            // Linear memory of 4 GiB: each buffer is inside it, and only
            // the iovec table, which is real, is ever read.
            len: 1 << 32,
        };
        assert_eq!(
            p1_read_iovs_with_byte_len(memory, GuestAddress::wasm32(0), 2).err(),
            Some(p1::errno::INVAL)
        );
    }

    /// A gathered write copies guest memory in chunks no larger than
    /// [`GUEST_IO_CHUNK_BYTES`], in iovec order, across overlapping and empty
    /// iovecs, and hands the sink every byte once.
    #[test]
    fn gathered_writes_move_in_bounded_chunks() {
        use futures_lite::future::block_on;

        let guest: Vec<u8> = (0..3 * GUEST_IO_CHUNK_BYTES)
            .map(|index| index as u8)
            .collect();
        let table = iovec_table(&[(0, 150_000), (7, 0), (50_000, 40_000)]);
        let table_at = guest.len() - table.len();
        let mut memory = guest.clone();
        memory[table_at..].copy_from_slice(&table);
        let memory = memory_of(&memory);
        let layout = p1_read_iovs_with_byte_len(memory, GuestAddress::wasm32(table_at as u32), 3)
            .expect("iovecs fit");
        let expected: Vec<u8> = guest[..150_000]
            .iter()
            .chain(&guest[50_000..90_000])
            .copied()
            .collect();
        let mut gather = Preview1Gather::new(memory, &layout).expect("buffers fit");
        let mut chunks = Vec::new();
        let sent = block_on(p1_send_gathered(&mut gather, |chunk| {
            chunks.push(chunk);
            core::future::ready(Ok(()))
        }))
        .expect("every chunk is taken");
        assert_eq!(sent as usize, expected.len());
        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.len() <= GUEST_IO_CHUNK_BYTES)
        );
        assert_eq!(chunks.concat(), expected);
    }

    /// A sink that refuses after taking some chunks leaves them written:
    /// the count of what went is the result, and a refusal before any
    /// went is the error.
    #[test]
    fn a_refusal_after_some_chunks_reports_what_went() {
        use futures_lite::future::block_on;

        let guest = vec![1_u8; 3 * GUEST_IO_CHUNK_BYTES];
        let table = iovec_table(&[(0, (3 * GUEST_IO_CHUNK_BYTES) as u32)]);
        let mut memory = guest.clone();
        memory[..table.len()].copy_from_slice(&table);
        let memory = memory_of(&memory);
        let layout = p1_read_iovs_with_byte_len(memory, GuestAddress::wasm32(0), 1).expect("fits");

        let mut calls = 0;
        let mut gather = Preview1Gather::new(memory, &layout).expect("fits");
        let sent = block_on(p1_send_gathered(&mut gather, |_| {
            calls += 1;
            core::future::ready(if calls == 1 {
                Ok(())
            } else {
                Err(p1::errno::AGAIN)
            })
        }));
        assert_eq!(sent, Ok(GUEST_IO_CHUNK_BYTES as u32));

        let mut gather = Preview1Gather::new(memory, &layout).expect("fits");
        let sent = block_on(p1_send_gathered(&mut gather, |_| {
            core::future::ready(Err(p1::errno::AGAIN))
        }));
        assert_eq!(sent, Err(p1::errno::AGAIN));
    }

    /// An empty transfer still reaches the sink once, as an empty write.
    #[test]
    fn an_empty_gathered_write_reaches_the_sink_once() {
        use futures_lite::future::block_on;

        let guest = vec![0_u8; 64];
        let memory = memory_of(&guest);
        let layout = p1_read_iovs_with_byte_len(memory, GuestAddress::wasm32(0), 0).expect("empty");
        let mut gather = Preview1Gather::new(memory, &layout).expect("empty");
        let mut calls = Vec::new();
        let sent = block_on(p1_send_gathered(&mut gather, |chunk| {
            calls.push(chunk.len());
            core::future::ready(Ok(()))
        }));
        assert_eq!(sent, Ok(0));
        assert_eq!(calls, [0]);
    }

    /// Guest memory of `len` payload bytes followed by one iovec naming
    /// them, and the layout that iovec decodes to.
    fn one_iovec(len: usize) -> (Vec<u8>, Preview1IovLayout) {
        let mut guest: Vec<u8> = (0..len).map(|index| index as u8).collect();
        guest.extend(iovec_table(&[(0, len as u32)]));
        let layout =
            p1_read_iovs_with_byte_len(memory_of(&guest), GuestAddress::wasm32(len as u32), 1)
                .expect("the iovec fits");
        (guest, layout)
    }

    /// A datagram's payload is gathered whole, across its buffers and past
    /// the size of a stream chunk, and one byte more than the protocol
    /// carries is `EMSGSIZE` with nothing taken.
    #[test]
    fn a_datagram_is_gathered_whole_or_refused() {
        let max = GUEST_IO_CHUNK_BYTES + 7;
        let mut guest: Vec<u8> = (0..max + 1).map(|index| index as u8).collect();
        let table = guest.len() as u32;
        guest.extend(iovec_table(&[(0, 9), (9, max as u32 - 9), (max as u32, 1)]));
        let memory = memory_of(&guest);

        let whole = p1_read_iovs_with_byte_len(memory, GuestAddress::wasm32(table), 2)
            .expect("the iovecs fit");
        let mut gather = Preview1Gather::new(memory, &whole).expect("buffers fit");
        assert_eq!(gather.datagram(max).as_deref(), Ok(&guest[..max]));
        assert_eq!(gather.remaining(), 0);

        let over = p1_read_iovs_with_byte_len(memory, GuestAddress::wasm32(table), 3)
            .expect("the iovecs fit");
        let mut gather = Preview1Gather::new(memory, &over).expect("buffers fit");
        assert_eq!(gather.datagram(max), Err(p1::errno::MSGSIZE));
        assert_eq!(gather.remaining(), max + 1);
    }

    /// A file for [`p1_write_gathered`] to land chunks in, which records
    /// where each was asked to go.
    struct ChunkedFile {
        bytes: Vec<u8>,
        positions: Vec<WritePosition>,
        /// How many more chunks land before the file refuses one.
        accepts: usize,
        /// What another writer appends after the first chunk landed.
        other_writer: &'static [u8],
    }

    impl ChunkedFile {
        fn new(bytes: &[u8], accepts: usize) -> Self {
            Self {
                bytes: bytes.to_vec(),
                positions: Vec::new(),
                accepts,
                other_writer: b"",
            }
        }
    }

    impl Preview1ChunkedFile for ChunkedFile {
        type Refusal = i32;

        async fn land(
            &mut self,
            gather: &mut Preview1Gather,
            position: WritePosition,
            len: usize,
        ) -> Result<u64, i32> {
            if self.positions.len() == 1 {
                self.bytes.extend_from_slice(self.other_writer);
            }
            self.positions.push(position);
            if self.accepts == 0 {
                return Err(p1::errno::IO);
            }
            self.accepts -= 1;
            let at = match position {
                WritePosition::End => self.bytes.len(),
                WritePosition::At(at) => at as usize,
            };
            if self.bytes.len() < at + len {
                self.bytes.resize(at + len, 0);
            }
            gather.fill(&mut self.bytes[at..at + len]);
            Ok((at + len) as u64)
        }
    }

    /// Every chunk of an append goes to the end the file has when the
    /// chunk arrives, so bytes another writer appended between two chunks
    /// are still there afterwards.
    #[test]
    fn a_chunked_append_never_lands_on_another_writers_bytes() {
        use futures_lite::future::block_on;

        let len = 2 * GUEST_IO_CHUNK_BYTES + 10;
        let (guest, layout) = one_iovec(len);
        let mut gather = Preview1Gather::new(memory_of(&guest), &layout).expect("buffers fit");
        let mut file = ChunkedFile::new(b"head", usize::MAX);
        file.other_writer = b"other";
        let landed = block_on(p1_write_gathered(
            &mut gather,
            WritePosition::End,
            &mut file,
        ));
        let expected: Vec<u8> = b"head"
            .iter()
            .chain(&guest[..GUEST_IO_CHUNK_BYTES])
            .chain(b"other")
            .chain(&guest[GUEST_IO_CHUNK_BYTES..len])
            .copied()
            .collect();
        assert_eq!(landed, Ok((expected.len() as u64, len as u32)));
        assert_eq!(file.positions, [WritePosition::End; 3]);
        assert!(file.bytes == expected, "the other writer's bytes survive");
    }

    /// A positioned write's chunks follow one another from the position
    /// it was given.
    #[test]
    fn a_chunked_positioned_write_continues_where_the_last_chunk_ended() {
        use futures_lite::future::block_on;

        let len = 2 * GUEST_IO_CHUNK_BYTES + 10;
        let (guest, layout) = one_iovec(len);
        let mut gather = Preview1Gather::new(memory_of(&guest), &layout).expect("buffers fit");
        let mut file = ChunkedFile::new(&[0xEE; 16], usize::MAX);
        let landed = block_on(p1_write_gathered(
            &mut gather,
            WritePosition::At(3),
            &mut file,
        ));
        assert_eq!(landed, Ok((3 + len as u64, len as u32)));
        assert_eq!(
            file.positions,
            [
                WritePosition::At(3),
                WritePosition::At(3 + GUEST_IO_CHUNK_BYTES as u64),
                WritePosition::At(3 + 2 * GUEST_IO_CHUNK_BYTES as u64),
            ]
        );
        assert_eq!(file.bytes[..3], [0xEE; 3]);
        assert!(file.bytes[3..] == guest[..len]);
    }

    /// A file that refuses a chunk after taking some leaves a short write
    /// that ends where the last landed chunk did; a refusal of the first
    /// chunk is the error, and an empty write reaches the file once.
    #[test]
    fn a_refused_file_chunk_is_a_short_write() {
        use futures_lite::future::block_on;

        let (guest, layout) = one_iovec(3 * GUEST_IO_CHUNK_BYTES);
        let write = |accepts: usize| {
            let mut gather = Preview1Gather::new(memory_of(&guest), &layout).expect("buffers fit");
            let mut file = ChunkedFile::new(b"", accepts);
            let landed = block_on(p1_write_gathered(
                &mut gather,
                WritePosition::At(0),
                &mut file,
            ));
            (landed, file.positions.len())
        };
        let two = 2 * GUEST_IO_CHUNK_BYTES;
        assert_eq!(write(2), (Ok((two as u64, two as u32)), 3));
        assert_eq!(write(0), (Err(p1::errno::IO), 1));

        let (guest, layout) = one_iovec(0);
        let mut gather = Preview1Gather::new(memory_of(&guest), &layout).expect("empty");
        let mut file = ChunkedFile::new(b"four", usize::MAX);
        let landed = block_on(p1_write_gathered(
            &mut gather,
            WritePosition::End,
            &mut file,
        ));
        assert_eq!(landed, Ok((4, 0)));
        assert_eq!(file.positions, [WritePosition::End]);
    }
}
