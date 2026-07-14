use super::*;

pub(crate) fn random_len(len: u64) -> Result<usize> {
    usize::try_from(len).map_err(|_| wasmtime::Error::new(WasiAdapterTrap::RandomLengthOverflow))
}

pub(crate) fn entropy_error(error: crate::EntropyError) -> wasmtime::Error {
    wasmtime::Error::new(error)
}

pub struct TerminalInput;
pub struct TerminalOutput;
impl<CpuImpl, HostFs> wasi::clocks::monotonic_clock::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn now(&mut self) -> Result<wasi::clocks::monotonic_clock::Mark> {
        Ok(self.now_nanos())
    }

    fn get_resolution(&mut self) -> Result<wasi::clocks::types::Duration> {
        Ok(1)
    }
}

impl<CpuImpl, HostFs, U> wasi::clocks::monotonic_clock::HostWithStore<U>
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn wait_until(
        accessor: &Accessor<U, Self>,
        when: wasi::clocks::monotonic_clock::Mark,
    ) -> Result<()> {
        let (timer, cpu, runtime_state) = accessor.with(|mut access| {
            let store = access.get();
            (
                store.timer(),
                store.cpu.clone(),
                store.runtime_state.clone(),
            )
        });
        crate::wait_until_runtime_deadline(timer, cpu, runtime_state, when).await;
        Ok(())
    }

    async fn wait_for(
        accessor: &Accessor<U, Self>,
        duration: wasi::clocks::types::Duration,
    ) -> Result<()> {
        let deadline =
            accessor.with(|mut access| access.get().now_nanos().saturating_add(duration));
        Self::wait_until(accessor, deadline).await
    }
}

impl<CpuImpl, HostFs> wasi::clocks::system_clock::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn now(&mut self) -> Result<wasi::clocks::system_clock::Instant> {
        Ok(system_time_from_nanos(self.system_time_nanos()))
    }

    fn get_resolution(&mut self) -> Result<wasi::clocks::types::Duration> {
        Ok(1)
    }
}

impl<CpuImpl, HostFs> wasi::cli::environment::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_arguments(&mut self) -> Result<Vec<String>> {
        Ok(self.arguments().to_vec())
    }

    fn get_environment(&mut self) -> Result<Vec<(String, String)>> {
        Ok(self.environment().to_vec())
    }

    fn get_initial_cwd(&mut self) -> Result<Option<String>> {
        Ok(self
            .process_authority()
            .cwd()
            .map(|cwd| cwd.guest_name().to_owned()))
    }
}

impl<CpuImpl, HostFs> wasi::cli::exit::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn exit(&mut self, status: core::result::Result<(), ()>) -> Result<()> {
        let code = match status {
            Ok(()) => 0,
            Err(()) => 1,
        };
        self.request_exit(code);
        Err(wasmtime::Error::new(Preview3GuestExit::new(u32::from(
            code,
        ))))
    }

    fn exit_with_code(&mut self, status_code: u8) -> Result<()> {
        self.request_exit(status_code);
        Err(wasmtime::Error::new(Preview3GuestExit::new(u32::from(
            status_code,
        ))))
    }
}

impl<CpuImpl, HostFs> wasi::cli::stdin::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs, U> wasi::cli::stdin::HostWithStore<U> for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn read_via_stream(
        mut access: Access<'_, U, Self>,
    ) -> Result<(
        StreamReader<u8>,
        FutureReader<core::result::Result<(), cli_types::ErrorCode>>,
    )> {
        use crate::ComponentOutputMode;
        // For spawn-mode programs, drain the parent-provided stdin reader
        // one chunk at a time. For Serial/Trace programs, hand back an
        // immediately empty stream (serial stdin is polled through the
        // dedicated `helios:system/serial` interface instead).
        let stream = match access.get().output_mode() {
            ComponentOutputMode::Child { stdin_rx, .. }
            | ComponentOutputMode::RoutedChild { stdin_rx, .. } => {
                let reader = stdin_rx.clone();
                StreamReader::new(&mut access, ChannelStreamProducer::new(reader))?
            }
            ComponentOutputMode::Serial | ComponentOutputMode::Trace => {
                StreamReader::new(&mut access, Vec::<u8>::new())?
            }
        };
        let future = FutureReader::new(&mut access, async {
            Ok::<_, wasmtime::Error>(Ok::<(), cli_types::ErrorCode>(()))
        })
        .map_err(FsError::trap)?;
        Ok((stream, future))
    }
}

impl<CpuImpl, HostFs> wasi::cli::stdout::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs, U> wasi::cli::stdout::HostWithStore<U> for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn write_via_stream(
        mut access: Access<'_, U, Self>,
        data: StreamReader<u8>,
    ) -> Result<FutureReader<core::result::Result<(), cli_types::ErrorCode>>> {
        let (tx, rx) = oneshot::channel();
        let getter = access.getter();
        data.pipe(
            &mut access,
            SerialStreamConsumer::new(getter, tx, OutputStreamKind::Stdout),
        )?;
        FutureReader::new(&mut access, async move {
            match rx.await {
                Ok(result) => Ok::<_, wasmtime::Error>(result),
                Err(_) => Ok::<_, wasmtime::Error>(Ok::<(), cli_types::ErrorCode>(())),
            }
        })
    }
}

impl<CpuImpl, HostFs> wasi::cli::stderr::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs, U> wasi::cli::stderr::HostWithStore<U> for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn write_via_stream(
        mut access: Access<'_, U, Self>,
        data: StreamReader<u8>,
    ) -> Result<FutureReader<core::result::Result<(), cli_types::ErrorCode>>> {
        let (tx, rx) = oneshot::channel();
        let getter = access.getter();
        data.pipe(
            &mut access,
            SerialStreamConsumer::new(getter, tx, OutputStreamKind::Stderr),
        )?;
        FutureReader::new(&mut access, async move {
            match rx.await {
                Ok(result) => Ok::<_, wasmtime::Error>(result),
                Err(_) => Ok::<_, wasmtime::Error>(Ok::<(), cli_types::ErrorCode>(())),
            }
        })
    }
}

impl<CpuImpl, HostFs> wasi::cli::terminal_input::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}
impl<CpuImpl, HostFs> wasi::cli::terminal_input::HostTerminalInput for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self, resource: Resource<TerminalInput>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl<CpuImpl, HostFs> wasi::cli::terminal_output::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}
impl<CpuImpl, HostFs> wasi::cli::terminal_output::HostTerminalOutput for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self, resource: Resource<TerminalOutput>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl<CpuImpl, HostFs> wasi::cli::terminal_stdin::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_terminal_stdin(&mut self) -> Result<Option<Resource<TerminalInput>>> {
        Ok(None)
    }
}

impl<CpuImpl, HostFs> wasi::cli::terminal_stdout::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_terminal_stdout(&mut self) -> Result<Option<Resource<TerminalOutput>>> {
        Ok(None)
    }
}

impl<CpuImpl, HostFs> wasi::cli::terminal_stderr::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_terminal_stderr(&mut self) -> Result<Option<Resource<TerminalOutput>>> {
        Ok(None)
    }
}

impl<CpuImpl, HostFs> wasi::random::random::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_random_bytes(&mut self, len: u64) -> Result<Vec<u8>> {
        let len = random_len(len)?;
        let mut bytes = vec![0_u8; len];
        self.fill_secure_random(&mut bytes).map_err(entropy_error)?;
        Ok(bytes)
    }

    fn get_random_u64(&mut self) -> Result<u64> {
        self.secure_random_u64().map_err(entropy_error)
    }
}

impl<CpuImpl, HostFs> wasi::random::insecure::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_insecure_random_bytes(&mut self, len: u64) -> Result<Vec<u8>> {
        Ok(self.insecure_random_bytes(random_len(len)?))
    }

    fn get_insecure_random_u64(&mut self) -> Result<u64> {
        Ok(self.insecure_random_u64())
    }
}

impl<CpuImpl, HostFs> wasi::random::insecure_seed::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_insecure_seed(&mut self) -> Result<(u64, u64)> {
        Ok(self.insecure_seed())
    }
}

pub(super) fn system_time_from_nanos(nanos: u64) -> wasi::clocks::system_clock::Instant {
    let seconds = nanos / 1_000_000_000;
    let nanoseconds = (nanos % 1_000_000_000) as u32;
    wasi::clocks::system_clock::Instant {
        seconds: seconds
            .try_into()
            .expect("debugger wall clock exceeded wasi system clock range"),
        nanoseconds,
    }
}

pub(super) fn p3_new_timestamp_nanos(
    timestamp: fs_types::NewTimestamp,
    now_nanos: u64,
) -> core::result::Result<Option<u64>, fs_types::ErrorCode> {
    match timestamp {
        fs_types::NewTimestamp::NoChange => Ok(None),
        fs_types::NewTimestamp::Now => Ok(Some(now_nanos)),
        fs_types::NewTimestamp::Timestamp(instant) => {
            if instant.seconds < 0 {
                return Err(fs_types::ErrorCode::Invalid);
            }
            let seconds: u64 = instant
                .seconds
                .try_into()
                .map_err(|_| fs_types::ErrorCode::Overflow)?;
            let nanos = seconds
                .checked_mul(1_000_000_000)
                .and_then(|value| value.checked_add(u64::from(instant.nanoseconds)))
                .ok_or(fs_types::ErrorCode::Overflow)?;
            Ok(Some(nanos))
        }
    }
}
