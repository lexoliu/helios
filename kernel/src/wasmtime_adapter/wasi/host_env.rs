use super::*;

pub(crate) fn random_len(len: u64) -> Result<usize> {
    usize::try_from(len).map_err(|_| wasmtime::Error::new(WasiAdapterTrap::RandomLengthOverflow))
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

/// The timezone Helios reports through `wasi:clocks/timezone`.
///
/// Helios keeps wall-clock time in UTC and carries no timezone database, so
/// the configured zone is UTC with a constant zero offset at every instant.
/// `UTC` is a valid IANA Time Zone Database identifier, so guests take their
/// normal timezone-aware path instead of the "no timezone available"
/// fallback.
pub(super) struct HostTimezone;

impl HostTimezone {
    const IANA_ID: &'static str = "UTC";

    pub(super) fn iana_id() -> Option<String> {
        Some(Self::IANA_ID.to_owned())
    }

    pub(super) fn utc_offset_nanos(_when: wasi::clocks::timezone::Instant) -> Option<i64> {
        Some(0)
    }

    pub(super) fn debug_string() -> String {
        Self::IANA_ID.to_owned()
    }
}

impl<CpuImpl, HostFs> wasi::clocks::timezone::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn iana_id(&mut self) -> Result<Option<String>> {
        Ok(HostTimezone::iana_id())
    }

    fn utc_offset(&mut self, when: wasi::clocks::timezone::Instant) -> Result<Option<i64>> {
        Ok(HostTimezone::utc_offset_nanos(when))
    }

    fn to_debug_string(&mut self) -> Result<String> {
        Ok(HostTimezone::debug_string())
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
        if !stdin_is_terminal(self.output_mode()) {
            return Ok(None);
        }
        Ok(Some(self.table.push(TerminalInput)?))
    }
}

impl<CpuImpl, HostFs> wasi::cli::terminal_stdout::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_terminal_stdout(&mut self) -> Result<Option<Resource<TerminalOutput>>> {
        if !output_is_terminal(self.output_mode(), ComponentOutputStreamKind::Stdout) {
            return Ok(None);
        }
        Ok(Some(self.table.push(TerminalOutput)?))
    }
}

impl<CpuImpl, HostFs> wasi::cli::terminal_stderr::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_terminal_stderr(&mut self) -> Result<Option<Resource<TerminalOutput>>> {
        if !output_is_terminal(self.output_mode(), ComponentOutputStreamKind::Stderr) {
            return Ok(None);
        }
        Ok(Some(self.table.push(TerminalOutput)?))
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
        self.fill_secure_random(&mut bytes);
        Ok(bytes)
    }

    fn get_random_u64(&mut self) -> Result<u64> {
        Ok(self.secure_random_u64())
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

#[cfg(test)]
mod tests {
    use super::{HostTimezone, wasi};

    #[test]
    fn host_timezone_reports_utc_with_a_zero_offset() {
        assert_eq!(HostTimezone::iana_id().as_deref(), Some("UTC"));
        assert_eq!(HostTimezone::debug_string(), "UTC");
        for seconds in [i64::MIN, -1, 0, 1, i64::MAX] {
            let when = wasi::clocks::timezone::Instant {
                seconds,
                nanoseconds: 500_000_000,
            };
            assert_eq!(
                HostTimezone::utc_offset_nanos(when),
                Some(0),
                "UTC offset must stay zero at every instant"
            );
        }
    }
}
