use super::*;
use crate::ComponentHostNetwork;

/// Builds the list a `wasi:random` call returns, `len` bytes long, and is the
/// only place a guest-chosen random length becomes an allocation.
///
/// The buffer is reserved fallibly: a length the kernel heap cannot serve
/// traps the calling instance with [`WasiAdapterTrap::RandomBytesUnavailable`]
/// instead of reaching the infallible allocator, whose failure is a kernel
/// panic. `fill` runs once, on the whole buffer, only after the reservation
/// succeeded.
pub(crate) fn random_bytes(len: u64, fill: impl FnOnce(&mut [u8])) -> Result<Vec<u8>> {
    let size = usize::try_from(len)
        .map_err(|_| wasmtime::Error::new(WasiAdapterTrap::RandomLengthOverflow))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| wasmtime::Error::new(WasiAdapterTrap::RandomBytesUnavailable { len }))?;
    bytes.resize(size, 0_u8);
    fill(&mut bytes);
    Ok(bytes)
}

pub struct TerminalInput;
pub struct TerminalOutput;
impl<CpuImpl, Net, StorageImpl> wasi::clocks::monotonic_clock::Host
    for StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    fn now(&mut self) -> Result<wasi::clocks::monotonic_clock::Mark> {
        Ok(self.now_nanos())
    }

    fn get_resolution(&mut self) -> Result<wasi::clocks::types::Duration> {
        Ok(1)
    }
}

impl<CpuImpl, Net, StorageImpl, U> wasi::clocks::monotonic_clock::HostWithStore<U>
    for HasSelf<StoreData<CpuImpl, Net, StorageImpl>>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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

impl<CpuImpl, Net, StorageImpl> wasi::clocks::system_clock::Host
    for StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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

impl<CpuImpl, Net, StorageImpl> wasi::clocks::timezone::Host
    for StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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

impl<CpuImpl, Net, StorageImpl> wasi::cli::environment::Host
    for StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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

impl<CpuImpl, Net, StorageImpl> wasi::cli::exit::Host for StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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

impl<CpuImpl, Net, StorageImpl> wasi::cli::stdin::Host for StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
}

impl<CpuImpl, Net, StorageImpl, U> wasi::cli::stdin::HostWithStore<U>
    for HasSelf<StoreData<CpuImpl, Net, StorageImpl>>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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

impl<CpuImpl, Net, StorageImpl> wasi::cli::stdout::Host for StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
}

impl<CpuImpl, Net, StorageImpl, U> wasi::cli::stdout::HostWithStore<U>
    for HasSelf<StoreData<CpuImpl, Net, StorageImpl>>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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

impl<CpuImpl, Net, StorageImpl> wasi::cli::stderr::Host for StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
}

impl<CpuImpl, Net, StorageImpl, U> wasi::cli::stderr::HostWithStore<U>
    for HasSelf<StoreData<CpuImpl, Net, StorageImpl>>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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

impl<CpuImpl, Net, StorageImpl> wasi::cli::terminal_input::Host
    for StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
}
impl<CpuImpl, Net, StorageImpl> wasi::cli::terminal_input::HostTerminalInput
    for StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    fn drop(&mut self, resource: Resource<TerminalInput>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl<CpuImpl, Net, StorageImpl> wasi::cli::terminal_output::Host
    for StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
}
impl<CpuImpl, Net, StorageImpl> wasi::cli::terminal_output::HostTerminalOutput
    for StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    fn drop(&mut self, resource: Resource<TerminalOutput>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl<CpuImpl, Net, StorageImpl> wasi::cli::terminal_stdin::Host
    for StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    fn get_terminal_stdin(&mut self) -> Result<Option<Resource<TerminalInput>>> {
        if !stdin_is_terminal(self.output_mode()) {
            return Ok(None);
        }
        Ok(Some(self.table.push(TerminalInput)?))
    }
}

impl<CpuImpl, Net, StorageImpl> wasi::cli::terminal_stdout::Host
    for StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    fn get_terminal_stdout(&mut self) -> Result<Option<Resource<TerminalOutput>>> {
        if !output_is_terminal(self.output_mode(), ComponentOutputStreamKind::Stdout) {
            return Ok(None);
        }
        Ok(Some(self.table.push(TerminalOutput)?))
    }
}

impl<CpuImpl, Net, StorageImpl> wasi::cli::terminal_stderr::Host
    for StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    fn get_terminal_stderr(&mut self) -> Result<Option<Resource<TerminalOutput>>> {
        if !output_is_terminal(self.output_mode(), ComponentOutputStreamKind::Stderr) {
            return Ok(None);
        }
        Ok(Some(self.table.push(TerminalOutput)?))
    }
}

impl<CpuImpl, Net, StorageImpl> wasi::random::random::Host for StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    fn get_random_bytes(&mut self, len: u64) -> Result<Vec<u8>> {
        random_bytes(len, |bytes| self.fill_secure_random(bytes))
    }

    fn get_random_u64(&mut self) -> Result<u64> {
        Ok(self.secure_random_u64())
    }
}

impl<CpuImpl, Net, StorageImpl> wasi::random::insecure::Host
    for StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    fn get_insecure_random_bytes(&mut self, len: u64) -> Result<Vec<u8>> {
        random_bytes(len, |bytes| self.fill_insecure_random(bytes))
    }

    fn get_insecure_random_u64(&mut self) -> Result<u64> {
        Ok(self.insecure_random_u64())
    }
}

impl<CpuImpl, Net, StorageImpl> wasi::random::insecure_seed::Host
    for StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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
