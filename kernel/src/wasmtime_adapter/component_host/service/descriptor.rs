use super::*;

pub(super) struct WasixChildProcess {
    pub(super) pid: u32,
    pub(super) signal_state: WasixSignalState,
    pub(super) exit:
        Option<futures::channel::oneshot::Receiver<Result<ChildExit, ProgramExecError>>>,
    pub(super) completed: Option<u32>,
}

pub(super) struct WasixThread {
    pub(super) tid: u32,
    pub(super) signal_state: WasixSignalState,
    pub(super) exit: Option<futures::channel::oneshot::Receiver<u32>>,
    pub(super) completed: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WasixSignalDisposition {
    pub(super) signal: u8,
    pub(super) action: WasixSignalDispositionAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WasixSignalDispositionAction {
    Default,
    Ignore,
}

#[derive(Clone)]
pub(super) struct WasixSignalState {
    pub(super) pending: Arc<AtomicU32>,
    pub(super) interval_generation: Arc<AtomicU64>,
}

impl WasixSignalState {
    pub(super) fn new() -> Self {
        Self {
            pending: Arc::new(AtomicU32::new(WASIX_NO_PENDING_SIGNAL)),
            interval_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(super) fn raise(&self, signal: u32) {
        self.pending.store(signal, AtomicOrdering::Release);
        crate::wasmtime_adapter::bump_user_engine_epoch();
    }

    pub(super) fn take_pending(&self) -> Option<u32> {
        match self
            .pending
            .swap(WASIX_NO_PENDING_SIGNAL, AtomicOrdering::AcqRel)
        {
            WASIX_NO_PENDING_SIGNAL => None,
            signal => Some(signal),
        }
    }

    pub(super) fn next_interval_generation(&self) -> u64 {
        self.interval_generation
            .fetch_add(1, AtomicOrdering::AcqRel)
            .saturating_add(1)
    }

    pub(super) fn cancel_interval(&self) {
        let _ = self.next_interval_generation();
    }

    pub(super) fn interval_generation_is_current(&self, generation: u64) -> bool {
        self.interval_generation.load(AtomicOrdering::Acquire) == generation
    }
}

#[derive(Clone, Copy)]
pub(super) struct WasixTtyState {
    pub(super) cols: u32,
    pub(super) rows: u32,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) stdin_tty: bool,
    pub(super) stdout_tty: bool,
    pub(super) stderr_tty: bool,
    pub(super) echo: bool,
    pub(super) line_buffered: bool,
    pub(super) line_feeds: bool,
}

#[derive(Clone)]
pub(super) struct Preview1Cwd {
    pub(super) guest_name: String,
    pub(super) descriptor: FsDescriptor,
}

#[derive(Clone)]
pub(super) struct Preview1DescriptorTable {
    pub(super) entries: Vec<Option<Preview1DescriptorEntry>>,
    pub(super) free: FreeDescriptorSlots,
}

#[derive(Clone)]
pub(super) struct Preview1DescriptorEntry {
    pub(super) descriptor: Preview1Descriptor,
    pub(super) close_on_exec: bool,
    pub(super) fdflags: u16,
}

#[derive(Clone)]
pub(super) enum Preview1Descriptor {
    Stdin {
        carry: Bytes,
    },
    Stdout,
    Stderr,
    PipeRead {
        reader: crate::ByteReader,
        carry: Bytes,
    },
    PipeWrite {
        writer: crate::ByteWriter,
    },
    Event(EventFd),
    Preopen {
        guest_name: String,
        descriptor: FsDescriptor,
    },
    File {
        descriptor: FsDescriptor,
        offset: u64,
        fdflags: u16,
    },
    NullDevice,
    Socket(WasixSocketDescriptor),
    Epoll(EpollDescriptor),
}

#[derive(Clone)]
pub(super) struct EpollDescriptor {
    pub(super) interests: Vec<EpollInterest>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct EpollInterest {
    pub(super) fd: i32,
    pub(super) events: u32,
    pub(super) data: EpollUserData,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct EpollUserData {
    pub(super) ptr: u32,
    pub(super) fd: u32,
    pub(super) data1: u32,
    pub(super) data2: u64,
}

pub(super) enum EpollWaitTarget {
    ByteReader {
        reader: crate::ByteReader,
        wait: crate::ByteReadWait,
    },
    Event {
        event: EventFd,
        wait: crate::NotifyWaiter,
    },
}

impl EpollWaitTarget {
    pub(super) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        match self {
            Self::ByteReader { reader, wait } => reader.poll_readable(cx, wait),
            Self::Event { event, wait } => event.poll_readable(cx, wait),
        }
    }
}

/// How long a waiter sleeps between re-probing descriptors that have no
/// wakeup source of their own.
///
/// Network sockets only advance when the device is driven and the serial
/// console has no readiness interrupt, so a waiter registered on one of them
/// has to come back and look. This matches the network service's own
/// progress cadence; it is a bounded sleep, not a spin.
pub(super) const P1_READINESS_REPOLL_INTERVAL: Duration = Duration::from_micros(50);

/// Everything a `poll_oneoff`/`epoll_wait` sleep has to watch.
pub(super) struct P1WaitSet {
    /// Descriptors that wake the task themselves (pipes, socketpairs,
    /// eventfds, child stdin).
    pub(super) notified: Vec<EpollWaitTarget>,
    /// Set when at least one descriptor only reveals readiness by being
    /// re-probed (network sockets, the serial console).
    pub(super) repoll: bool,
}

impl P1WaitSet {
    pub(super) fn new() -> Self {
        Self {
            notified: Vec::new(),
            repoll: false,
        }
    }

    /// True when nothing in the set can ever report readiness, so a waiter
    /// with no deadline would block forever.
    pub(super) fn is_empty(&self) -> bool {
        self.notified.is_empty() && !self.repoll
    }
}

/// Sleep until a watched descriptor wakes us, the re-probe cadence elapses,
/// or `remaining` runs out.
///
/// The caller re-probes after every step, so this only has to guarantee that
/// it yields control and comes back when something might have changed.
/// `remaining` of `None` means "no deadline": block until a descriptor makes
/// progress.
pub(super) async fn p1_wait_step<CpuImpl>(
    timer: &crate::Timer<CpuImpl>,
    wait: &mut P1WaitSet,
    remaining: Option<Duration>,
) where
    CpuImpl: Cpu + Clone,
{
    if wait.is_empty() && remaining.is_none() {
        // Nothing subscribed can ever become ready and no deadline will end
        // the call. Park rather than spin; the task is dropped with the
        // instance.
        core::future::pending::<()>().await;
        return;
    }

    // A descriptor without a wakeup source needs us back on a fixed cadence;
    // otherwise sleep the whole remaining budget in one go.
    let sleep = match (wait.repoll, remaining) {
        (true, Some(remaining)) => remaining.min(P1_READINESS_REPOLL_INTERVAL),
        (true, None) => P1_READINESS_REPOLL_INTERVAL,
        (false, Some(remaining)) => remaining,
        (false, None) => {
            // Every subscribed descriptor can wake us, so wait on them alone.
            let targets = &mut wait.notified;
            core::future::poll_fn(|cx| poll_epoll_wait_targets(targets, cx)).await;
            return;
        }
    };

    if sleep.is_zero() {
        return;
    }

    let mut timer = core::pin::pin!(timer.sleep_for(sleep));
    if wait.notified.is_empty() {
        timer.await;
        return;
    }

    // Race the wakeup sources against the sleep: whichever comes first ends
    // this step and sends the caller back to re-probe.
    let targets = &mut wait.notified;
    core::future::poll_fn(|cx| {
        if poll_epoll_wait_targets(targets, cx).is_ready() {
            return Poll::Ready(());
        }
        if timer.as_mut().poll(cx).is_ready() {
            return Poll::Ready(());
        }
        Poll::Pending
    })
    .await;
}

/// Register whatever wakes `fd` into `wait`.
///
/// Descriptors backed by a kernel channel register a real waiter. Network
/// sockets and the serial console have no wakeup source, so they only set the
/// re-probe flag.
pub(super) fn p1_add_wait_target<CpuImpl, HostFs>(
    store: &Preview1ProgramStore<CpuImpl, HostFs>,
    fd: i32,
    event_type: u8,
    wait: &mut P1WaitSet,
) where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    match store.descriptors.get(fd) {
        Some(Preview1Descriptor::PipeRead { reader, carry })
        | Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { reader, carry, .. }))
            if carry.is_empty() && event_type == P1_EVENTTYPE_FD_READ =>
        {
            wait.notified.push(EpollWaitTarget::ByteReader {
                reader: reader.clone(),
                wait: reader.wait_state(),
            });
        }
        Some(Preview1Descriptor::Event(event))
            if event_type == P1_EVENTTYPE_FD_READ && !event.is_readable() =>
        {
            wait.notified.push(EpollWaitTarget::Event {
                event: event.clone(),
                wait: event.wait_state(),
            });
        }
        Some(Preview1Descriptor::Stdin { carry })
            if carry.is_empty() && event_type == P1_EVENTTYPE_FD_READ =>
        {
            match &store.output_mode {
                // A child's stdin is a byte channel, so it can wake us.
                OutputMode::Child { stdin_rx, .. } | OutputMode::RoutedChild { stdin_rx, .. } => {
                    wait.notified.push(EpollWaitTarget::ByteReader {
                        reader: stdin_rx.clone(),
                        wait: stdin_rx.wait_state(),
                    });
                }
                // The serial console has no readiness interrupt.
                OutputMode::Serial => wait.repoll = true,
                // Trace output never delivers input; the probe reports it as
                // an immediate hangup, so there is nothing to wait for.
                OutputMode::Trace => {}
            }
        }
        Some(Preview1Descriptor::Socket(
            WasixSocketDescriptor::Tcp(
                WasixTcpSocket::Connected { .. } | WasixTcpSocket::Listening { .. },
            )
            | WasixSocketDescriptor::Udp(WasixUdpSocket::Bound { .. }),
        )) => wait.repoll = true,
        _ => {}
    }
}

/// Resolve a descriptor's readiness, consulting the network service when the
/// descriptor is a socket and the console when it is serial-backed stdin.
pub(super) async fn p1_descriptor_readiness<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    event_type: u8,
) -> Result<P1Readiness, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if event_type == P1_EVENTTYPE_FD_READ
        && matches!(
            caller.data().descriptors.get(fd),
            Some(Preview1Descriptor::Stdin { .. })
        )
    {
        return Ok(caller.data_mut().probe_stdin(fd));
    }
    let probe = match p1_probe_descriptor(caller.data().descriptors.get(fd), event_type)? {
        P1Probe::Local(readiness) => return Ok(readiness),
        P1Probe::Network(probe) => probe,
    };
    let Some(service) = caller.data().runtime_state.network_service() else {
        return Err(p1::errno::NETDOWN);
    };
    let readiness = match probe {
        P1NetworkProbe::TcpStream(stream) => service
            .tcp_readiness(stream)
            .await
            .map_err(p1_errno_from_tcp_error)?,
        P1NetworkProbe::TcpListener(listener) => service
            .tcp_listener_readiness(listener)
            .await
            .map_err(p1_errno_from_tcp_error)?,
        P1NetworkProbe::UdpSocket(socket) => service
            .udp_readiness(socket)
            .await
            .map_err(p1_errno_from_udp_error)?,
    };
    Ok(p1_readiness_from_socket(readiness, event_type))
}

pub(super) fn p1_readiness_from_socket(
    readiness: crate::SocketReadiness,
    event_type: u8,
) -> P1Readiness {
    let ready = if event_type == P1_EVENTTYPE_FD_WRITE {
        readiness.writable
    } else {
        readiness.readable
    };
    if !ready {
        return P1Readiness::Pending;
    }
    if event_type == P1_EVENTTYPE_FD_READ && readiness.hangup {
        return P1Readiness::Hangup;
    }
    // The stack reports "something is queued" without a length, and a
    // send is window-limited rather than byte-counted; report one byte so
    // callers see a non-zero count and read what is actually there.
    P1Readiness::Ready { bytes: 1 }
}

#[derive(Clone)]
pub(super) enum WasixSocketDescriptor {
    Tcp(WasixTcpSocket),
    Udp(WasixUdpSocket),
    Pair {
        reader: crate::ByteReader,
        writer: crate::ByteWriter,
        carry: Bytes,
        options: WasixSocketOptions,
        socket_type: i32,
    },
}

#[derive(Clone)]
pub(super) enum WasixTcpSocket {
    Unconnected {
        options: WasixSocketOptions,
    },
    Bound {
        local_port: u16,
        options: WasixSocketOptions,
    },
    Listening {
        listener: u64,
        local_port: u16,
        options: WasixSocketOptions,
    },
    Connected {
        stream: u64,
        peer_address: crate::Ipv4Address,
        peer_port: u16,
        options: WasixSocketOptions,
    },
}

#[derive(Clone)]
pub(super) enum WasixUdpSocket {
    Unbound {
        options: WasixSocketOptions,
    },
    Bound {
        socket: u64,
        local_port: u16,
        options: WasixSocketOptions,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WasixSocketOptions {
    pub(super) flag_bits: u32,
    pub(super) receive_buffer_size: u64,
    pub(super) send_buffer_size: u64,
    pub(super) receive_low_water: u64,
    pub(super) send_low_water: u64,
    pub(super) ttl: u64,
    pub(super) multicast_ttl_v4: u64,
    pub(super) receive_timeout: Option<u64>,
    pub(super) send_timeout: Option<u64>,
    pub(super) connect_timeout: Option<u64>,
    pub(super) accept_timeout: Option<u64>,
    pub(super) linger: Option<u64>,
}

impl Default for WasixSocketOptions {
    fn default() -> Self {
        Self {
            flag_bits: 0,
            receive_buffer_size: DEFAULT_WASIX_SOCKET_BUFFER_BYTES,
            send_buffer_size: DEFAULT_WASIX_SOCKET_BUFFER_BYTES,
            receive_low_water: DEFAULT_WASIX_SOCKET_LOW_WATER_BYTES,
            send_low_water: DEFAULT_WASIX_SOCKET_LOW_WATER_BYTES,
            ttl: DEFAULT_WASIX_SOCKET_TTL,
            multicast_ttl_v4: DEFAULT_WASIX_SOCKET_MULTICAST_TTL,
            receive_timeout: None,
            send_timeout: None,
            connect_timeout: None,
            accept_timeout: None,
            linger: None,
        }
    }
}

impl WasixSocketOptions {
    /// Records a boolean socket option, refusing the ones the netstack has no
    /// mechanism for.
    ///
    /// `SO_KEEPALIVE` has no keepalive timer behind it, and `TCP_NODELAY`
    /// cleared is a request for Nagle's algorithm, which the stack does not
    /// implement — it transmits as soon as the window allows. Accepting either
    /// would report a behaviour change that never happens, so both are
    /// answered with `ENOTSUP`; clearing keepalive and setting nodelay
    /// describe what the stack already does and succeed.
    pub(super) fn set_flag(&mut self, option: i32, flag: bool) -> i32 {
        let bit = match wasix_socket_flag_bit(option) {
            Ok(bit) => bit,
            Err(errno) => return errno,
        };
        match (option, flag) {
            (WASIX_SOCK_OPTION_KEEP_ALIVE, true) | (WASIX_SOCK_OPTION_NO_DELAY, false) => {
                return p1::errno::NOTSUP;
            }
            _ => {}
        }
        if flag {
            self.flag_bits |= bit;
        } else {
            self.flag_bits &= !bit;
        }
        p1::errno::SUCCESS
    }

    /// Records a sized socket option.
    ///
    /// Buffer sizes clamp to what the netstack actually reserves per socket so
    /// a read-back reports the effective value; `IP_TTL` is range-checked here
    /// and pushed to the live socket by the syscall wrapper.
    pub(super) fn set_size(&mut self, option: i32, size: u64) -> i32 {
        match option {
            WASIX_SOCK_OPTION_RECV_BUF_SIZE => {
                self.receive_buffer_size = size.min(WASIX_SOCKET_RECEIVE_BUFFER_CEILING);
            }
            WASIX_SOCK_OPTION_SEND_BUF_SIZE => {
                self.send_buffer_size = size.min(WASIX_SOCKET_SEND_BUFFER_CEILING);
            }
            WASIX_SOCK_OPTION_RECV_LOWAT => self.receive_low_water = size,
            WASIX_SOCK_OPTION_SEND_LOWAT => self.send_low_water = size,
            WASIX_SOCK_OPTION_TTL => {
                if size == 0 || size > u64::from(u8::MAX) {
                    return p1::errno::INVAL;
                }
                self.ttl = size;
            }
            WASIX_SOCK_OPTION_MULTICAST_TTL_V4 => self.multicast_ttl_v4 = size,
            WASIX_SOCK_OPTION_TYPE | WASIX_SOCK_OPTION_PROTO => return p1::errno::INVAL,
            _ => return p1::errno::INVAL,
        }
        p1::errno::SUCCESS
    }

    /// TTL to stamp on this socket's packets, as a netstack hop limit.
    pub(super) fn hop_limit(self) -> u8 {
        u8::try_from(self.ttl).unwrap_or(helios_netstack::DEFAULT_HOP_LIMIT)
    }

    pub(super) fn size(self, option: i32) -> Result<u64, i32> {
        match option {
            WASIX_SOCK_OPTION_RECV_BUF_SIZE => Ok(self.receive_buffer_size),
            WASIX_SOCK_OPTION_SEND_BUF_SIZE => Ok(self.send_buffer_size),
            WASIX_SOCK_OPTION_RECV_LOWAT => Ok(self.receive_low_water),
            WASIX_SOCK_OPTION_SEND_LOWAT => Ok(self.send_low_water),
            WASIX_SOCK_OPTION_TTL => Ok(self.ttl),
            WASIX_SOCK_OPTION_MULTICAST_TTL_V4 => Ok(self.multicast_ttl_v4),
            WASIX_SOCK_OPTION_TYPE | WASIX_SOCK_OPTION_PROTO => Err(p1::errno::INVAL),
            _ => Err(p1::errno::INVAL),
        }
    }

    pub(super) fn flag(self, option: i32) -> Result<bool, i32> {
        let bit = wasix_socket_flag_bit(option)?;
        Ok(self.flag_bits & bit != 0)
    }

    pub(super) fn set_time(&mut self, option: i32, time: Option<u64>) -> i32 {
        match option {
            WASIX_SOCK_OPTION_RECV_TIMEOUT => self.receive_timeout = time,
            WASIX_SOCK_OPTION_SEND_TIMEOUT => self.send_timeout = time,
            WASIX_SOCK_OPTION_CONNECT_TIMEOUT => self.connect_timeout = time,
            WASIX_SOCK_OPTION_ACCEPT_TIMEOUT => self.accept_timeout = time,
            WASIX_SOCK_OPTION_LINGER => self.linger = time,
            _ => return p1::errno::INVAL,
        }
        p1::errno::SUCCESS
    }

    pub(super) fn time(self, option: i32) -> Result<Option<u64>, i32> {
        match option {
            WASIX_SOCK_OPTION_RECV_TIMEOUT => Ok(self.receive_timeout),
            WASIX_SOCK_OPTION_SEND_TIMEOUT => Ok(self.send_timeout),
            WASIX_SOCK_OPTION_CONNECT_TIMEOUT => Ok(self.connect_timeout),
            WASIX_SOCK_OPTION_ACCEPT_TIMEOUT => Ok(self.accept_timeout),
            WASIX_SOCK_OPTION_LINGER => Ok(self.linger),
            _ => Err(p1::errno::INVAL),
        }
    }
}

impl WasixSocketDescriptor {
    pub(super) fn options(&self) -> &WasixSocketOptions {
        match self {
            WasixSocketDescriptor::Tcp(socket) => socket.options(),
            WasixSocketDescriptor::Udp(socket) => socket.options(),
            WasixSocketDescriptor::Pair { options, .. } => options,
        }
    }

    pub(super) fn options_mut(&mut self) -> &mut WasixSocketOptions {
        match self {
            WasixSocketDescriptor::Tcp(socket) => socket.options_mut(),
            WasixSocketDescriptor::Udp(socket) => socket.options_mut(),
            WasixSocketDescriptor::Pair { options, .. } => options,
        }
    }

    pub(super) fn socket_type(&self) -> i32 {
        match self {
            WasixSocketDescriptor::Tcp(_) => WASIX_SOCK_TYPE_STREAM,
            WasixSocketDescriptor::Udp(_) => WASIX_SOCK_TYPE_DGRAM,
            WasixSocketDescriptor::Pair { socket_type, .. } => *socket_type,
        }
    }

    pub(super) fn protocol(&self) -> u64 {
        match self {
            WasixSocketDescriptor::Tcp(_) => WASIX_IPPROTO_TCP,
            WasixSocketDescriptor::Udp(_) => WASIX_IPPROTO_UDP,
            WasixSocketDescriptor::Pair { .. } => 0,
        }
    }
}

impl WasixTcpSocket {
    pub(super) fn options(&self) -> &WasixSocketOptions {
        match self {
            WasixTcpSocket::Unconnected { options }
            | WasixTcpSocket::Bound { options, .. }
            | WasixTcpSocket::Listening { options, .. }
            | WasixTcpSocket::Connected { options, .. } => options,
        }
    }

    pub(super) fn options_mut(&mut self) -> &mut WasixSocketOptions {
        match self {
            WasixTcpSocket::Unconnected { options }
            | WasixTcpSocket::Bound { options, .. }
            | WasixTcpSocket::Listening { options, .. }
            | WasixTcpSocket::Connected { options, .. } => options,
        }
    }
}

impl WasixUdpSocket {
    pub(super) fn options(&self) -> &WasixSocketOptions {
        match self {
            WasixUdpSocket::Unbound { options } | WasixUdpSocket::Bound { options, .. } => options,
        }
    }

    pub(super) fn options_mut(&mut self) -> &mut WasixSocketOptions {
        match self {
            WasixUdpSocket::Unbound { options } | WasixUdpSocket::Bound { options, .. } => options,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WasixSocketAuthority {
    LocalOnly,
    Tcp,
    Udp,
}

#[derive(Clone)]
pub(super) struct EventFd {
    pub(super) state: Arc<Mutex<EventFdState>>,
    pub(super) notify: Arc<crate::Notify>,
    pub(super) semaphore: bool,
}

pub(super) struct EventFdState {
    pub(super) value: u64,
}

#[derive(Debug, Error)]
#[error("guest requested wasi preview1 exit")]
pub(super) struct Preview1Exit;

pub(super) fn preview1_cwd_from_authority(authority: &ProcessAuthority) -> Option<Preview1Cwd> {
    authority.cwd().map(|preopen| Preview1Cwd {
        guest_name: preopen.guest_name().to_owned(),
        descriptor: FsDescriptor {
            path: preopen.source_path().to_owned(),
            kind: FsNodeKind::Directory,
            flags: directory_authority_to_descriptor_flags(preopen.rights()),
            identity: None,
        },
    })
}

pub(super) fn guest_path_is_within_preopen(path: &str, preopen: &str) -> bool {
    if path == preopen {
        return true;
    }
    let prefix = crate::directory_prefix(preopen);
    path.starts_with(&prefix)
}

pub(super) fn guest_path_suffix<'a>(path: &'a str, preopen: &str) -> &'a str {
    if preopen == "/" {
        path.strip_prefix('/').unwrap_or(path)
    } else {
        path.strip_prefix(preopen)
            .and_then(|suffix| suffix.strip_prefix('/'))
            .unwrap_or("")
    }
}

impl Preview1DescriptorTable {
    pub(super) fn from_authority(authority: &ProcessAuthority) -> Self {
        let preopens = authority.directory_preopens();
        let mut entries = Vec::with_capacity(3 + preopens.len());
        entries.push(Some(Preview1DescriptorEntry::new(
            Preview1Descriptor::Stdin {
                carry: Bytes::new(),
            },
            false,
        )));
        entries.push(Some(Preview1DescriptorEntry::new(
            Preview1Descriptor::Stdout,
            false,
        )));
        entries.push(Some(Preview1DescriptorEntry::new(
            Preview1Descriptor::Stderr,
            false,
        )));
        let mut table = Self::from_entries(entries);
        for preopen in preopens {
            let descriptor = FsDescriptor {
                path: preopen.source_path().to_owned(),
                kind: FsNodeKind::Directory,
                flags: directory_authority_to_descriptor_flags(preopen.rights()),
                identity: None,
            };
            table.entries.push(Some(Preview1DescriptorEntry::new(
                Preview1Descriptor::Preopen {
                    guest_name: preopen.guest_name().to_owned(),
                    descriptor,
                },
                false,
            )));
        }
        table
    }

    pub(super) fn from_entries(entries: Vec<Option<Preview1DescriptorEntry>>) -> Self {
        let mut free = FreeDescriptorSlots::with_capacity(entries.len());
        for (index, entry) in entries.iter().enumerate() {
            if entry.is_none() {
                free.release(index);
            }
        }
        Self { entries, free }
    }

    pub(super) fn get(&self, fd: i32) -> Option<&Preview1Descriptor> {
        usize::try_from(fd)
            .ok()
            .and_then(|index| self.entries.get(index))
            .and_then(Option::as_ref)
            .map(|entry| &entry.descriptor)
    }

    pub(super) fn get_mut(&mut self, fd: i32) -> Option<&mut Preview1Descriptor> {
        usize::try_from(fd)
            .ok()
            .and_then(|index| self.entries.get_mut(index))
            .and_then(Option::as_mut)
            .map(|entry| &mut entry.descriptor)
    }

    pub(super) fn insert(&mut self, descriptor: Preview1Descriptor) -> Result<u32, i32> {
        self.insert_with_close_on_exec(descriptor, false)
    }

    pub(super) fn insert_with_close_on_exec(
        &mut self,
        descriptor: Preview1Descriptor,
        close_on_exec: bool,
    ) -> Result<u32, i32> {
        self.insert_entry(Preview1DescriptorEntry::new(descriptor, close_on_exec))
    }

    pub(super) fn insert_with_fdflags(
        &mut self,
        descriptor: Preview1Descriptor,
        close_on_exec: bool,
        fdflags: u16,
    ) -> Result<u32, i32> {
        self.insert_entry(Preview1DescriptorEntry {
            descriptor,
            close_on_exec,
            fdflags,
        })
    }

    pub(super) fn insert_entry(&mut self, entry: Preview1DescriptorEntry) -> Result<u32, i32> {
        let index = self.allocate_slot_index();
        self.entries[index] = Some(entry);
        u32::try_from(index).map_err(|_| p1::errno::OVERFLOW)
    }

    pub(super) fn dup(&mut self, fd: i32) -> Result<u32, i32> {
        let mut entry = self.get_entry(fd).cloned().ok_or(p1::errno::BADF)?;
        entry.close_on_exec = false;
        self.insert_entry(entry)
    }

    pub(super) fn dup_to(&mut self, fd: i32, to_fd: i32, close_on_exec: bool) -> Result<u32, i32> {
        let mut entry = self.get_entry(fd).cloned().ok_or(p1::errno::BADF)?;
        entry.close_on_exec = close_on_exec;
        self.insert_entry_at(to_fd, entry)
    }

    pub(super) fn insert_at(
        &mut self,
        fd: i32,
        descriptor: Preview1Descriptor,
        close_on_exec: bool,
    ) -> Result<u32, i32> {
        self.insert_entry_at(fd, Preview1DescriptorEntry::new(descriptor, close_on_exec))
    }

    pub(super) fn insert_entry_at(
        &mut self,
        fd: i32,
        entry: Preview1DescriptorEntry,
    ) -> Result<u32, i32> {
        let to = usize::try_from(fd).map_err(|_| p1::errno::BADF)?;
        if self.entries.len() <= to {
            let previous_len = self.entries.len();
            self.entries.resize_with(to + 1, || None);
            self.free.release_range(previous_len..to);
        }
        self.entries[to] = Some(entry);
        u32::try_from(to).map_err(|_| p1::errno::OVERFLOW)
    }

    pub(super) fn get_entry(&self, fd: i32) -> Option<&Preview1DescriptorEntry> {
        usize::try_from(fd)
            .ok()
            .and_then(|index| self.entries.get(index))
            .and_then(Option::as_ref)
    }

    pub(super) fn get_entry_mut(&mut self, fd: i32) -> Option<&mut Preview1DescriptorEntry> {
        usize::try_from(fd)
            .ok()
            .and_then(|index| self.entries.get_mut(index))
            .and_then(Option::as_mut)
    }

    pub(super) fn fdflags(&self, fd: i32) -> Result<u16, i32> {
        self.get_entry(fd)
            .map(|entry| entry.fdflags)
            .ok_or(p1::errno::BADF)
    }

    pub(super) fn set_fdflags(&mut self, fd: i32, fdflags: u16) -> i32 {
        let Some(entry) = self.get_entry_mut(fd) else {
            return p1::errno::BADF;
        };
        entry.fdflags = fdflags;
        if let Preview1Descriptor::File {
            fdflags: file_flags,
            ..
        } = &mut entry.descriptor
        {
            *file_flags = fdflags;
        }
        p1::errno::SUCCESS
    }

    pub(super) fn close_on_exec(&self, fd: i32) -> Result<bool, i32> {
        usize::try_from(fd)
            .ok()
            .and_then(|index| self.entries.get(index))
            .and_then(Option::as_ref)
            .map(|entry| entry.close_on_exec)
            .ok_or(p1::errno::BADF)
    }

    pub(super) fn set_close_on_exec(&mut self, fd: i32, close_on_exec: bool) -> i32 {
        match usize::try_from(fd)
            .ok()
            .and_then(|index| self.entries.get_mut(index))
            .and_then(Option::as_mut)
        {
            Some(entry) => {
                entry.close_on_exec = close_on_exec;
                p1::errno::SUCCESS
            }
            None => p1::errno::BADF,
        }
    }

    pub(super) fn clone_for_exec(&self) -> Self {
        Self::from_entries(
            self.entries
                .iter()
                .map(|entry| match entry {
                    Some(entry) if entry.close_on_exec => None,
                    _ => entry.clone(),
                })
                .collect(),
        )
    }

    pub(super) fn close(&mut self, fd: i32) -> i32 {
        let Some(index) = usize::try_from(fd).ok() else {
            return p1::errno::BADF;
        };
        self.entries
            .get_mut(index)
            .and_then(Option::take)
            .map(|_| self.free.release(index))
            .map_or(p1::errno::BADF, |_| p1::errno::SUCCESS)
    }

    pub(super) fn allocate_slot_index(&mut self) -> usize {
        while let Some(index) = self.free.allocate() {
            if self.entries.get(index).is_some_and(Option::is_none) {
                return index;
            }
        }
        let index = self.entries.len();
        self.entries.push(None);
        index
    }

    pub(super) fn get_owned_entry(
        &mut self,
        fd: i32,
    ) -> Result<(usize, Preview1DescriptorEntry), i32> {
        let index = usize::try_from(fd).map_err(|_| p1::errno::BADF)?;
        let entry = self
            .entries
            .get_mut(index)
            .and_then(Option::take)
            .ok_or(p1::errno::BADF)?;
        Ok((index, entry))
    }

    pub(super) fn close_slot(&mut self, index: usize) {
        self.entries[index] = None;
        self.free.release(index);
    }

    pub(super) fn renumber(&mut self, from: i32, to: i32) -> i32 {
        let Ok(to) = usize::try_from(to) else {
            return p1::errno::BADF;
        };
        let Ok((from, mut entry)) = self.get_owned_entry(from) else {
            return p1::errno::BADF;
        };
        entry.close_on_exec = false;
        if from == to {
            self.entries[from] = Some(entry);
            return p1::errno::SUCCESS;
        }
        if self.entries.len() <= to {
            let previous_len = self.entries.len();
            self.entries.resize_with(to + 1, || None);
            self.free.release_range(previous_len..to);
        }
        self.close_slot(from);
        self.entries[to] = Some(entry);
        p1::errno::SUCCESS
    }
}

impl Preview1DescriptorEntry {
    pub(super) fn new(descriptor: Preview1Descriptor, close_on_exec: bool) -> Self {
        let fdflags = preview1_descriptor_initial_fdflags(&descriptor);
        Self {
            descriptor,
            close_on_exec,
            fdflags,
        }
    }
}

pub(super) fn preview1_descriptor_initial_fdflags(descriptor: &Preview1Descriptor) -> u16 {
    match descriptor {
        Preview1Descriptor::File { fdflags, .. } => *fdflags,
        _ => 0,
    }
}

impl EventFd {
    pub(super) fn new(value: u64, semaphore: bool) -> Self {
        Self {
            state: Arc::new(Mutex::new(EventFdState { value })),
            notify: Arc::new(crate::Notify::new()),
            semaphore,
        }
    }

    pub(super) fn write(&self, increment: u64) -> Result<(), i32> {
        if increment == u64::MAX {
            return Err(p1::errno::INVAL);
        }
        let mut state = self.state.lock();
        let next = state
            .value
            .checked_add(increment)
            .filter(|value| *value != u64::MAX)
            .ok_or(p1::errno::AGAIN)?;
        state.value = next;
        drop(state);
        self.notify.notify_all();
        Ok(())
    }

    pub(super) fn is_readable(&self) -> bool {
        self.state.lock().value != 0
    }

    pub(super) fn wait_state(&self) -> crate::NotifyWaiter {
        self.notify.waiter()
    }

    pub(super) fn poll_readable(
        &self,
        cx: &mut Context<'_>,
        wait: &mut crate::NotifyWaiter,
    ) -> Poll<()> {
        loop {
            if self.is_readable() {
                return Poll::Ready(());
            }
            match self.notify.poll_notified(cx, wait) {
                Poll::Ready(()) => continue,
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    pub(super) async fn read(&self) -> u64 {
        loop {
            {
                let mut state = self.state.lock();
                if state.value != 0 {
                    if self.semaphore {
                        state.value -= 1;
                        return 1;
                    }
                    return core::mem::take(&mut state.value);
                }
            }
            self.notify.notified().await;
        }
    }
}

impl WasixTtyState {
    pub(super) fn from_authority(authority: &ProcessAuthority) -> Self {
        let rights = authority.terminal_rights();
        let input = rights.contains(TerminalAuthorityRights::INPUT);
        let output = rights.contains(TerminalAuthorityRights::OUTPUT);
        Self {
            cols: 80,
            rows: 24,
            width: 0,
            height: 0,
            stdin_tty: input,
            stdout_tty: output,
            stderr_tty: output,
            echo: input,
            line_buffered: input,
            line_feeds: true,
        }
    }
}

pub(super) fn directory_authority_to_descriptor_flags(
    rights: DirectoryAuthorityRights,
) -> fs_types::DescriptorFlags {
    let mut flags = fs_types::DescriptorFlags::empty();
    if rights.contains(DirectoryAuthorityRights::READ) {
        flags |= fs_types::DescriptorFlags::READ;
    }
    if rights.contains(DirectoryAuthorityRights::WRITE) {
        flags |= fs_types::DescriptorFlags::WRITE;
    }
    if rights.contains(DirectoryAuthorityRights::MUTATE_DIRECTORY) {
        flags |= fs_types::DescriptorFlags::MUTATE_DIRECTORY;
    }
    flags
}

pub(super) fn descriptor_flags_to_directory_authority(
    flags: fs_types::DescriptorFlags,
) -> DirectoryAuthorityRights {
    let mut rights = DirectoryAuthorityRights::empty();
    if flags.contains(fs_types::DescriptorFlags::READ) {
        rights |= DirectoryAuthorityRights::READ;
    }
    if flags.contains(fs_types::DescriptorFlags::WRITE) {
        rights |= DirectoryAuthorityRights::WRITE;
    }
    if flags.contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY) {
        rights |= DirectoryAuthorityRights::MUTATE_DIRECTORY;
    }
    rights
}

pub(super) fn p1_descriptor_rights(descriptor: &Preview1Descriptor) -> u64 {
    match descriptor {
        Preview1Descriptor::Stdin { .. } => P1_RIGHT_FD_READ | P1_RIGHT_POLL_FD_READWRITE,
        Preview1Descriptor::Stdout | Preview1Descriptor::Stderr => {
            P1_RIGHT_FD_WRITE | P1_RIGHT_POLL_FD_READWRITE
        }
        Preview1Descriptor::PipeRead { .. } => P1_RIGHT_FD_READ | P1_RIGHT_POLL_FD_READWRITE,
        Preview1Descriptor::PipeWrite { .. } => P1_RIGHT_FD_WRITE | P1_RIGHT_POLL_FD_READWRITE,
        Preview1Descriptor::Event(_) => {
            P1_RIGHT_FD_READ | P1_RIGHT_FD_WRITE | P1_RIGHT_POLL_FD_READWRITE
        }
        Preview1Descriptor::NullDevice => {
            P1_RIGHT_FD_READ
                | P1_RIGHT_FD_WRITE
                | P1_RIGHT_FD_FDSTAT_SET_FLAGS
                | P1_RIGHT_FD_FILESTAT_GET
                | P1_RIGHT_POLL_FD_READWRITE
        }
        Preview1Descriptor::Epoll(_) => P1_RIGHT_FD_READ | P1_RIGHT_POLL_FD_READWRITE,
        Preview1Descriptor::Socket(_) => {
            P1_RIGHT_FD_READ | P1_RIGHT_FD_WRITE | P1_RIGHT_POLL_FD_READWRITE
        }
        Preview1Descriptor::Preopen { descriptor, .. }
        | Preview1Descriptor::File { descriptor, .. } => {
            let mut rights = P1_RIGHT_FD_ADVISE | P1_RIGHT_FD_FILESTAT_GET;
            if descriptor.flags.contains(fs_types::DescriptorFlags::READ) {
                rights |= P1_RIGHT_FD_READ
                    | P1_RIGHT_FD_SEEK
                    | P1_RIGHT_FD_TELL
                    | P1_RIGHT_FD_READDIR
                    | P1_RIGHT_POLL_FD_READWRITE
                    | P1_RIGHT_PATH_READ_MASK;
            }
            if descriptor.flags.contains(fs_types::DescriptorFlags::WRITE) {
                rights |= P1_RIGHT_FD_DATASYNC
                    | P1_RIGHT_FD_SYNC
                    | P1_RIGHT_FD_WRITE
                    | P1_RIGHT_FD_ALLOCATE
                    | P1_RIGHT_FD_FDSTAT_SET_FLAGS
                    | P1_RIGHT_FD_FILESTAT_SET_SIZE
                    | P1_RIGHT_FD_FILESTAT_SET_TIMES
                    | P1_RIGHT_PATH_FILE_WRITE_MASK;
            }
            if descriptor
                .flags
                .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
            {
                rights |= P1_RIGHT_PATH_MUTATE_MASK;
            }
            rights
        }
    }
}

pub(super) fn p1_filetype(kind: FsNodeKind) -> u8 {
    match kind {
        FsNodeKind::Directory => 3,
        FsNodeKind::File => 4,
        FsNodeKind::Symlink => 7,
    }
}

pub(super) fn p1_filetype_from_descriptor_type(type_: fs_types::DescriptorType) -> u8 {
    match type_ {
        fs_types::DescriptorType::Directory => 3,
        fs_types::DescriptorType::RegularFile => 4,
        fs_types::DescriptorType::SymbolicLink => 7,
        fs_types::DescriptorType::CharacterDevice => 2,
        fs_types::DescriptorType::BlockDevice => 1,
        _ => 0,
    }
}

pub(super) fn p1_descriptor_path(descriptor: Option<&Preview1Descriptor>) -> Option<&str> {
    match descriptor {
        Some(Preview1Descriptor::Preopen { descriptor, .. })
        | Some(Preview1Descriptor::File { descriptor, .. }) => Some(&descriptor.path),
        _ => None,
    }
}

pub(super) fn p1_null_device_stat() -> fs_types::DescriptorStat {
    fs_types::DescriptorStat {
        type_: fs_types::DescriptorType::CharacterDevice,
        link_count: 1,
        size: 0,
        data_access_timestamp: None,
        data_modification_timestamp: None,
        status_change_timestamp: None,
    }
}

/// Stable identity of the synthetic `/dev/null` character device.
///
/// It has no backing filesystem object, so it gets the guest-device authority
/// domain and a fixed inode: two `stat` calls on `/dev/null` must agree, and
/// its `st_dev` must not collide with bootfs or the host share.
pub(super) fn p1_null_device_identity() -> crate::ObjectIdentity {
    crate::ObjectIdentity::new(crate::AuthorityDomain::GUEST_DEVICES, 1)
}

pub(super) fn p1_directory_descriptor(
    descriptor: Option<&Preview1Descriptor>,
) -> Option<&FsDescriptor> {
    match descriptor {
        Some(Preview1Descriptor::Preopen { descriptor, .. })
        | Some(Preview1Descriptor::File { descriptor, .. })
            if descriptor.kind == FsNodeKind::Directory =>
        {
            Some(descriptor)
        }
        _ => None,
    }
}

/// Readiness of one descriptor for a single `poll_oneoff`/`epoll` interest.
///
/// The probe used to return `Result<u64, i32>`, where `Ok(0)` meant both
/// "ready, nothing buffered" and "not ready at all". `poll_oneoff` read that
/// as ready and reported every subscription; `epoll_wait` read it as not
/// ready and never reported a socket. The tri-state removes the ambiguity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum P1Readiness {
    /// The operation would block; the caller has to wait for this descriptor.
    Pending,
    /// A non-blocking operation would make progress now. `bytes` is a
    /// best-effort count of what is immediately available; zero is valid
    /// (a regular file at end-of-file is always ready with nothing left).
    Ready { bytes: u64 },
    /// The stream ended: the peer hung up, or a read yields end-of-file.
    /// Ready for reporting purposes, and additionally flagged as a hangup.
    Hangup,
}

impl P1Readiness {
    pub(super) fn is_ready(self) -> bool {
        !matches!(self, Self::Pending)
    }

    pub(super) fn bytes(self) -> u64 {
        match self {
            Self::Ready { bytes } => bytes,
            Self::Pending | Self::Hangup => 0,
        }
    }

    pub(super) fn is_hangup(self) -> bool {
        matches!(self, Self::Hangup)
    }
}

/// Outcome of the synchronous readiness probe.
///
/// Descriptors backed by kernel-owned buffers answer immediately. Network
/// sockets cannot: the stack only advances when the device is driven, so
/// their readiness has to be resolved through the async network service.
/// Making that an explicit variant keeps a caller from silently reading a
/// socket as "not ready" when it simply never asked the network.
pub(super) enum P1Probe {
    Local(P1Readiness),
    Network(P1NetworkProbe),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum P1NetworkProbe {
    TcpStream(u64),
    TcpListener(u64),
    UdpSocket(u64),
}

/// Probe every descriptor whose readiness is answerable from local state.
///
/// `event_type` is one of `P1_EVENTTYPE_FD_READ` / `P1_EVENTTYPE_FD_WRITE`.
pub(super) fn p1_probe_descriptor(
    descriptor: Option<&Preview1Descriptor>,
    event_type: u8,
) -> Result<P1Probe, i32> {
    let local = |readiness| Ok(P1Probe::Local(readiness));
    match (descriptor, event_type) {
        (Some(Preview1Descriptor::Stdin { carry }), P1_EVENTTYPE_FD_READ) => {
            // Buffered stdin is ready; an empty carry means the backing
            // source has to be polled, which only the waiter can do.
            if carry.is_empty() {
                local(P1Readiness::Pending)
            } else {
                local(P1Readiness::Ready {
                    bytes: carry.len() as u64,
                })
            }
        }
        (Some(Preview1Descriptor::PipeRead { reader, carry }), P1_EVENTTYPE_FD_READ)
        | (
            Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { reader, carry, .. })),
            P1_EVENTTYPE_FD_READ,
        ) => {
            if !carry.is_empty() {
                return local(P1Readiness::Ready {
                    bytes: carry.len() as u64,
                });
            }
            // `is_readable` covers queued bytes and a closed writer alike;
            // the channel exposes no queue length, so report one byte
            // available and let the read itself surface end-of-stream.
            if reader.is_readable() {
                local(P1Readiness::Ready { bytes: 1 })
            } else {
                local(P1Readiness::Pending)
            }
        }
        (Some(Preview1Descriptor::Event(event)), P1_EVENTTYPE_FD_READ) => {
            if event.is_readable() {
                local(P1Readiness::Ready { bytes: 8 })
            } else {
                local(P1Readiness::Pending)
            }
        }
        // Reading /dev/null returns end-of-file immediately, so it never
        // blocks.
        (Some(Preview1Descriptor::NullDevice), P1_EVENTTYPE_FD_READ) => {
            local(P1Readiness::Ready { bytes: 0 })
        }
        // POSIX: regular files and directories are always ready for both
        // directions. The byte count is advisory and the descriptor does not
        // carry a cached size, so report readiness without one rather than
        // paying a filesystem round-trip per poll.
        (
            Some(Preview1Descriptor::File { .. }) | Some(Preview1Descriptor::Preopen { .. }),
            P1_EVENTTYPE_FD_READ | P1_EVENTTYPE_FD_WRITE,
        ) => local(P1Readiness::Ready { bytes: 0 }),
        (Some(Preview1Descriptor::Stdout), P1_EVENTTYPE_FD_WRITE)
        | (Some(Preview1Descriptor::Stderr), P1_EVENTTYPE_FD_WRITE)
        | (Some(Preview1Descriptor::PipeWrite { .. }), P1_EVENTTYPE_FD_WRITE)
        | (Some(Preview1Descriptor::Event(_)), P1_EVENTTYPE_FD_WRITE)
        | (Some(Preview1Descriptor::NullDevice), P1_EVENTTYPE_FD_WRITE)
        | (
            Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { .. })),
            P1_EVENTTYPE_FD_WRITE,
        ) => local(P1Readiness::Ready {
            bytes: usize::MAX as u64,
        }),
        (
            Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
                WasixTcpSocket::Connected { stream, .. },
            ))),
            P1_EVENTTYPE_FD_READ | P1_EVENTTYPE_FD_WRITE,
        ) => Ok(P1Probe::Network(P1NetworkProbe::TcpStream(*stream))),
        (
            Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
                WasixTcpSocket::Listening { listener, .. },
            ))),
            P1_EVENTTYPE_FD_READ | P1_EVENTTYPE_FD_WRITE,
        ) => Ok(P1Probe::Network(P1NetworkProbe::TcpListener(*listener))),
        (
            Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Bound {
                socket,
                ..
            }))),
            P1_EVENTTYPE_FD_READ | P1_EVENTTYPE_FD_WRITE,
        ) => Ok(P1Probe::Network(P1NetworkProbe::UdpSocket(*socket))),
        // A socket with no endpoint yet can never make progress; it is not
        // an error to poll it, it simply never becomes ready.
        (
            Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
                WasixTcpSocket::Unconnected { .. } | WasixTcpSocket::Bound { .. },
            ))),
            P1_EVENTTYPE_FD_READ | P1_EVENTTYPE_FD_WRITE,
        )
        | (
            Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Unbound {
                ..
            }))),
            P1_EVENTTYPE_FD_READ | P1_EVENTTYPE_FD_WRITE,
        ) => local(P1Readiness::Pending),
        (Some(_), _) => Err(p1::errno::INVAL),
        (None, _) => Err(p1::errno::BADF),
    }
}

pub(super) fn p1_descriptor_stat_from_host_metadata(
    metadata: &crate::HostMetadata,
) -> fs_types::DescriptorStat {
    crate::wasmtime_adapter::wasi::descriptor_stat_from_host_metadata(metadata)
}
