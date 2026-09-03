use super::*;

pub(super) struct Preview1ProgramStore<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    pub(super) cpu: CpuImpl,
    pub(super) timer: crate::Timer<CpuImpl>,
    pub(super) spawner: crate::Spawner<CpuImpl>,
    pub(super) runtime_state: HostRuntimeState<CpuImpl, HostFs>,
    pub(super) instance: crate::RegisteredInstance,
    pub(super) parent_instance_id: Option<crate::InstanceId>,
    pub(super) filesystem: DebugFileSystem<HostRuntimeState<CpuImpl, HostFs>, HostFs>,
    pub(super) clock: crate::KernelClock<CpuImpl, HostRuntimeState<CpuImpl, HostFs>>,
    pub(super) wall_clock_cap: Option<crate::SetWallClockCap>,
    pub(super) cwd: Option<Preview1Cwd>,
    pub(super) arguments: Vec<String>,
    pub(super) environment: Vec<(String, String)>,
    pub(super) output_mode: OutputMode,
    pub(super) read_serial: crate::SerialReader,
    pub(super) serial_read_buffer: Vec<u8>,
    pub(super) write_serial: fn(&[u8]),
    pub(super) imported_memory: Option<SharedMemory>,
    pub(super) current_core_module: Option<Arc<WasmtimeCompiledCoreModule>>,
    pub(super) wasix_abi: bool,
    pub(super) entropy: crate::EntropyPool,
    pub(super) authority: ProcessAuthority,
    pub(super) tty_state: WasixTtyState,
    pub(super) signal_callback: Option<String>,
    pub(super) signal_state: WasixSignalState,
    pub(super) signal_dispositions: Vec<WasixSignalDisposition>,
    pub(super) descriptors: Preview1DescriptorTable,
    pub(super) asyncify: WasixAsyncifyState,
    pub(super) children: Vec<WasixChildProcess>,
    pub(super) thread_id: u32,
    pub(super) next_thread_id: u32,
    pub(super) threads: Vec<WasixThread>,
    pub(super) requested_exit: Option<u32>,
    pub(super) exec_replacement: Option<WasixExecReplacement<CpuImpl, HostFs>>,
}

impl<CpuImpl, HostFs> Preview1ProgramStore<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        cpu: CpuImpl,
        timer: crate::Timer<CpuImpl>,
        spawner: crate::Spawner<CpuImpl>,
        runtime_state: HostRuntimeState<CpuImpl, HostFs>,
        instance: crate::RegisteredInstance,
        parent_instance_id: Option<crate::InstanceId>,
        arguments: Vec<String>,
        environment: Vec<(String, String)>,
        authority: ProcessAuthority,
        output_mode: OutputMode,
        read_serial: crate::SerialReader,
        write_serial: fn(&[u8]),
        imported_memory: Option<SharedMemory>,
        filesystem: Option<DebugFileSystemSnapshot>,
        descriptors: Option<Preview1DescriptorTable>,
        signal_state: WasixSignalState,
        signal_dispositions: Vec<WasixSignalDisposition>,
        current_core_module: Option<Arc<WasmtimeCompiledCoreModule>>,
        wasix_abi: bool,
    ) -> Self {
        let filesystem = filesystem.map_or_else(
            || DebugFileSystem::new(runtime_state.clone()),
            |snapshot| DebugFileSystem::from_snapshot(runtime_state.clone(), snapshot),
        );
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
            instance,
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
            signal_dispositions,
            descriptors,
            asyncify: WasixAsyncifyState::new(),
            children: Vec::new(),
            thread_id: 0,
            next_thread_id: 1,
            threads: Vec::new(),
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
            crate::ProcessMemoryIdentity::new(self.instance.id().raw()),
            crate::GuestAddress::new(u64::from(address)),
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
        replacement: WasixExecReplacement<CpuImpl, HostFs>,
    ) {
        self.exec_replacement = Some(replacement);
    }

    pub(super) fn take_exec_replacement(
        &mut self,
    ) -> Option<WasixExecReplacement<CpuImpl, HostFs>> {
        self.exec_replacement.take()
    }

    pub(super) fn set_thread_id(&mut self, thread_id: u32) {
        self.thread_id = thread_id;
        self.next_thread_id = thread_id.saturating_add(1);
    }

    pub(super) fn allocate_thread_id(&mut self) -> Result<u32, i32> {
        let tid = self.next_thread_id;
        self.next_thread_id = self
            .next_thread_id
            .checked_add(1)
            .ok_or(p1::errno::OVERFLOW)?;
        Ok(tid)
    }

    pub(super) fn take_requested_exit(&mut self) -> Option<u32> {
        self.requested_exit.take()
    }

    pub(super) fn exec_context(&self) -> ProgramExecContext<CpuImpl, HostFs> {
        ProgramExecContext {
            cpu: self.cpu.clone(),
            timer: self.timer.clone(),
            spawner: self.spawner.clone(),
            runtime_state: self.runtime_state.clone(),
            instance_registry: self.runtime_state.instance_registry(),
            parent_instance_id: Some(self.instance.id()),
            read_serial: self.read_serial,
            write_serial: self.write_serial,
        }
    }

    /// Same naming as the component path: while this processor is
    /// running the program's guest code, pages it commits are the
    /// program's. See `component::runtime`'s `record_transition`.
    pub(super) fn record_transition(&self, transition: crate::InstanceExecutionTransition) {
        let now_nanos = self.now_nanos();
        let elapsed = crate::record_instance_transition(&self.instance, transition, now_nanos);
        let owner = match transition {
            crate::InstanceExecutionTransition::Resume => {
                Some(crate::MemoryOwner::new(self.instance.id().raw()))
            }
            crate::InstanceExecutionTransition::Pause if elapsed.is_some() => {
                Some(crate::MemoryOwner::NONE)
            }
            crate::InstanceExecutionTransition::Pause => None,
        };
        if let Some(owner) = owner {
            crate::set_user_memory_owner(self.cpu.current_processor(), owner);
        }
        if let Some(elapsed) = elapsed
            && self.runtime_state.profiling_enabled()
        {
            self.runtime_state.record_profile_stack_parts_nanos(
                crate::ProfileScope::User,
                "user;",
                self.instance.name(),
                elapsed,
            );
        }
    }

    pub(super) fn check_pending_kill(&self) -> Option<crate::KillReason> {
        self.instance.pending_kill()
    }

    /// Deliver stdout/stderr bytes to a sink that cannot block, or hand
    /// back the bounded child channel the caller has to push through.
    ///
    /// The split exists because the store is not `Sync`: holding `&self`
    /// across an `.await` would make every host-call future non-`Send`.
    /// Callers take the returned writer — a cheap handle clone — and then
    /// await or `try_write` it outside this borrow.
    pub(super) fn route_output(
        &self,
        stream: crate::ComponentOutputStreamKind,
        bytes: &[u8],
    ) -> Option<crate::ByteWriter> {
        if bytes.is_empty() {
            return None;
        }
        match self.output_mode.sink(stream) {
            crate::ComponentOutputSink::Local(local) => {
                local.write(&self.cpu, &self.runtime_state, self.write_serial, bytes);
                None
            }
            crate::ComponentOutputSink::Child(writer) => Some(writer.clone()),
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
                    self.filesystem.replace_with_snapshot(filesystem);
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

    pub(super) fn insert_thread(
        &mut self,
        tid: u32,
        signal_state: WasixSignalState,
        exit: futures::channel::oneshot::Receiver<u32>,
    ) {
        self.threads.push(WasixThread {
            tid,
            signal_state,
            exit: Some(exit),
            completed: None,
        });
    }

    pub(super) fn poll_thread_exit(&mut self, index: usize) -> Option<u32> {
        if let Some(code) = self.threads[index].completed {
            return Some(code);
        }
        let Some(exit) = self.threads[index].exit.as_mut() else {
            return self.threads[index].completed;
        };
        let waker = futures::task::noop_waker_ref();
        let mut context = Context::from_waker(waker);
        match Pin::new(exit).poll(&mut context) {
            Poll::Pending => None,
            Poll::Ready(Ok(code)) => {
                self.threads[index].completed = Some(code);
                self.threads[index].exit = None;
                Some(code)
            }
            Poll::Ready(Err(_)) => {
                let code = u32::from(p1::errno::IO as u16);
                self.threads[index].completed = Some(code);
                self.threads[index].exit = None;
                Some(code)
            }
        }
    }

    pub(super) fn find_thread_index(&self, tid: u32) -> Option<usize> {
        self.threads.iter().position(|thread| thread.tid == tid)
    }

    pub(super) async fn read_stdin(&mut self, max_bytes: usize) -> Bytes {
        let descriptor = self.descriptors.get_mut(0);
        let Some(Preview1Descriptor::Stdin { carry }) = descriptor else {
            return Bytes::new();
        };
        if carry.is_empty() {
            match &self.output_mode {
                OutputMode::Serial => loop {
                    (self.read_serial)(
                        &mut self.serial_read_buffer,
                        u32::try_from(max_bytes)
                            .unwrap_or_else(|_| panic!("stdin read capacity exceeds u32")),
                    );
                    if !self.serial_read_buffer.is_empty() {
                        *carry = Bytes::copy_from_slice(&self.serial_read_buffer);
                        break;
                    }
                    crate::yield_now().await;
                },
                OutputMode::Trace => {}
                OutputMode::Child { stdin_rx, .. } | OutputMode::RoutedChild { stdin_rx, .. } => {
                    if let Some(bytes) = stdin_rx.read().await {
                        *carry = bytes;
                    }
                }
            }
        }
        take_preview1_carry(carry, max_bytes)
    }

    /// Probe stdin for `poll_oneoff`/`epoll_wait` without blocking.
    ///
    /// Serial-backed stdin has no readiness notification, so the probe pulls
    /// whatever the console already has into the descriptor's carry; a later
    /// `fd_read` drains that carry, so nothing is lost. A child's stdin is a
    /// byte channel that answers directly, and trace output never delivers
    /// input at all.
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

    pub(super) fn chdir(&mut self, path: &str) -> i32 {
        let cwd = match self.resolve_cwd_target(path) {
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

    pub(super) fn resolve_cwd_target(&self, path: &str) -> Result<Preview1Cwd, i32> {
        let (guest_name, source_path, flags) = if path.starts_with('/') {
            let guest_name =
                crate::resolve_absolute_path(path).map_err(p1_errno_from_component_path)?;
            let (source_path, flags) = self.resolve_absolute_guest_path(&guest_name)?;
            (guest_name, source_path, flags)
        } else {
            let cwd = self.cwd.as_ref().ok_or(p1::errno::NOTCAPABLE)?;
            let guest_name = crate::resolve_child_path(&cwd.guest_name, path)
                .map_err(p1_errno_from_component_path)?;
            let source_path = crate::resolve_child_path(&cwd.descriptor.path, path)
                .map_err(p1_errno_from_component_path)?;
            (guest_name, source_path, cwd.descriptor.flags)
        };

        if !flags.contains(fs_types::DescriptorFlags::READ) {
            return Err(p1::errno::NOTCAPABLE);
        }
        let stat = self
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

pub(super) fn add_wasi_p1_imports<CpuImpl, HostFs>(
    linker: &mut CoreLinker<CompilerCoreStore<CpuImpl, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "random_get",
            |caller: Caller<'_, CompilerCoreStore<CpuImpl, HostFs>>, ptr: i32, len: i32| -> i32 {
                fill_random(
                    caller.data().memory(),
                    &caller.data().shared.entropy,
                    ptr as u32,
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
            |caller: Caller<'_, CompilerCoreStore<CpuImpl, HostFs>>,
             _id: i32,
             _precision: i64,
             ptr: i32|
             -> i32 {
                write_u64(
                    caller.data().memory(),
                    ptr as u32,
                    crate::monotonic_nanos(&caller.data().cpu),
                )
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_write",
            |caller: Caller<'_, CompilerCoreStore<CpuImpl, HostFs>>,
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
            |caller: Caller<'_, CompilerCoreStore<CpuImpl, HostFs>>,
             environ: i32,
             buf: i32|
             -> i32 { compiler_environ_get(caller, environ as u32, buf as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "environ_sizes_get",
            |caller: Caller<'_, CompilerCoreStore<CpuImpl, HostFs>>,
             count: i32,
             size: i32|
             -> i32 {
                let thread_count = compiler_plugin_worker_threads(&caller.data().cpu);
                let env_len = compiler_rayon_env_len(thread_count);
                let memory = caller.data().memory();
                let first = write_u32(memory, count as u32, 1);
                let second = write_u32(memory, size as u32, env_len);
                first.max(second)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_fdstat_get",
            |caller: Caller<'_, CompilerCoreStore<CpuImpl, HostFs>>, fd: i32, stat: i32| -> i32 {
                compiler_fd_fdstat_get(caller, fd, stat as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_close",
            |mut caller: Caller<'_, CompilerCoreStore<CpuImpl, HostFs>>, fd: i32| -> i32 {
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

pub(super) fn add_wasi_thread_spawn<CpuImpl, HostFs>(
    linker: &mut CoreLinker<CompilerCoreStore<CpuImpl, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap(
            "wasi",
            "thread-spawn",
            |caller: Caller<'_, CompilerCoreStore<CpuImpl, HostFs>>, start_arg: i32| -> i32 {
                let store_data = caller.data().clone();
                let next = store_data.shared.next_thread_id.fetch_update(
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
                let task = spawner.spawn(async move {
                    let mut store =
                        wasmtime::Store::new(instance_pre.module().engine(), store_data);
                    configure_compiler_core_store(&mut store);
                    let thread_started = store.data().cpu.now().ticks();
                    let result = instance_pre.instantiate(&mut store).and_then(|instance| {
                        let start = instance
                            .get_typed_func::<(i32, i32), ()>(&mut store, "wasi_thread_start")?;
                        start.call(&mut store, (thread_id, start_arg))
                    });
                    let thread_elapsed = store
                        .data()
                        .cpu
                        .now()
                        .ticks()
                        .saturating_sub(thread_started);
                    store.data().record_user_ticks(thread_elapsed);
                    if let Err(error) = result {
                        tracing::error!(thread_id, "compiler plugin thread failed: {error:#}");
                    }
                });
                shared.thread_tasks.lock().push(task);
                thread_id
            },
        )
        .map_err(map_program_runtime_error)?;
    Ok(())
}

pub(super) fn configure_preview1_program_store<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, HostFs>>,
) where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    store.call_hook(
        |mut caller: StoreContextMut<'_, Preview1ProgramStore<CpuImpl, HostFs>>, hook| {
            let transition = crate::wasmtime_adapter::store::translate_call_hook(hook);
            caller.data().record_transition(transition);
            if let Some(signal) = caller.data().signal_state.take_pending() {
                caller
                    .data_mut()
                    .request_exit(128u32.saturating_add(signal));
                return Err(wasmtime::Error::new(Preview1Exit));
            }
            if let Some(reason) = caller.data().check_pending_kill() {
                return Err(wasmtime::Error::from(crate::InstanceKilled { reason }));
            }
            Ok(())
        },
    );
    store.set_epoch_deadline(1);
    // Epoch ticks double as the kill observation point for CPU-bound
    // guests (see `store_with_state`): check the flag, otherwise yield.
    store.epoch_deadline_callback(|caller| {
        if let Some(reason) = caller.data().check_pending_kill() {
            return Err(wasmtime::Error::from(crate::InstanceKilled { reason }));
        }
        Ok(wasmtime::UpdateDeadline::Yield(1))
    });
}

pub(super) fn preview1_program_linker<CpuImpl, HostFs>(
    engine: &wasmtime::Engine,
) -> Result<CoreLinker<Preview1ProgramStore<CpuImpl, HostFs>>, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let mut linker = CoreLinker::new(engine);
    add_preview1_program_imports(&mut linker)?;
    Ok(linker)
}

pub(super) fn add_preview1_program_imports<CpuImpl, HostFs>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
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
            "wasi_snapshot_preview1",
            "args_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             argv: i32,
             argv_buf: i32|
             -> i32 { p1_args_get(&mut caller, argv as u32, argv_buf as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "environ_sizes_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             count: i32,
             size: i32|
             -> i32 { p1_environ_sizes_get(&mut caller, count as u32, size as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "environ_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             environ: i32,
             environ_buf: i32|
             -> i32 { p1_environ_get(&mut caller, environ as u32, environ_buf as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "clock_res_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             id: i32,
             resolution: i32|
             -> i32 { p1_clock_res_get(&mut caller, id, resolution as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "clock_time_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
            "wasi_snapshot_preview1",
            "random_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             ptr: i32,
             len: i32|
             -> i32 { p1_random_get(&mut caller, ptr as u32, len as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
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
            "wasi_snapshot_preview1",
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
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_close",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, fd: i32| -> i32 {
                caller.data_mut().descriptors.close(fd)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_prestat_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             buf: i32|
             -> i32 { p1_fd_prestat_get(&mut caller, fd, buf as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
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
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_fdstat_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             stat: i32|
             -> i32 { p1_fd_fdstat_get(&mut caller, fd, stat as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_fdstat_set_flags",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             fdflags: i32|
             -> i32 { p1_fd_fdstat_set_flags(&mut caller, fd, fdflags as u16) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_fdstat_set_rights",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, stat): (i32, i32)| {
                Box::new(async move { p1_fd_filestat_get(&mut caller, fd, stat as u32).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_filestat_set_size",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, size): (i32, i64)| {
                Box::new(async move { p1_fd_filestat_set_size(&mut caller, fd, size as u64).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_filestat_set_times",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, (fd,): (i32,)| {
                Box::new(async move { p1_fd_datasync(&mut caller, fd).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_sync",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, (fd,): (i32,)| {
                Box::new(async move { p1_fd_sync(&mut caller, fd).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_pread",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
            "wasi_snapshot_preview1",
            "fd_renumber",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             from: i32,
             to: i32|
             -> i32 { p1_fd_renumber(&mut caller, from, to) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
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
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_tell",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             offset: i32|
             -> i32 { p1_fd_tell(&mut caller, fd, offset as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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

pub(super) fn add_preview1_program_remaining_imports<CpuImpl, HostFs>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "path_create_directory",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             signal: i32|
             -> wasmtime::Result<i32> { p1_proc_raise(&mut caller, signal as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "sock_accept",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, how): (i32, i32)| {
                Box::new(async move { p1_sock_shutdown(&mut caller, fd, how as u8).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    Ok(())
}

pub(super) fn p1_args_sizes_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    argc: u32,
    argv_buf_size: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
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
    p1_write_u32(caller, memory, argc, count).max(p1_write_u32(caller, memory, argv_buf_size, size))
}

pub(super) fn p1_args_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    argv: u32,
    argv_buf: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let values = caller.data().arguments.clone();
    p1_write_string_array(caller, argv, argv_buf, values.iter().map(String::as_str))
}

pub(super) fn p1_environ_sizes_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    count: u32,
    size: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
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
    p1_write_u32(caller, memory, count, env_count).max(p1_write_u32(caller, memory, size, env_size))
}

pub(super) fn p1_environ_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    environ: u32,
    environ_buf: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let env = p1_environment_strings(caller.data());
    p1_write_string_array(caller, environ, environ_buf, env.iter().map(String::as_str))
}

pub(super) fn p1_clock_res_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    id: i32,
    resolution: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    match id {
        0 | 1 => {
            let Some(memory) = p1_memory(caller) else {
                return p1::errno::FAULT;
            };
            p1_write_u64(caller, memory, resolution, 1)
        }
        _ => p1::errno::INVAL,
    }
}

pub(super) fn p1_clock_time_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    id: i32,
    timestamp: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
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
    p1_write_u64(caller, memory, timestamp, value)
}

pub(super) fn p1_random_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    ptr: u32,
    len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let mut bytes = vec![0; len as usize];
    caller.data_mut().entropy.fill_secure(&mut bytes);
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_memory(caller, memory, ptr, &bytes)
}

pub(super) fn p1_record_kernel_profile<CpuImpl, HostFs>(
    store: &Preview1ProgramStore<CpuImpl, HostFs>,
    syscall: &'static str,
    started_ticks: u64,
) where
    CpuImpl: Cpu + Clone,
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

pub(super) fn p1_kernel_profile_start<CpuImpl, HostFs>(
    store: &Preview1ProgramStore<CpuImpl, HostFs>,
) -> Option<u64>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    store
        .runtime_state
        .profiling_enabled()
        .then(|| store.cpu.now().ticks())
}

pub(super) fn p1_record_optional_kernel_profile<CpuImpl, HostFs>(
    store: &Preview1ProgramStore<CpuImpl, HostFs>,
    syscall: &'static str,
    started_ticks: Option<u64>,
) where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if let Some(started_ticks) = started_ticks {
        p1_record_kernel_profile(store, syscall, started_ticks);
    }
}

pub(super) async fn p1_fd_write<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    nwritten: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let started = caller.data().cpu.now().ticks();
    let result = p1_fd_write_inner(caller, fd, iovs, iovs_len, nwritten).await;
    p1_record_kernel_profile(caller.data(), "fd_write", started);
    result
}

pub(super) async fn p1_fd_write_inner<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    nwritten: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    if let Some(Preview1Descriptor::File {
        descriptor,
        offset,
        fdflags,
    }) = caller.data().descriptors.get(fd)
    {
        let descriptor = descriptor.clone();
        if crate::guest_host_share_path(&descriptor.path).is_none() {
            let current_offset = *offset;
            let fdflags = *fdflags;
            let layout = match p1_read_iovs_with_byte_len(caller, memory, iovs, iovs_len) {
                Ok(layout) => layout,
                Err(errno) => return errno,
            };
            let byte_len = layout.byte_len;
            let written = match u32::try_from(byte_len) {
                Ok(written) => written,
                Err(_) => return p1::errno::OVERFLOW,
            };
            let ranges = match p1_iovs_memory_ranges(memory, &layout.iovs) {
                Ok(ranges) => ranges,
                Err(errno) => return errno,
            };
            let memory_base = memory.base as *const u8;
            let now_nanos = caller.data().now_nanos();
            let write_result = if fdflags & P1_FDFLAG_APPEND != 0 {
                caller.data_mut().filesystem.append_with(
                    &descriptor,
                    byte_len,
                    now_nanos,
                    |destination| {
                        copy_preview1_ranges_to_slice(memory_base, &ranges, destination);
                    },
                )
            } else {
                let write_offset: usize = match current_offset.try_into() {
                    Ok(offset) => offset,
                    Err(_) => return p1::errno::OVERFLOW,
                };
                caller.data_mut().filesystem.write_at_with(
                    &descriptor,
                    write_offset,
                    byte_len,
                    now_nanos,
                    |destination| {
                        copy_preview1_ranges_to_slice(memory_base, &ranges, destination);
                    },
                )
            };
            if let Err(error) = write_result {
                return p1_errno_from_fs(error);
            }
            let Some(Preview1Descriptor::File { offset, .. }) =
                caller.data_mut().descriptors.get_mut(fd)
            else {
                panic!("Preview1 descriptor disappeared during direct file write");
            };
            *offset = current_offset.saturating_add(byte_len as u64);
            return p1_write_u32(caller, memory, nwritten, written);
        }
    }
    let bytes = match p1_read_iovs_to_bytes(caller, memory, iovs, iovs_len) {
        Ok(bytes) => bytes,
        Err(errno) => return errno,
    };
    let written = match p1_write_descriptor(caller, fd, &bytes).await {
        Ok(written) => written,
        Err(errno) => return errno,
    };
    p1_write_u32(caller, memory, nwritten, written)
}

pub(super) async fn p1_fd_read<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    nread: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let started = caller.data().cpu.now().ticks();
    let result = p1_fd_read_inner(caller, fd, iovs, iovs_len, nread).await;
    p1_record_kernel_profile(caller.data(), "fd_read", started);
    result
}

pub(super) async fn p1_fd_read_inner<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    nread: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let layout = match p1_read_iovs_with_byte_len(caller, memory, iovs, iovs_len) {
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
            let bytes = match caller.data_mut().read_pipe(fd, capacity).await {
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
            let bytes = if let Some(host_path) =
                crate::guest_host_share_path(&descriptor.path).map(ToOwned::to_owned)
            {
                let service = match caller.data().filesystem.host_service() {
                    Ok(service) => service,
                    Err(error) => return p1_errno_from_fs(error),
                };
                let max_bytes = match u32::try_from(capacity) {
                    Ok(max_bytes) => max_bytes,
                    Err(_) => return p1::errno::OVERFLOW,
                };
                match service.read_file_range(&host_path, offset, max_bytes).await {
                    Ok(bytes) => Bytes::from(bytes),
                    Err(error) => {
                        return p1_errno_from_fs(crate::wasmtime_adapter::wasi::map_host_fs_error(
                            error,
                        ));
                    }
                }
            } else {
                match caller
                    .data()
                    .filesystem
                    .read_file_chunk(&descriptor, offset, capacity)
                {
                    Ok(bytes) => bytes,
                    Err(error) => return p1_errno_from_fs(error),
                }
            };
            if let Some(Preview1Descriptor::File { offset, .. }) =
                caller.data_mut().descriptors.get_mut(fd)
            {
                *offset = offset.saturating_add(bytes.len() as u64);
            }
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
        let status = p1_write_memory(caller, memory, ptr, &bytes[copied..copied + len]);
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

pub(super) fn p1_fd_prestat_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    buf: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
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
    p1_write_u8(caller, memory, buf, 0).max(p1_write_u32(caller, memory, buf + 4, len))
}

pub(super) fn p1_fd_prestat_dir_name<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    path: u32,
    len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
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
    preview1_write_memory(memory, path, bytes)
}

pub(super) fn p1_fdstat_bytes(filetype: u8, fdflags: u16, rights: u64) -> [u8; 24] {
    let mut bytes = [0; 24];
    bytes[0] = filetype;
    bytes[2..4].copy_from_slice(&fdflags.to_le_bytes());
    bytes[8..16].copy_from_slice(&rights.to_le_bytes());
    bytes[16..24].copy_from_slice(&rights.to_le_bytes());
    bytes
}

pub(super) fn p1_fd_fdstat_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    stat: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
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
    preview1_write_memory(memory, stat, &p1_fdstat_bytes(filetype, fdflags, rights))
}

pub(super) fn p1_fd_fdstat_set_flags<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    fdflags: u16,
) -> i32
where
    CpuImpl: Cpu + Clone,
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

pub(super) fn p1_fd_fdstat_set_rights<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    rights_base: u64,
    rights_inheriting: u64,
) -> i32
where
    CpuImpl: Cpu + Clone,
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

pub(super) async fn p1_fd_filestat_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    stat: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
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
    let (identity, stat_value) = match p1_stat_absolute_path(caller, &path).await {
        Ok(stat) => stat,
        Err(errno) => return errno,
    };
    p1_write_filestat(caller, stat, identity, stat_value)
}

/// Resolves a guest-absolute path to its object identity and descriptor stat.
///
/// Host-share paths take the async 9p route so `st_dev`/`st_ino`, link count,
/// and timestamps all come from the host's own `Rgetattr`; embedded paths read
/// the in-memory node, whose identity is allocated once per object.
pub(super) async fn p1_stat_absolute_path<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    path: &str,
) -> Result<(crate::ObjectIdentity, fs_types::DescriptorStat), i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if let Some(host_path) = crate::guest_host_share_path(path).map(ToOwned::to_owned) {
        let service = caller
            .data()
            .filesystem
            .host_service()
            .map_err(p1_errno_from_fs)?;
        let metadata = service.stat_path(&host_path).await.map_err(|error| {
            p1_errno_from_fs(crate::wasmtime_adapter::wasi::map_host_fs_error(error))
        })?;
        let stat = p1_descriptor_stat_from_host_metadata(&metadata);
        return Ok((metadata.identity, stat));
    }
    let filesystem = &caller.data().filesystem;
    let identity = filesystem
        .identity_at_path(path)
        .map_err(p1_errno_from_fs)?;
    let stat = filesystem.stat(path).map_err(p1_errno_from_fs)?;
    Ok((identity, stat))
}

pub(super) async fn p1_fd_filestat_set_size<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    size: u64,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(Preview1Descriptor::File { descriptor, .. }) = caller.data().descriptors.get(fd)
    else {
        return p1::errno::BADF;
    };
    let descriptor = descriptor.clone();
    if let Some(host_path) = crate::guest_host_share_path(&descriptor.path).map(ToOwned::to_owned) {
        if !descriptor.flags.contains(fs_types::DescriptorFlags::WRITE) {
            return p1::errno::NOTCAPABLE;
        }
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        return service
            .set_file_size(&host_path, size)
            .await
            .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
            .map_or_else(p1_errno_from_fs, |_| {
                caller
                    .data_mut()
                    .filesystem
                    .invalidate_host_subtree(&descriptor.path);
                p1::errno::SUCCESS
            });
    }
    let now_nanos = caller.data().now_nanos();
    caller
        .data_mut()
        .filesystem
        .set_size(&descriptor, size, now_nanos)
        .map_or_else(p1_errno_from_fs, |_| p1::errno::SUCCESS)
}

pub(super) async fn p1_fd_filestat_set_times<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    atim: u64,
    mtim: u64,
    fstflags: u16,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let (Some(Preview1Descriptor::File { descriptor, .. })
    | Some(Preview1Descriptor::Preopen { descriptor, .. })) = caller.data().descriptors.get(fd)
    else {
        return p1::errno::BADF;
    };
    let descriptor = descriptor.clone();
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
    if let Some(host_path) = crate::guest_host_share_path(&descriptor.path).map(ToOwned::to_owned) {
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        return service
            .set_times(&host_path, access, modified)
            .await
            .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
            .map_or_else(p1_errno_from_fs, |_| {
                caller
                    .data_mut()
                    .filesystem
                    .invalidate_host_subtree(&descriptor.path);
                p1::errno::SUCCESS
            });
    }
    caller
        .data_mut()
        .filesystem
        .set_times(&descriptor, access, modified, now_nanos)
        .map_or_else(p1_errno_from_fs, |_| p1::errno::SUCCESS)
}

pub(super) fn p1_fd_advise<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    caller
        .data()
        .descriptors
        .get(fd)
        .map_or(p1::errno::BADF, |_| p1::errno::SUCCESS)
}

pub(super) async fn p1_fd_allocate<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    offset: u64,
    len: u64,
) -> i32
where
    CpuImpl: Cpu + Clone,
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
    if let Some(host_path) = crate::guest_host_share_path(&descriptor.path).map(ToOwned::to_owned) {
        if !descriptor.flags.contains(fs_types::DescriptorFlags::WRITE) {
            return p1::errno::NOTCAPABLE;
        }
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        let current = match service.stat_path(&host_path).await {
            Ok(metadata) => metadata.size,
            Err(error) => {
                return p1_errno_from_fs(crate::wasmtime_adapter::wasi::map_host_fs_error(error));
            }
        };
        if end <= current {
            return p1::errno::SUCCESS;
        }
        return service
            .set_file_size(&host_path, end)
            .await
            .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
            .map_or_else(p1_errno_from_fs, |_| {
                caller
                    .data_mut()
                    .filesystem
                    .invalidate_host_subtree(&descriptor.path);
                p1::errno::SUCCESS
            });
    }
    let current = match caller.data().filesystem.stat(&descriptor.path) {
        Ok(stat) => stat.size,
        Err(error) => return p1_errno_from_fs(error),
    };
    if end <= current {
        return p1::errno::SUCCESS;
    }
    let now_nanos = caller.data().now_nanos();
    caller
        .data_mut()
        .filesystem
        .set_size(&descriptor, end, now_nanos)
        .map_or_else(p1_errno_from_fs, |_| p1::errno::SUCCESS)
}

pub(super) async fn p1_fd_datasync<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    p1_fd_sync_impl(caller, fd).await
}

pub(super) async fn p1_fd_sync<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    p1_fd_sync_impl(caller, fd).await
}

/// Backs `fd_sync` and `fd_datasync`.
///
/// A descriptor on the host share is flushed with a real 9p `Tfsync`; 9p has
/// no separate data-only barrier, so both entry points map to it. Descriptors
/// on the embedded filesystem, on stdio, and on `/dev/null` have no
/// write-back stage to flush, so success there is an accurate answer and not
/// a silent skip.
async fn p1_fd_sync_impl<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(descriptor) = caller.data().descriptors.get(fd) else {
        return p1::errno::BADF;
    };
    let host_path = p1_descriptor_path(Some(descriptor))
        .and_then(crate::guest_host_share_path)
        .map(ToOwned::to_owned);
    let Some(host_path) = host_path else {
        return p1::errno::SUCCESS;
    };
    let service = match caller.data().filesystem.host_service() {
        Ok(service) => service,
        Err(error) => return p1_errno_from_fs(error),
    };
    match service.sync_file(&host_path).await {
        Ok(()) => p1::errno::SUCCESS,
        Err(error) => p1_errno_from_fs(crate::wasmtime_adapter::wasi::map_host_fs_error(error)),
    }
}

pub(super) async fn p1_fd_pread<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    offset: u64,
    nread: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let layout = match p1_read_iovs_with_byte_len(caller, memory, iovs, iovs_len) {
        Ok(layout) => layout,
        Err(errno) => return errno,
    };
    let capacity = layout.byte_len;
    let Some(Preview1Descriptor::File { descriptor, .. }) = caller.data().descriptors.get(fd)
    else {
        return p1::errno::BADF;
    };
    let bytes = if let Some(host_path) =
        crate::guest_host_share_path(&descriptor.path).map(ToOwned::to_owned)
    {
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        let max_bytes = match u32::try_from(capacity) {
            Ok(max_bytes) => max_bytes,
            Err(_) => return p1::errno::OVERFLOW,
        };
        match service.read_file_range(&host_path, offset, max_bytes).await {
            Ok(bytes) => Bytes::from(bytes),
            Err(error) => {
                return p1_errno_from_fs(crate::wasmtime_adapter::wasi::map_host_fs_error(error));
            }
        }
    } else {
        match caller
            .data()
            .filesystem
            .read_file_chunk(descriptor, offset, capacity)
        {
            Ok(bytes) => bytes,
            Err(error) => return p1_errno_from_fs(error),
        }
    };
    p1_write_iovs_from_bytes(caller, memory, layout.iovs, &bytes, nread)
}

pub(super) async fn p1_fd_pwrite<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    offset: u64,
    nwritten: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let bytes = match p1_read_iovs_to_bytes(caller, memory, iovs, iovs_len) {
        Ok(bytes) => bytes,
        Err(errno) => return errno,
    };
    let Some(Preview1Descriptor::File { descriptor, .. }) = caller.data().descriptors.get(fd)
    else {
        return p1::errno::BADF;
    };
    let offset: usize = match offset.try_into() {
        Ok(offset) => offset,
        Err(_) => return p1::errno::OVERFLOW,
    };
    let descriptor = descriptor.clone();
    if let Some(host_path) = crate::guest_host_share_path(&descriptor.path).map(ToOwned::to_owned) {
        if !descriptor.flags.contains(fs_types::DescriptorFlags::WRITE) {
            return p1::errno::NOTCAPABLE;
        }
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        if let Err(error) = service.write_file(&host_path, offset as u64, &bytes).await {
            return p1_errno_from_fs(crate::wasmtime_adapter::wasi::map_host_fs_error(error));
        }
        caller
            .data_mut()
            .filesystem
            .invalidate_host_subtree(&descriptor.path);
        let written = match u32::try_from(bytes.len()) {
            Ok(written) => written,
            Err(_) => return p1::errno::OVERFLOW,
        };
        return p1_write_u32(caller, memory, nwritten, written);
    }
    let now_nanos = caller.data().now_nanos();
    if let Err(error) =
        caller
            .data_mut()
            .filesystem
            .write_at(&descriptor, offset, &bytes, now_nanos)
    {
        return p1_errno_from_fs(error);
    }
    let written = match u32::try_from(bytes.len()) {
        Ok(written) => written,
        Err(_) => return p1::errno::OVERFLOW,
    };
    p1_write_u32(caller, memory, nwritten, written)
}

pub(super) async fn p1_fd_readdir<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    buf: u32,
    buf_len: u32,
    cookie: u64,
    bufused: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(path) = p1_descriptor_path(caller.data().descriptors.get(fd)) else {
        return p1::errno::BADF;
    };
    if let Some(host_path) = crate::guest_host_share_path(path).map(ToOwned::to_owned) {
        let directory_path = path.to_owned();
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        let entries = match service.read_dir(&host_path).await {
            Ok(entries) => entries,
            Err(error) => {
                return p1_errno_from_fs(crate::wasmtime_adapter::wasi::map_host_fs_error(error));
            }
        };
        caller
            .data_mut()
            .filesystem
            .seed_host_directory_entries(&directory_path, entries);
        let entries = match caller.data().filesystem.read_directory(&directory_path) {
            Ok(entries) => entries,
            Err(error) => return p1_errno_from_fs(error),
        };
        return p1_fd_readdir_entries(caller, entries, buf, buf_len, cookie, bufused);
    }
    let entries = match caller.data().filesystem.read_directory(path) {
        Ok(entries) => entries,
        Err(error) => return p1_errno_from_fs(error),
    };
    p1_fd_readdir_entries(caller, entries, buf, buf_len, cookie, bufused)
}

pub(super) fn p1_fd_readdir_entries<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    entries: Vec<fs_types::DirectoryEntry>,
    buf: u32,
    buf_len: u32,
    cookie: u64,
    bufused: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
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
        let dirent_ptr = match buf.checked_add(used as u32) {
            Some(ptr) => ptr,
            None => return p1::errno::OVERFLOW,
        };
        let status = p1_write_u64(caller, memory, dirent_ptr, next)
            .max(p1_write_u64(caller, memory, dirent_ptr + 8, next))
            .max(p1_write_u32(
                caller,
                memory,
                dirent_ptr + 16,
                name.len().try_into().unwrap_or(u32::MAX),
            ))
            .max(p1_write_u8(
                caller,
                memory,
                dirent_ptr + 20,
                p1_filetype_from_descriptor_type(entry.type_.clone()),
            ));
        if status != p1::errno::SUCCESS {
            return status;
        }
        used += dirent_len;
        let remaining = capacity - used;
        let copied = remaining.min(name.len());
        let name_ptr = match buf.checked_add(used as u32) {
            Some(ptr) => ptr,
            None => return p1::errno::OVERFLOW,
        };
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

pub(super) fn p1_fd_renumber<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    from: i32,
    to: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    caller.data_mut().descriptors.renumber(from, to)
}

pub(super) async fn p1_fd_seek<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    offset: i64,
    whence: u8,
    new_offset: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
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
            if let Some(host_path) =
                crate::guest_host_share_path(&descriptor.path).map(ToOwned::to_owned)
            {
                let service = match caller.data().filesystem.host_service() {
                    Ok(service) => service,
                    Err(error) => return p1_errno_from_fs(error),
                };
                match service.stat_path(&host_path).await {
                    Ok(metadata) => metadata.size,
                    Err(error) => {
                        return p1_errno_from_fs(crate::wasmtime_adapter::wasi::map_host_fs_error(
                            error,
                        ));
                    }
                }
            } else {
                match caller.data().filesystem.stat(&descriptor.path) {
                    Ok(stat) => stat.size,
                    Err(error) => return p1_errno_from_fs(error),
                }
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
    p1_write_u64(caller, memory, new_offset, next)
}

pub(super) fn p1_fd_tell<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    offset_out: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
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
    p1_write_u64(caller, memory, offset_out, offset)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn p1_path_open<CpuImpl, HostFs>(
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
    let path = match p1_read_path(caller, memory, path, path_len) {
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
pub(super) async fn p1_path_open_resolved<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
            return p1_write_u32(caller, memory, opened_fd, fd);
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
    p1_write_u32(caller, memory, opened_fd, fd)
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

pub(super) async fn p1_open_descriptor_resolved<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    base: &FsDescriptor,
    path_flags: fs_types::PathFlags,
    path: &str,
    open_flags: fs_types::OpenFlags,
    descriptor_flags: fs_types::DescriptorFlags,
) -> Result<FsDescriptor, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if base.kind != FsNodeKind::Directory {
        return Err(p1::errno::NOTDIR);
    }
    let absolute =
        crate::resolve_child_path(&base.path, path).map_err(p1_errno_from_component_path)?;
    if let Some(host_path) = crate::guest_host_share_path(&absolute).map(ToOwned::to_owned) {
        return p1_open_host_descriptor_resolved(
            caller,
            base,
            absolute,
            host_path,
            open_flags,
            descriptor_flags,
        )
        .await;
    }
    let now_nanos = caller.data().now_nanos();
    caller
        .data_mut()
        .filesystem
        .open_at(
            base,
            path_flags,
            path,
            open_flags,
            descriptor_flags,
            now_nanos,
        )
        .map_err(p1_errno_from_fs)
}

pub(super) async fn p1_open_host_descriptor_resolved<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    base: &FsDescriptor,
    absolute: String,
    host_path: String,
    open_flags: fs_types::OpenFlags,
    descriptor_flags: fs_types::DescriptorFlags,
) -> Result<FsDescriptor, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let service = caller
        .data()
        .filesystem
        .host_service()
        .map_err(p1_errno_from_fs)?;
    let metadata = service.stat_path(&host_path).await;
    // Host files are never materialised in kernel memory on open: `fd_read`
    // and `fd_pread` pull bounded ranges over 9p on demand.
    let (kind, identity, descriptor_flags) = match metadata {
        Ok(metadata) => {
            let kind = crate::wasmtime_adapter::wasi::host_metadata_node_kind(&metadata);
            if open_flags.contains(fs_types::OpenFlags::EXCLUSIVE)
                && open_flags.contains(fs_types::OpenFlags::CREATE)
            {
                return Err(p1::errno::EXIST);
            }
            if open_flags.contains(fs_types::OpenFlags::DIRECTORY) && kind != FsNodeKind::Directory
            {
                return Err(p1::errno::NOTDIR);
            }
            if !open_flags.contains(fs_types::OpenFlags::DIRECTORY) && kind == FsNodeKind::Directory
            {
                return Err(p1::errno::ISDIR);
            }
            let descriptor_flags = crate::wasmtime_adapter::wasi::effective_open_descriptor_flags(
                base.flags,
                descriptor_flags,
                kind,
            )
            .map_err(p1_errno_from_fs)?;
            if open_flags.contains(fs_types::OpenFlags::TRUNCATE) {
                if kind != FsNodeKind::File {
                    return Err(p1::errno::ISDIR);
                }
                if !base
                    .flags
                    .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
                {
                    return Err(p1::errno::ROFS);
                }
                service
                    .truncate_file(&host_path)
                    .await
                    .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
                    .map_err(p1_errno_from_fs)?;
            }
            if kind != FsNodeKind::File {
                let entries = service
                    .read_dir(&host_path)
                    .await
                    .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
                    .map_err(p1_errno_from_fs)?;
                caller
                    .data_mut()
                    .filesystem
                    .seed_host_directory_entries(&absolute, entries);
            }
            (kind, metadata.identity, descriptor_flags)
        }
        Err(error) => {
            let error = crate::wasmtime_adapter::wasi::map_host_fs_error(error);
            if !matches!(error, fs_types::ErrorCode::NoEntry)
                || !open_flags.contains(fs_types::OpenFlags::CREATE)
            {
                return Err(p1_errno_from_fs(error));
            }
            if !base
                .flags
                .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
            {
                return Err(p1::errno::ROFS);
            }
            service
                .create_file(&host_path)
                .await
                .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
                .map_err(p1_errno_from_fs)?;
            let metadata = service
                .stat_path(&host_path)
                .await
                .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
                .map_err(p1_errno_from_fs)?;
            let descriptor_flags = crate::wasmtime_adapter::wasi::effective_open_descriptor_flags(
                base.flags,
                descriptor_flags,
                FsNodeKind::File,
            )
            .map_err(p1_errno_from_fs)?;
            (FsNodeKind::File, metadata.identity, descriptor_flags)
        }
    };
    Ok(FsDescriptor {
        path: absolute,
        kind,
        flags: descriptor_flags,
        identity: Some(identity),
    })
}

pub(super) async fn p1_path_create_directory<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
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
    let path = match p1_read_path(caller, memory, path, path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let (descriptor, path, absolute) = match caller.data().resolve_preview1_path(fd, &path) {
        Ok(resolved) => resolved,
        Err(errno) => return errno,
    };
    if let Some(host_path) = crate::guest_host_share_path(&absolute) {
        if descriptor.kind != FsNodeKind::Directory {
            return p1::errno::NOTDIR;
        }
        if !descriptor
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return p1::errno::ROFS;
        }
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        return service
            .create_directory(host_path)
            .await
            .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
            .map_or_else(p1_errno_from_fs, |_| {
                caller
                    .data_mut()
                    .filesystem
                    .invalidate_host_subtree(&absolute);
                p1::errno::SUCCESS
            });
    }
    let now_nanos = caller.data().now_nanos();
    caller
        .data_mut()
        .filesystem
        .create_directory_at(&descriptor, &path, now_nanos)
        .map_or_else(p1_errno_from_fs, |_| p1::errno::SUCCESS)
}

pub(super) async fn p1_path_filestat_get<CpuImpl, HostFs>(
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
    let (_, _, absolute) = match caller.data().resolve_preview1_path(fd, &path) {
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
    let (identity, stat_value) = match p1_stat_absolute_path(caller, &absolute).await {
        Ok(stat) => stat,
        Err(errno) => return errno,
    };
    p1_write_filestat(caller, stat, identity, stat_value)
}

#[expect(
    clippy::too_many_arguments,
    reason = "the parameter list is the guest ABI of this call, so grouping it would hide the contract and break the one-to-one match with the linker registration"
)]
pub(super) async fn p1_path_filestat_set_times<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let path = match p1_read_path(caller, memory, path, path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let (_, _, absolute) = match caller.data().resolve_preview1_path(fd, &path) {
        Ok(resolved) => resolved,
        Err(errno) => return errno,
    };
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
    if let Some(host_path) = crate::guest_host_share_path(&absolute).map(ToOwned::to_owned) {
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        return service
            .set_times(&host_path, access, modified)
            .await
            .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
            .map_or_else(p1_errno_from_fs, |_| {
                caller
                    .data_mut()
                    .filesystem
                    .invalidate_host_subtree(&absolute);
                p1::errno::SUCCESS
            });
    }
    caller
        .data_mut()
        .filesystem
        .set_times_at_path(&absolute, access, modified, now_nanos)
        .map_or_else(p1_errno_from_fs, |_| p1::errno::SUCCESS)
}

#[expect(
    clippy::too_many_arguments,
    reason = "the parameter list is the guest ABI of this call, so grouping it would hide the contract and break the one-to-one match with the linker registration"
)]
pub(super) async fn p1_path_link<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
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
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let old_path = match p1_read_path(caller, memory, old_path, old_path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let new_path = match p1_read_path(caller, memory, new_path, new_path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let status = caller.data().require_hardlink_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let (source_base, old_path, source_absolute) =
        match caller.data().resolve_preview1_path(old_fd, &old_path) {
            Ok(resolved) => resolved,
            Err(errno) => return errno,
        };
    let (destination_base, new_path, destination_absolute) =
        match caller.data().resolve_preview1_path(new_fd, &new_path) {
            Ok(resolved) => resolved,
            Err(errno) => return errno,
        };
    if source_base.kind != FsNodeKind::Directory || destination_base.kind != FsNodeKind::Directory {
        return p1::errno::NOTDIR;
    }
    let source_host = crate::guest_host_share_path(&source_absolute);
    let destination_host = crate::guest_host_share_path(&destination_absolute);
    if source_host.is_some() || destination_host.is_some() {
        if !destination_base
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return p1::errno::ROFS;
        }
        let Some(source_host) = source_host else {
            return p1::errno::XDEV;
        };
        let Some(destination_host) = destination_host else {
            return p1::errno::XDEV;
        };
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        return service
            .hard_link(source_host, destination_host)
            .await
            .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
            .map_or_else(p1_errno_from_fs, |_| {
                caller
                    .data_mut()
                    .filesystem
                    .invalidate_host_subtree(&destination_absolute);
                p1::errno::SUCCESS
            });
    }
    let now_nanos = caller.data().now_nanos();
    caller
        .data_mut()
        .filesystem
        .link_at(
            &source_base,
            &old_path,
            &destination_base,
            &new_path,
            now_nanos,
        )
        .map_or_else(p1_errno_from_fs, |_| p1::errno::SUCCESS)
}

pub(super) async fn p1_path_readlink<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    path: u32,
    path_len: u32,
    buf: u32,
    buf_len: u32,
    bufused: u32,
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
    let status = caller.data().require_symlink_read_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let (base, path, absolute) = match caller.data().resolve_preview1_path(fd, &path) {
        Ok(resolved) => resolved,
        Err(errno) => return errno,
    };
    let payload = if let Some(host_path) = crate::guest_host_share_path(&absolute) {
        if base.kind != FsNodeKind::Directory {
            return p1::errno::NOTDIR;
        }
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        let payload = match service.read_link(host_path).await {
            Ok(payload) => payload,
            Err(error) => {
                return p1_errno_from_fs(crate::wasmtime_adapter::wasi::map_host_fs_error(error));
            }
        };
        if let Err(error) =
            crate::wasmtime_adapter::wasi::resolve_symlink_payload(&absolute, &payload)
        {
            return p1_errno_from_fs(error);
        }
        payload
    } else {
        match caller.data().filesystem.readlink_at(&base, &path) {
            Ok(payload) => payload,
            Err(error) => return p1_errno_from_fs(error),
        }
    };
    let bytes = payload.as_bytes();
    let copied = (buf_len as usize).min(bytes.len());
    let status = p1_write_memory(caller, memory, buf, &bytes[..copied]);
    if status != p1::errno::SUCCESS {
        return status;
    }
    let copied = match u32::try_from(copied) {
        Ok(copied) => copied,
        Err(_) => return p1::errno::OVERFLOW,
    };
    p1_write_u32(caller, memory, bufused, copied)
}

pub(super) async fn p1_path_remove_directory<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
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
    let path = match p1_read_path(caller, memory, path, path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let (base, path, absolute) = match caller.data().resolve_preview1_path(fd, &path) {
        Ok(resolved) => resolved,
        Err(errno) => return errno,
    };
    if let Some(host_path) = crate::guest_host_share_path(&absolute) {
        if !base
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return p1::errno::ROFS;
        }
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        return service
            .remove(host_path, true)
            .await
            .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
            .map_or_else(p1_errno_from_fs, |_| {
                caller
                    .data_mut()
                    .filesystem
                    .invalidate_host_subtree(&absolute);
                p1::errno::SUCCESS
            });
    }
    caller
        .data_mut()
        .filesystem
        .remove_directory_at(&base, &path)
        .map_or_else(p1_errno_from_fs, |_| p1::errno::SUCCESS)
}

pub(super) async fn p1_path_rename<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    old_fd: i32,
    old_path: u32,
    old_path_len: u32,
    new_fd: i32,
    new_path: u32,
    new_path_len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let old_path = match p1_read_path(caller, memory, old_path, old_path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let new_path = match p1_read_path(caller, memory, new_path, new_path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let (source_base, old_path, source_absolute) =
        match caller.data().resolve_preview1_path(old_fd, &old_path) {
            Ok(resolved) => resolved,
            Err(errno) => return errno,
        };
    let (destination_base, new_path, destination_absolute) =
        match caller.data().resolve_preview1_path(new_fd, &new_path) {
            Ok(resolved) => resolved,
            Err(errno) => return errno,
        };
    if source_base.kind != FsNodeKind::Directory || destination_base.kind != FsNodeKind::Directory {
        return p1::errno::NOTDIR;
    }
    let source_host = crate::guest_host_share_path(&source_absolute).map(ToOwned::to_owned);
    let destination_host =
        crate::guest_host_share_path(&destination_absolute).map(ToOwned::to_owned);
    if source_host.is_some() || destination_host.is_some() {
        if !source_base
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
            || !destination_base
                .flags
                .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return p1::errno::ROFS;
        }
        let Some(source_host) = source_host else {
            return p1::errno::XDEV;
        };
        let Some(destination_host) = destination_host else {
            return p1::errno::XDEV;
        };
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        return service
            .rename(&source_host, &destination_host)
            .await
            .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
            .map_or_else(p1_errno_from_fs, |_| {
                let filesystem = &mut caller.data_mut().filesystem;
                filesystem.invalidate_host_subtree(&source_absolute);
                filesystem.invalidate_host_subtree(&destination_absolute);
                p1::errno::SUCCESS
            });
    }
    let now_nanos = caller.data().now_nanos();
    caller
        .data_mut()
        .filesystem
        .rename_at(
            &source_base,
            &old_path,
            &destination_base,
            &new_path,
            now_nanos,
        )
        .map_or_else(p1_errno_from_fs, |_| p1::errno::SUCCESS)
}

pub(super) async fn p1_path_symlink<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    old_path: u32,
    old_path_len: u32,
    fd: i32,
    new_path: u32,
    new_path_len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let old_path = match p1_read_path(caller, memory, old_path, old_path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let new_path = match p1_read_path(caller, memory, new_path, new_path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let status = caller.data().require_symlink_create_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let (base, new_path, absolute) = match caller.data().resolve_preview1_path(fd, &new_path) {
        Ok(resolved) => resolved,
        Err(errno) => return errno,
    };
    if let Err(error) = crate::wasmtime_adapter::wasi::resolve_symlink_payload(&absolute, &old_path)
    {
        return p1_errno_from_fs(error);
    }
    if let Some(host_path) = crate::guest_host_share_path(&absolute) {
        if base.kind != FsNodeKind::Directory {
            return p1::errno::NOTDIR;
        }
        if !base
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return p1::errno::ROFS;
        }
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        return service
            .symlink(&old_path, host_path)
            .await
            .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
            .map_or_else(p1_errno_from_fs, |_| {
                caller
                    .data_mut()
                    .filesystem
                    .invalidate_host_subtree(&absolute);
                p1::errno::SUCCESS
            });
    }
    let now_nanos = caller.data().now_nanos();
    caller
        .data_mut()
        .filesystem
        .symlink_at(&base, &new_path, &old_path, now_nanos)
        .map_or_else(p1_errno_from_fs, |_| p1::errno::SUCCESS)
}

pub(super) async fn p1_path_unlink_file<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
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
    let path = match p1_read_path(caller, memory, path, path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let (base, path, absolute) = match caller.data().resolve_preview1_path(fd, &path) {
        Ok(resolved) => resolved,
        Err(errno) => return errno,
    };
    if let Some(host_path) = crate::guest_host_share_path(&absolute) {
        if !base
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return p1::errno::ROFS;
        }
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        return service
            .remove(host_path, false)
            .await
            .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
            .map_or_else(p1_errno_from_fs, |_| {
                caller
                    .data_mut()
                    .filesystem
                    .invalidate_host_subtree(&absolute);
                p1::errno::SUCCESS
            });
    }
    caller
        .data_mut()
        .filesystem
        .unlink_file_at(&base, &path)
        .map_or_else(p1_errno_from_fs, |_| p1::errno::SUCCESS)
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
pub(super) async fn p1_poll_oneoff<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    subscriptions: u32,
    events: u32,
    nsubscriptions: u32,
    nevents: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if nsubscriptions == 0 {
        return p1::errno::INVAL;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };

    let parsed = match p1_read_subscriptions(caller, memory, subscriptions, nsubscriptions) {
        Ok(parsed) => parsed,
        Err(errno) => return errno,
    };

    let ready = loop {
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

        // Nothing is ready and no deadline has expired: register on every
        // subscribed descriptor and sleep until one of them makes progress
        // or the earliest deadline arrives.
        let mut wait = P1WaitSet::new();
        for subscription in &parsed {
            if let P1Subscription::Fd { fd, event_type, .. } = subscription {
                p1_add_wait_target(caller.data(), *fd, *event_type, &mut wait);
            }
        }
        let timer = caller.data().timer();
        p1_wait_step(&timer, &mut wait, earliest).await;
    };

    let mut event_count = 0u32;
    for event in &ready {
        let Some(event_ptr) = event_count
            .checked_mul(P1_EVENT_SIZE)
            .and_then(|offset| events.checked_add(offset))
        else {
            return p1::errno::OVERFLOW;
        };
        let status = p1_write_u64(caller, memory, event_ptr, event.userdata)
            .max(p1_write_u16(caller, memory, event_ptr + 8, event.error))
            .max(p1_write_u8(
                caller,
                memory,
                event_ptr + 10,
                event.event_type,
            ))
            .max(p1_write_u64(caller, memory, event_ptr + 16, event.nbytes))
            .max(p1_write_u16(caller, memory, event_ptr + 24, event.fd_flags));
        if status != p1::errno::SUCCESS {
            return status;
        }
        event_count += 1;
    }
    p1_write_u32(caller, memory, nevents, event_count)
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

fn p1_read_subscriptions<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    memory: Preview1Memory,
    subscriptions: u32,
    nsubscriptions: u32,
) -> Result<Vec<P1Subscription>, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let mut parsed = Vec::with_capacity(nsubscriptions as usize);
    for index in 0..nsubscriptions {
        let subscription_ptr = index
            .checked_mul(P1_SUBSCRIPTION_SIZE)
            .and_then(|offset| subscriptions.checked_add(offset))
            .ok_or(p1::errno::OVERFLOW)?;
        let userdata =
            p1_try_read_u64(caller, memory, subscription_ptr).map_err(|_| p1::errno::FAULT)?;
        let event_type =
            p1_try_read_u8(caller, memory, subscription_ptr + 8).map_err(|_| p1::errno::FAULT)?;
        parsed.push(match event_type {
            P1_EVENTTYPE_CLOCK => {
                let clock_id = p1_try_read_u32(caller, memory, subscription_ptr + 16)
                    .map_err(|_| p1::errno::FAULT)?;
                let timeout = p1_try_read_u64(caller, memory, subscription_ptr + 24)
                    .map_err(|_| p1::errno::FAULT)?;
                let flags = p1_try_read_u16(caller, memory, subscription_ptr + 40)
                    .map_err(|_| p1::errno::FAULT)?;
                if !matches!(clock_id, 0 | 1) {
                    P1Subscription::Failed {
                        userdata,
                        event_type,
                        error: p1::errno::INVAL as u16,
                    }
                } else {
                    let monotonic = clock_id == 1;
                    let now = if monotonic {
                        caller.data().now_nanos()
                    } else {
                        caller.data().system_time_nanos()
                    };
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
                let fd = p1_try_read_u32(caller, memory, subscription_ptr + 16)
                    .map_err(|_| p1::errno::FAULT)? as i32;
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

pub(super) fn p1_proc_raise<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    signal: u32,
) -> wasmtime::Result<i32>
where
    CpuImpl: Cpu + Clone,
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

pub(super) async fn p1_sock_accept<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    fdflags: i32,
    fd_out: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
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
        ))) => (*listener, *family),
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
            stream: accepted.stream,
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
    let status = p1_write_u32(caller, memory, fd_out, accepted_fd);
    if status != p1::errno::SUCCESS {
        let _ = caller.data_mut().descriptors.close(accepted_fd as i32);
        return status;
    }
    p1::errno::SUCCESS
}

pub(super) fn p1_connected_tcp_stream<CpuImpl, HostFs>(
    caller: &Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
) -> Result<u64, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected { stream, .. },
        ))) => Ok(*stream),
        Some(Preview1Descriptor::Socket(_)) => Err(p1::errno::INVAL),
        Some(_) => Err(p1::errno::NOTSOCK),
        None => Err(p1::errno::BADF),
    }
}

pub(super) async fn p1_sock_recv<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    ri_data: u32,
    ri_data_len: u32,
    _ri_flags: u16,
    ro_datalen: u32,
    ro_flags: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
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

pub(super) async fn p1_sock_recv_inner<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    ri_data: u32,
    ri_data_len: u32,
    _ri_flags: u16,
    ro_datalen: u32,
    ro_flags: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let iovs_started = p1_kernel_profile_start(caller.data());
    let layout = match p1_read_iovs_with_byte_len(caller, memory, ri_data, ri_data_len) {
        Ok(layout) => layout,
        Err(errno) => return errno,
    };
    p1_record_optional_kernel_profile(caller.data(), "sock_recv_iovs", iovs_started);
    let capacity = match layout.byte_len_u32() {
        Ok(capacity) => capacity,
        Err(errno) => return errno,
    };
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
        ))) => *stream,
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
    let bytes = match service
        .tcp_read_into_registered(stream, buffer, timeout)
        .await
    {
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

pub(super) async fn p1_sock_send<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    si_data: u32,
    si_data_len: u32,
    _si_flags: u16,
    so_datalen: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
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

pub(super) async fn p1_sock_send_inner<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    si_data: u32,
    si_data_len: u32,
    _si_flags: u16,
    so_datalen: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let bytes = match p1_read_iovs_to_bytes(caller, memory, si_data, si_data_len) {
        Ok(bytes) => bytes,
        Err(errno) => return errno,
    };
    let descriptor = caller.data().descriptors.get(fd).cloned();
    if let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { writer, .. })) = descriptor
    {
        let written = match u32::try_from(bytes.len()) {
            Ok(written) => written,
            Err(_) => return p1::errno::OVERFLOW,
        };
        let fdflags = match caller.data().descriptors.fdflags(fd) {
            Ok(fdflags) => fdflags,
            Err(errno) => return errno,
        };
        if let Err(errno) = p1_send_to_socketpair(&writer, bytes, fdflags).await {
            return errno;
        }
        return p1_write_u32(caller, memory, so_datalen, written);
    }
    let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(WasixTcpSocket::Connected {
        stream,
        ..
    }))) = descriptor
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
    let written = match u32::try_from(bytes.len()) {
        Ok(written) => written,
        Err(_) => return p1::errno::OVERFLOW,
    };
    if let Err(error) = service
        .tcp_write_all_bytes(stream, Bytes::from(bytes), timeout)
        .await
    {
        return p1_errno_from_tcp_error_for_fdflags(error, fdflags);
    }
    p1_write_u32(caller, memory, so_datalen, written)
}

pub(super) async fn p1_sock_shutdown<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    how: u8,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if how == 0 || how & !P1_SDFLAGS_SUPPORTED != 0 {
        return p1::errno::INVAL;
    }
    let descriptor = caller.data().descriptors.get(fd).cloned();
    match descriptor {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected { stream, .. },
        ))) => {
            let status = caller.data().require_tcp_authority();
            if status != p1::errno::SUCCESS {
                return status;
            }
            let Some(service) = caller.data().runtime_state.network_service() else {
                return p1::errno::NETDOWN;
            };
            service.tcp_close(stream).await;
            let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(slot))) =
                caller.data_mut().descriptors.get_mut(fd)
            else {
                return p1::errno::BADF;
            };
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
            socket,
            ..
        }))) => {
            let status = caller.data().require_udp_authority();
            if status != p1::errno::SUCCESS {
                return status;
            }
            let Some(service) = caller.data().runtime_state.network_service() else {
                return p1::errno::NETDOWN;
            };
            service.udp_close(socket).await;
            let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(slot))) =
                caller.data_mut().descriptors.get_mut(fd)
            else {
                return p1::errno::BADF;
            };
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

pub(super) fn p1_environment_strings<CpuImpl, HostFs>(
    store: &Preview1ProgramStore<CpuImpl, HostFs>,
) -> Vec<String>
where
    CpuImpl: Cpu + Clone,
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
async fn p1_write_stdio<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    stream: crate::ComponentOutputStreamKind,
    bytes: &[u8],
    nonblocking: bool,
) -> Result<u32, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let written = u32::try_from(bytes.len()).map_err(|_| p1::errno::OVERFLOW)?;
    let Some(writer) = caller.data().route_output(stream, bytes) else {
        return Ok(written);
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

pub(super) async fn p1_write_descriptor<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    bytes: &[u8],
) -> Result<u32, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    // A descriptor whose sink is a bounded channel blocks the guest while
    // that channel is full, unless the guest asked for non-blocking IO —
    // then it gets `EAGAIN` and keeps its bytes, as POSIX requires.
    let nonblocking = caller
        .data()
        .descriptors
        .fdflags(fd)
        .is_ok_and(p1_fdflags_nonblocking);
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
        Some(Preview1Descriptor::File { .. }) => {
            let Some(Preview1Descriptor::File {
                descriptor,
                offset,
                fdflags,
            }) = caller.data().descriptors.get(fd)
            else {
                return Err(p1::errno::BADF);
            };
            let current_offset = *offset;
            let descriptor = descriptor.clone();
            let next_offset = current_offset.saturating_add(bytes.len() as u64);
            if let Some(host_path) =
                crate::guest_host_share_path(&descriptor.path).map(ToOwned::to_owned)
            {
                if !descriptor.flags.contains(fs_types::DescriptorFlags::WRITE) {
                    return Err(p1::errno::NOTCAPABLE);
                }
                let service = caller
                    .data()
                    .filesystem
                    .host_service()
                    .map_err(p1_errno_from_fs)?;
                let host_offset = if fdflags & P1_FDFLAG_APPEND != 0 {
                    service
                        .stat_path(&host_path)
                        .await
                        .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
                        .map_err(p1_errno_from_fs)?
                        .size
                } else {
                    current_offset
                };
                service
                    .write_file(&host_path, host_offset, bytes)
                    .await
                    .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
                    .map_err(p1_errno_from_fs)?;
                caller
                    .data_mut()
                    .filesystem
                    .invalidate_host_subtree(&descriptor.path);
                let Some(Preview1Descriptor::File { offset, .. }) =
                    caller.data_mut().descriptors.get_mut(fd)
                else {
                    panic!("Preview1 descriptor disappeared during host write");
                };
                *offset = next_offset;
                return u32::try_from(bytes.len()).map_err(|_| p1::errno::OVERFLOW);
            }
            let now_nanos = caller.data().now_nanos();
            let write_offset: usize = current_offset.try_into().map_err(|_| p1::errno::OVERFLOW)?;
            if fdflags & P1_FDFLAG_APPEND != 0 {
                caller
                    .data_mut()
                    .filesystem
                    .append(&descriptor, bytes, now_nanos)
                    .map_err(p1_errno_from_fs)?;
            } else {
                caller
                    .data_mut()
                    .filesystem
                    .write_at(&descriptor, write_offset, bytes, now_nanos)
                    .map_err(p1_errno_from_fs)?;
            }
            let Some(Preview1Descriptor::File { offset, .. }) =
                caller.data_mut().descriptors.get_mut(fd)
            else {
                panic!("Preview1 descriptor disappeared during write");
            };
            *offset = next_offset;
            u32::try_from(bytes.len()).map_err(|_| p1::errno::OVERFLOW)
        }
        Some(_) => Err(p1::errno::BADF),
        None => Err(p1::errno::BADF),
    }
}

pub(super) async fn p1_read_descriptor<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    capacity: usize,
) -> Result<Bytes, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Stdin { .. }) => Ok(caller.data_mut().read_stdin(capacity).await),
        Some(Preview1Descriptor::PipeRead { .. }) => {
            caller.data_mut().read_pipe(fd, capacity).await
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
            let bytes = if let Some(host_path) =
                crate::guest_host_share_path(&descriptor.path).map(ToOwned::to_owned)
            {
                let service = caller
                    .data()
                    .filesystem
                    .host_service()
                    .map_err(p1_errno_from_fs)?;
                let max_bytes = u32::try_from(capacity).map_err(|_| p1::errno::OVERFLOW)?;
                service
                    .read_file_range(&host_path, offset, max_bytes)
                    .await
                    .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
                    .map_err(p1_errno_from_fs)?
                    .into()
            } else {
                caller
                    .data()
                    .filesystem
                    .read_file_chunk(&descriptor, offset, capacity)
                    .map_err(p1_errno_from_fs)?
            };
            if let Some(Preview1Descriptor::File { offset, .. }) =
                caller.data_mut().descriptors.get_mut(fd)
            {
                *offset = offset.saturating_add(bytes.len() as u64);
            }
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
pub(super) fn p1_write_filestat<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    stat: u32,
    identity: crate::ObjectIdentity,
    value: fs_types::DescriptorStat,
) -> i32
where
    CpuImpl: Cpu + Clone,
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
        .max(p1_write_u64(caller, memory, stat + 8, identity.local()))
        .max(p1_write_u8(
            caller,
            memory,
            stat + 16,
            p1_filetype_from_descriptor_type(value.type_),
        ))
        .max(p1_write_u64(caller, memory, stat + 24, value.link_count))
        .max(p1_write_u64(caller, memory, stat + 32, value.size))
        .max(p1_write_u64(caller, memory, stat + 40, atim))
        .max(p1_write_u64(caller, memory, stat + 48, mtim))
        .max(p1_write_u64(caller, memory, stat + 56, ctim))
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

pub(super) fn p1_errno_from_fs(error: fs_types::ErrorCode) -> i32 {
    match error {
        fs_types::ErrorCode::Access => p1::errno::ACCES,
        fs_types::ErrorCode::Already => p1::errno::EXIST,
        fs_types::ErrorCode::Invalid => p1::errno::INVAL,
        fs_types::ErrorCode::Io => p1::errno::IO,
        fs_types::ErrorCode::IsDirectory => p1::errno::ISDIR,
        fs_types::ErrorCode::Loop => p1::errno::LOOP,
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

    fn regular_file() -> Preview1Descriptor {
        Preview1Descriptor::File {
            descriptor: FsDescriptor {
                path: "/data".into(),
                kind: FsNodeKind::File,
                flags: fs_types::DescriptorFlags::READ,
                identity: None,
            },
            offset: 0,
            fdflags: 0,
        }
    }

    fn connected_socket() -> Preview1Descriptor {
        Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(WasixTcpSocket::Connected {
            family: WasixSocketFamily::Ipv4,
            stream: 9,
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
}
