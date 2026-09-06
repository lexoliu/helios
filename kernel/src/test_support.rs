//! Fixtures the kernel's own unit tests share.
//!
//! A platform value is the first thing most kernel code asks for, and a
//! test that needs one should not have to spell a whole `Cpu`
//! implementation out again.

use core::sync::atomic::{AtomicU64, Ordering};

use helios_hal::cpu::{Cpu, Instant, ProcessorId};
use helios_hal::entropy::{EntropyQuality, EntropyUnavailable};
use triomphe::Arc;

/// A CPU whose only interesting behaviour is whether it has an
/// entropy source, and what that source produces.
#[derive(Clone, Copy)]
pub(crate) struct TestCpu {
    entropy: Option<u8>,
}

impl TestCpu {
    pub(crate) const fn with_entropy(fill: u8) -> Self {
        Self {
            entropy: Some(fill),
        }
    }

    pub(crate) const fn without_entropy() -> Self {
        Self { entropy: None }
    }
}

impl Cpu for TestCpu {
    fn current_processor(&self) -> ProcessorId {
        ProcessorId::new(0)
    }

    fn has_lazy_commit_virtual_memory(&self) -> bool {
        // The unit tests build on a hosted platform, where the operating
        // system commits a reservation lazily on its own. A test platform
        // that claimed otherwise could not stand in for any real backend:
        // every one of them reserves user memory this way.
        true
    }

    fn processor_count(&self) -> usize {
        1
    }

    fn bootstrap_processor(&self) -> ProcessorId {
        ProcessorId::new(0)
    }

    fn park_current(&self) {}

    fn start_processor(&self, _: ProcessorId) {}

    fn wake_processor(&self, _: ProcessorId) {}

    fn now(&self) -> Instant {
        Instant::new(11)
    }

    fn timer_frequency(&self) -> u64 {
        1_000_000
    }

    fn set_deadline(&self, _: Instant) {}

    fn publish_executable(&self, _: *const u8, _: usize) {}

    fn unpublish_executable(&self, _: *const u8, _: usize) {}

    fn native_feature_probe(&self) -> Option<fn(&str) -> Option<bool>> {
        None
    }

    fn fill_entropy(&self, buffer: &mut [u8]) -> Result<EntropyQuality, EntropyUnavailable> {
        let fill = self.entropy.ok_or(EntropyUnavailable)?;
        buffer.fill(fill);
        Ok(EntropyQuality::Cryptographic)
    }

    fn shutdown(&self) -> ! {
        panic!("test CPU should not shut down")
    }

    fn reboot(&self) -> ! {
        panic!("test CPU should not reboot")
    }
}

/// A CPU whose clock a test moves by hand.
///
/// Its timebase is one tick per nanosecond, so a test that wants to
/// step past a two-second TTL says so in nanoseconds and does not have
/// to reason about a tick conversion at the same time.
#[derive(Clone)]
pub(crate) struct ManualClockCpu {
    nanos: Arc<AtomicU64>,
}

impl ManualClockCpu {
    pub(crate) fn new() -> Self {
        Self {
            nanos: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Moves the clock forward by `nanos`.
    pub(crate) fn advance(&self, nanos: u64) {
        self.nanos.fetch_add(nanos, Ordering::Relaxed);
    }
}

impl Cpu for ManualClockCpu {
    fn current_processor(&self) -> ProcessorId {
        ProcessorId::new(0)
    }

    fn processor_count(&self) -> usize {
        1
    }

    fn bootstrap_processor(&self) -> ProcessorId {
        ProcessorId::new(0)
    }

    fn park_current(&self) {}

    fn start_processor(&self, _: ProcessorId) {}

    fn wake_processor(&self, _: ProcessorId) {}

    fn now(&self) -> Instant {
        Instant::new(self.nanos.load(Ordering::Relaxed))
    }

    fn timer_frequency(&self) -> u64 {
        1_000_000_000
    }

    fn set_deadline(&self, _: Instant) {}

    fn publish_executable(&self, _: *const u8, _: usize) {}

    fn unpublish_executable(&self, _: *const u8, _: usize) {}

    fn native_feature_probe(&self) -> Option<fn(&str) -> Option<bool>> {
        None
    }

    fn fill_entropy(&self, _: &mut [u8]) -> Result<EntropyQuality, EntropyUnavailable> {
        Err(EntropyUnavailable)
    }

    fn shutdown(&self) -> ! {
        panic!("test CPU should not shut down")
    }

    fn reboot(&self) -> ! {
        panic!("test CPU should not reboot")
    }
}

/// A CPU that answers for a chosen slot out of a chosen processor count
/// and records every cross-processor wake it is asked to deliver.
///
/// SMP hand-off paths — the network RX demux placing a frame in another
/// processor's shard, above all — are only correct if they actually pull
/// the owning processor out of its idle park. That is invisible to a
/// single-processor fixture, so this one reports the topology the test
/// needs and keeps the IPIs for the test to assert on.
pub(crate) struct RecordingSmpCpu {
    base: TestCpu,
    current: ProcessorId,
    processors: usize,
    woken: spin::Mutex<alloc::vec::Vec<ProcessorId>>,
}

impl RecordingSmpCpu {
    pub(crate) fn new(current: u16, processors: usize) -> Self {
        assert!(processors != 0, "test CPU needs at least one processor");
        assert!(
            usize::from(current) < processors,
            "test CPU slot {current} out of range for {processors} processors"
        );
        Self {
            base: TestCpu::without_entropy(),
            current: ProcessorId::new(current),
            processors,
            woken: spin::Mutex::new(alloc::vec::Vec::new()),
        }
    }

    /// The processors this CPU was asked to wake, in order.
    pub(crate) fn woken(&self) -> alloc::vec::Vec<ProcessorId> {
        self.woken.lock().clone()
    }
}

impl Cpu for RecordingSmpCpu {
    fn current_processor(&self) -> ProcessorId {
        self.current
    }

    fn processor_count(&self) -> usize {
        self.processors
    }

    fn bootstrap_processor(&self) -> ProcessorId {
        ProcessorId::new(0)
    }

    fn park_current(&self) {
        self.base.park_current();
    }

    fn start_processor(&self, processor: ProcessorId) {
        self.base.start_processor(processor);
    }

    fn wake_processor(&self, processor: ProcessorId) {
        self.woken.lock().push(processor);
    }

    fn now(&self) -> Instant {
        self.base.now()
    }

    fn timer_frequency(&self) -> u64 {
        self.base.timer_frequency()
    }

    fn set_deadline(&self, deadline: Instant) {
        self.base.set_deadline(deadline);
    }

    fn publish_executable(&self, address: *const u8, len: usize) {
        self.base.publish_executable(address, len);
    }

    fn unpublish_executable(&self, address: *const u8, len: usize) {
        self.base.unpublish_executable(address, len);
    }

    fn native_feature_probe(&self) -> Option<fn(&str) -> Option<bool>> {
        self.base.native_feature_probe()
    }

    fn fill_entropy(&self, buffer: &mut [u8]) -> Result<EntropyQuality, EntropyUnavailable> {
        self.base.fill_entropy(buffer)
    }

    fn shutdown(&self) -> ! {
        self.base.shutdown()
    }

    fn reboot(&self) -> ! {
        self.base.reboot()
    }
}

/// Runtime state that answers the few questions a service asks during
/// construction and records nothing.
///
/// Uptime is the raw tick count, which is what every other kernel test
/// fixture does: the tests that use this assert on ordering between
/// events, never on wall time.
#[derive(Clone, Copy, Default)]
pub(crate) struct TestRuntimeState;

impl crate::component::ComponentRuntimeState for TestRuntimeState {
    fn uptime_nanos(&self, current_ticks: u64) -> u64 {
        current_ticks
    }

    fn wall_clock_offset_nanos(&self) -> i128 {
        0
    }

    fn record_console_text(&self, _: u64, _: &str) {}

    fn root_entropy(&self) -> &crate::RootEntropy {
        panic!("the network test runtime state has no root entropy")
    }

    fn memory_balloon(&self) -> Option<crate::memory::BalloonHandle> {
        None
    }

    fn profiling_enabled(&self) -> bool {
        false
    }

    fn record_profile_stack_nanos(&self, _: crate::ProfileScope, _: alloc::string::String, _: u64) {
    }

    fn record_profile_stack_parts_nanos(&self, _: crate::ProfileScope, _: &str, _: &str, _: u64) {}

    fn record_perf_metric_parts(
        &self,
        _: crate::ProfileScope,
        _: &str,
        _: &str,
        _: crate::PerfSample,
    ) {
    }
}

/// A network interface that moves no frames and does nothing but report
/// events, the way a driver's interrupt handler does.
///
/// Counters, not permits: an interface event is a broadcast fact, and
/// what the tests care about is whether a waiter that sampled the
/// counters *before* looking at its own state observes an event raised
/// in between. [`Self::complete_on`] stands in for the interrupt
/// handler; [`helios_netstack::NetworkInterface::event_mark`] is what a
/// waiter takes beforehand.
#[derive(Clone)]
pub(crate) struct RecordingNetworkInterface {
    inner: Arc<RecordingInterfaceState>,
}

struct RecordingInterfaceState {
    /// Events each queue pair has reported.
    queues: alloc::vec::Vec<AtomicU64>,
    /// Frames each queue pair is holding for the next drain, in arrival
    /// order. A test that wants to prove the kernel takes a frame off
    /// the device has to put one there first.
    pending: alloc::vec::Vec<spin::Mutex<alloc::collections::VecDeque<helios_netstack::RxFrame>>>,
    /// Events reported that belong to no queue pair.
    device: AtomicU64,
    /// Wakes whatever is parked on either counter.
    progress: crate::ProgressSignal,
    /// Whether the transmit ring takes frames. A recording interface
    /// refuses them by default so a test can read what the stack queued;
    /// one built with [`RecordingNetworkInterface::accepting_transmissions`]
    /// takes every frame and counts it instead.
    accept_transmissions: bool,
    /// Every frame the transmit ring took, headers and payload joined,
    /// in the order it took them.
    transmitted: spin::Mutex<alloc::vec::Vec<alloc::vec::Vec<u8>>>,
}

impl RecordingNetworkInterface {
    pub(crate) fn new(queue_pairs: usize) -> Self {
        Self::with_transmit_policy(queue_pairs, false)
    }

    /// An interface whose transmit ring accepts every frame offered, so a
    /// test can assert that a frame reached the device rather than that
    /// it sat in the stack's outbound queue.
    pub(crate) fn accepting_transmissions(queue_pairs: usize) -> Self {
        Self::with_transmit_policy(queue_pairs, true)
    }

    fn with_transmit_policy(queue_pairs: usize, accept_transmissions: bool) -> Self {
        assert!(queue_pairs != 0, "an interface has at least one queue pair");
        Self {
            inner: Arc::new(RecordingInterfaceState {
                queues: (0..queue_pairs).map(|_| AtomicU64::new(0)).collect(),
                pending: (0..queue_pairs)
                    .map(|_| spin::Mutex::new(alloc::collections::VecDeque::new()))
                    .collect(),
                device: AtomicU64::new(0),
                progress: crate::ProgressSignal::new(),
                accept_transmissions,
                transmitted: spin::Mutex::new(alloc::vec::Vec::new()),
            }),
        }
    }

    /// The frames the transmit ring has taken since the interface was
    /// built, each with its scatter payload appended to its headers.
    pub(crate) fn transmitted_frames(&self) -> alloc::vec::Vec<alloc::vec::Vec<u8>> {
        self.inner.transmitted.lock().clone()
    }

    /// Puts a frame in one queue pair's receive ring, where the next
    /// drain will find it, and raises the event its arrival raises.
    pub(crate) fn deliver_on(&self, queue_idx: usize, frame: &[u8]) {
        self.inner.pending[queue_idx]
            .lock()
            .push_back(helios_netstack::RxFrame::new(
                bytes::Bytes::copy_from_slice(frame),
            ));
        self.complete_on(queue_idx);
    }

    /// Raises the event one queue pair's completions raise, as the
    /// driver's interrupt handler would.
    pub(crate) fn complete_on(&self, queue_idx: usize) {
        self.inner.queues[queue_idx].fetch_add(1, Ordering::AcqRel);
        self.inner.progress.signal();
    }
}

impl RecordingInterfaceState {
    fn mark(&self, queue_idx: usize) -> helios_netstack::InterfaceEventMark {
        helios_netstack::InterfaceEventMark {
            queue: self.queues[queue_idx].load(Ordering::Acquire),
            device: self.device.load(Ordering::Acquire),
        }
    }
}

impl helios_netstack::NetworkInterface for RecordingNetworkInterface {
    fn mac_address(&self) -> [u8; 6] {
        [0x02, 0, 0, 0, 0, 1]
    }

    fn max_frame_len(&self) -> usize {
        helios_netstack::ETHERNET_FRAME_BYTES
    }

    fn queue_pair_count(&self) -> usize {
        self.inner.queues.len()
    }

    fn capabilities(&self) -> helios_netstack::InterfaceCapabilities {
        helios_netstack::InterfaceCapabilities {
            max_frame_len: helios_netstack::ETHERNET_FRAME_BYTES,
            events: helios_netstack::EventDeliveryCapabilities {
                polling: true,
                interrupts: true,
                rx_poll_budget: helios_netstack::DEFAULT_POLL_BUDGET,
                tx_completion_budget: helios_netstack::DEFAULT_POLL_BUDGET,
                ..helios_netstack::EventDeliveryCapabilities::default()
            },
            ..helios_netstack::InterfaceCapabilities::default()
        }
    }

    fn try_receive<'a>(
        &'a self,
        _: &'a mut helios_netstack::PacketBuffer,
    ) -> impl core::future::Future<Output = helios_hal::io::IoResult<bool>> + Send + 'a {
        core::future::ready(Ok(false))
    }

    fn try_receive_frame(
        &self,
    ) -> impl core::future::Future<
        Output = helios_hal::io::IoResult<Option<helios_netstack::RxFrame>>,
    > + Send {
        core::future::ready(Ok(None))
    }

    fn try_receive_frames_immediate_on<'a, 'slots>(
        &'a self,
        queue_idx: usize,
        slots: &'slots mut [Option<helios_netstack::RxFrame>],
    ) -> helios_hal::io::IoResult<Option<usize>>
    where
        'a: 'slots,
    {
        let mut pending = self.inner.pending[queue_idx].lock();
        let mut taken = 0;
        for slot in slots.iter_mut() {
            let Some(frame) = pending.pop_front() else {
                break;
            };
            *slot = Some(frame);
            taken += 1;
        }
        Ok(Some(taken))
    }

    fn repost_rx_frame<'a>(
        &'a self,
        _: helios_netstack::RxFrame,
    ) -> impl core::future::Future<Output = helios_hal::io::IoResult<()>> + Send + 'a {
        core::future::ready(Ok(()))
    }

    fn repost_rx_frames_immediate<'a, 'slots>(
        &'a self,
        _: &'slots mut [Option<helios_netstack::RxFrame>],
    ) -> helios_hal::io::IoResult<Option<()>>
    where
        'a: 'slots,
    {
        Ok(Some(()))
    }

    fn try_transmit_scatter_immediate_on(
        &self,
        _: usize,
        frames: &[helios_netstack::TxFrameRef<'_>],
    ) -> helios_hal::io::IoResult<Option<usize>> {
        if !self.inner.accept_transmissions {
            return Ok(Some(0));
        }
        let mut transmitted = self.inner.transmitted.lock();
        for frame in frames {
            let mut bytes = frame.bytes.to_vec();
            if let Some(payload) = frame.payload {
                bytes.extend_from_slice(payload);
            }
            transmitted.push(bytes);
        }
        Ok(Some(frames.len()))
    }

    fn reclaim_transmit_completions_immediate_on(
        &self,
        _: usize,
        _: usize,
    ) -> helios_hal::io::IoResult<Option<usize>> {
        Ok(Some(0))
    }

    fn event_mark(&self, queue_idx: usize) -> helios_netstack::InterfaceEventMark {
        self.inner.mark(queue_idx)
    }

    fn wait_for_event_since(
        &self,
        queue_idx: usize,
        mark: helios_netstack::InterfaceEventMark,
    ) -> impl core::future::Future<Output = ()> + Send + '_ {
        // Armed here, not at the first poll, exactly as a driver must:
        // the wake this races is raised by an interrupt handler that
        // does not wait to be observed.
        let progress = self.inner.progress.mark();
        async move {
            let changed = self.inner.progress.changed(progress);
            let mut changed = core::pin::pin!(changed);
            core::future::poll_fn(|cx| {
                if self.inner.mark(queue_idx) != mark {
                    return core::task::Poll::Ready(());
                }
                core::future::Future::poll(changed.as_mut(), cx)
            })
            .await;
        }
    }
}

/// The network service the kernel's own tests drive.
///
/// An in-memory double that models an always-ready loopback peer. It
/// lives here rather than inside one test module because several of
/// them need the same double, and one of them needs a real
/// [`crate::ComponentHostNetworkService`] built on top of it.
#[cfg(feature = "wasmtime-runtime")]
mod network {
    use alloc::vec;
    use alloc::vec::Vec;

    use bytes::Bytes;

    use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use triomphe::Arc;

    use crate::{ComponentNetworkService, SocketReadiness};

    /// The handles a [`TestNetworkService`] has been asked to retire.
    ///
    /// TCP streams and UDP sockets get one of these each, because a
    /// socket-lifetime test asserts on a count and the two protocols
    /// number their handles independently.
    #[derive(Default)]
    pub(crate) struct TestClosedStreams {
        count: AtomicUsize,
        last: AtomicU64,
    }

    impl TestClosedStreams {
        fn record(&self, stream: u64) {
            self.last.store(stream, Ordering::Release);
            self.count.fetch_add(1, Ordering::AcqRel);
        }

        pub(crate) fn count(&self) -> usize {
            self.count.load(Ordering::Acquire)
        }

        pub(crate) fn last(&self) -> u64 {
            self.last.load(Ordering::Acquire)
        }
    }

    #[derive(Clone, Default)]
    pub(crate) struct TestNetworkService {
        closed: Arc<TestClosedStreams>,
        closed_udp: Arc<TestClosedStreams>,
    }

    impl TestNetworkService {
        pub(crate) fn new() -> Self {
            Self::default()
        }

        /// The TCP retirement log this service writes to, which is
        /// what a stream-lifetime test asserts against.
        pub(crate) fn closed(&self) -> Arc<TestClosedStreams> {
            self.closed.clone()
        }

        /// The UDP retirement log, kept apart from the TCP one so a
        /// datagram-socket test counts only its own protocol.
        pub(crate) fn closed_udp_sockets(&self) -> Arc<TestClosedStreams> {
            self.closed_udp.clone()
        }
    }

    impl ComponentNetworkService for TestNetworkService {
        type TcpStream = u64;
        type TcpListener = u64;
        type UdpSocket = u64;

        // The in-memory doubles model an always-ready loopback peer.
        fn tcp_readiness(
            &self,
            _: Self::TcpStream,
        ) -> impl Future<Output = Result<SocketReadiness, crate::TcpError>> + Send + '_ {
            core::future::ready(Ok(SocketReadiness {
                readable: true,
                writable: true,
                hangup: false,
            }))
        }

        fn tcp_listener_readiness(
            &self,
            _: Self::TcpListener,
        ) -> impl Future<Output = Result<SocketReadiness, crate::TcpError>> + Send + '_ {
            core::future::ready(Ok(SocketReadiness {
                readable: true,
                writable: false,
                hangup: false,
            }))
        }

        fn udp_readiness(
            &self,
            _: Self::UdpSocket,
        ) -> impl Future<Output = Result<SocketReadiness, crate::UdpError>> + Send + '_ {
            core::future::ready(Ok(SocketReadiness {
                readable: true,
                writable: true,
                hangup: false,
            }))
        }

        fn hardware_address(&self) -> [u8; 6] {
            [2, 0, 0, 0, 0, 1]
        }

        fn ipv4_cidr(
            &self,
        ) -> impl core::future::Future<Output = Option<crate::Ipv4Cidr>> + Send + '_ {
            core::future::ready(Some(crate::Ipv4Cidr::new(
                crate::Ipv4Address::new([127, 0, 0, 1]),
                8,
            )))
        }

        fn ping(
            &self,
            _: &str,
            _: u64,
        ) -> impl core::future::Future<Output = Result<crate::PingReply, crate::PingError>> + Send + '_
        {
            core::future::ready(Ok(crate::PingReply {
                address: crate::NetworkIpAddress::Ipv4(crate::Ipv4Address::new([127, 0, 0, 1])),
                round_trip_nanos: 1,
                payload_bytes: 1,
            }))
        }

        fn dns_resolve(
            &self,
            _: &str,
            _: u64,
        ) -> impl core::future::Future<
            Output = Result<Vec<crate::NetworkIpAddress>, crate::DnsError>,
        > + Send
        + '_ {
            core::future::ready(Ok(vec![crate::NetworkIpAddress::Ipv4(
                crate::Ipv4Address::new([127, 0, 0, 1]),
            )]))
        }

        fn tcp_connect(
            &self,
            _: &str,
            _: u16,
            _: u64,
        ) -> impl core::future::Future<Output = Result<Self::TcpStream, crate::TcpError>> + Send + '_
        {
            core::future::ready(Ok(7))
        }

        fn tcp_connect_from(
            &self,
            _: &str,
            _: u16,
            local_port: u16,
            _: u8,
            _: u64,
        ) -> impl core::future::Future<Output = Result<Self::TcpStream, crate::TcpError>> + Send + '_
        {
            core::future::ready(Ok(u64::from(local_port)))
        }

        fn tcp_connect_address(
            &self,
            _: crate::NetworkIpAddress,
            _: u16,
            local_port: u16,
            _: u8,
            _: u64,
        ) -> impl core::future::Future<Output = Result<Self::TcpStream, crate::TcpError>> + Send + '_
        {
            core::future::ready(Ok(if local_port == 0 {
                7
            } else {
                u64::from(local_port)
            }))
        }

        fn tcp_listen(
            &self,
            _: crate::NetworkIpAddress,
            local_port: u16,
            _: u16,
            _: u8,
        ) -> impl core::future::Future<
            Output = Result<crate::TcpListener<Self::TcpListener>, crate::TcpError>,
        > + Send
        + '_ {
            core::future::ready(Ok(crate::TcpListener {
                listener: 8,
                local_port,
            }))
        }

        fn tcp_set_hop_limit(&self, _: Self::TcpStream, _: u8) -> Result<(), crate::TcpError> {
            Ok(())
        }

        fn tcp_listener_set_hop_limit(
            &self,
            _: Self::TcpListener,
            _: u8,
        ) -> Result<(), crate::TcpError> {
            Ok(())
        }

        fn tcp_accept(
            &self,
            listener: Self::TcpListener,
            _: u64,
        ) -> impl core::future::Future<
            Output = Result<crate::TcpAccepted<Self::TcpStream>, crate::TcpError>,
        > + Send
        + '_ {
            core::future::ready(Ok(crate::TcpAccepted {
                stream: listener + 1,
                address: crate::NetworkIpAddress::Ipv4(crate::Ipv4Address::new([127, 0, 0, 1])),
                port: 4040,
            }))
        }

        fn tcp_write_all(
            &self,
            _: Self::TcpStream,
            _: &[u8],
            _: u64,
        ) -> impl core::future::Future<Output = Result<(), crate::TcpError>> + Send + '_ {
            core::future::ready(Ok(()))
        }

        fn tcp_read(
            &self,
            _: Self::TcpStream,
            _: u32,
            _: u64,
        ) -> impl core::future::Future<Output = Result<Option<Bytes>, crate::TcpError>> + Send + '_
        {
            core::future::ready(Ok(Some(Bytes::from_static(&[4, 2]))))
        }

        fn tcp_shutdown_send(
            &self,
            _: Self::TcpStream,
        ) -> impl core::future::Future<Output = Result<(), crate::TcpError>> + Send + '_ {
            core::future::ready(Ok(()))
        }

        fn tcp_close(&self, stream: Self::TcpStream) {
            self.closed.record(stream);
        }

        fn udp_bind(
            &self,
            local_port: u16,
        ) -> impl core::future::Future<
            Output = Result<crate::UdpBinding<Self::UdpSocket>, crate::UdpError>,
        > + Send
        + '_ {
            core::future::ready(Ok(crate::UdpBinding {
                socket: 9,
                local_port,
            }))
        }

        fn udp_connect(
            &self,
            _: Self::UdpSocket,
            _: crate::NetworkIpAddress,
            _: u16,
        ) -> Result<(), crate::UdpError> {
            Ok(())
        }

        fn udp_disconnect(&self, _: Self::UdpSocket) -> Result<(), crate::UdpError> {
            Ok(())
        }

        fn udp_set_hop_limit(&self, _: Self::UdpSocket, _: u8) -> Result<(), crate::UdpError> {
            Ok(())
        }

        fn udp_send(
            &self,
            _: Self::UdpSocket,
            _: &str,
            _: u16,
            bytes: &[u8],
            _: u64,
        ) -> impl core::future::Future<Output = Result<u64, crate::UdpError>> + Send + '_ {
            let _ = bytes;
            async { panic!("a UDP send must take the typed address path") }
        }

        fn udp_send_address(
            &self,
            _: Self::UdpSocket,
            _: crate::NetworkIpAddress,
            _: u16,
            bytes: &[u8],
            _: u64,
        ) -> impl core::future::Future<Output = Result<u64, crate::UdpError>> + Send + '_ {
            core::future::ready(Ok(bytes.len() as u64))
        }

        fn udp_receive(
            &self,
            _: Self::UdpSocket,
            _: u32,
            _: u64,
        ) -> impl core::future::Future<
            Output = Result<Option<crate::UdpDatagram>, crate::UdpError>,
        > + Send
        + '_ {
            core::future::ready(Ok(None))
        }

        fn udp_join_multicast_v4(
            &self,
            _: crate::Ipv4Address,
            _: crate::Ipv4Address,
        ) -> impl core::future::Future<Output = Result<(), crate::UdpError>> + Send + '_ {
            core::future::ready(Ok(()))
        }

        fn udp_leave_multicast_v4(
            &self,
            _: crate::Ipv4Address,
            _: crate::Ipv4Address,
        ) -> impl core::future::Future<Output = Result<(), crate::UdpError>> + Send + '_ {
            core::future::ready(Ok(()))
        }

        fn udp_close(&self, socket: Self::UdpSocket) {
            self.closed_udp.record(socket);
        }
    }

    impl crate::NetworkAdminBackend for TestNetworkService {
        fn network_stats(&self) -> crate::NetworkStats {
            crate::NetworkStats::default()
        }

        fn bridge_port(
            &self,
            _: crate::NetworkPortId,
            _: crate::NetworkBridgeRequest,
        ) -> impl core::future::Future<Output = Result<(), crate::NetworkControlError>> + Send
        {
            core::future::ready(Err(crate::NetworkControlError::BridgeUnavailable))
        }

        fn unbridge_port(
            &self,
            _: crate::NetworkPortId,
        ) -> impl core::future::Future<Output = Result<(), crate::NetworkControlError>> + Send
        {
            core::future::ready(Err(crate::NetworkControlError::BridgeUnavailable))
        }

        fn acquire_dhcp(
            &self,
            _: crate::NetworkPortId,
        ) -> impl core::future::Future<Output = Result<crate::Ipv4Cidr, crate::NetworkControlError>> + Send
        {
            core::future::ready(Ok(crate::Ipv4Cidr::new(
                crate::Ipv4Address::new([127, 0, 0, 1]),
                8,
            )))
        }

        fn add_address(
            &self,
            _: crate::NetworkPortId,
            _: crate::Ipv4Cidr,
        ) -> impl core::future::Future<Output = Result<(), crate::NetworkControlError>> + Send
        {
            core::future::ready(Ok(()))
        }

        fn remove_address(
            &self,
            _: crate::NetworkPortId,
            _: crate::Ipv4Cidr,
        ) -> impl core::future::Future<Output = Result<(), crate::NetworkControlError>> + Send
        {
            core::future::ready(Ok(()))
        }

        fn clear_addresses(
            &self,
            _: crate::NetworkPortId,
        ) -> impl core::future::Future<Output = Result<(), crate::NetworkControlError>> + Send
        {
            core::future::ready(Ok(()))
        }

        fn list_addresses(
            &self,
            _: crate::NetworkPortId,
        ) -> impl core::future::Future<
            Output = Result<Vec<crate::Ipv4Cidr>, crate::NetworkControlError>,
        > + Send {
            core::future::ready(Ok(vec![crate::Ipv4Cidr::new(
                crate::Ipv4Address::new([127, 0, 0, 1]),
                8,
            )]))
        }

        fn mac_address(
            &self,
            _: crate::NetworkPortId,
        ) -> impl core::future::Future<
            Output = Result<crate::MacAddress, crate::NetworkControlError>,
        > + Send {
            core::future::ready(Ok(crate::MacAddress::new([2, 0, 0, 0, 0, 1])))
        }

        fn set_gateway(
            &self,
            _: crate::NetworkPortId,
            _: crate::Ipv4Address,
        ) -> impl core::future::Future<Output = Result<(), crate::NetworkControlError>> + Send
        {
            core::future::ready(Ok(()))
        }

        fn add_route(
            &self,
            _: crate::NetworkPortId,
            _: crate::Ipv4Route,
        ) -> impl core::future::Future<Output = Result<(), crate::NetworkControlError>> + Send
        {
            core::future::ready(Ok(()))
        }

        fn remove_route(
            &self,
            _: crate::NetworkPortId,
            _: crate::Ipv4Route,
        ) -> impl core::future::Future<Output = Result<(), crate::NetworkControlError>> + Send
        {
            core::future::ready(Ok(()))
        }

        fn clear_routes(
            &self,
            _: crate::NetworkPortId,
        ) -> impl core::future::Future<Output = Result<(), crate::NetworkControlError>> + Send
        {
            core::future::ready(Ok(()))
        }

        fn list_routes(
            &self,
            _: crate::NetworkPortId,
        ) -> impl core::future::Future<
            Output = Result<Vec<crate::Ipv4Route>, crate::NetworkControlError>,
        > + Send {
            core::future::ready(Ok(vec![crate::Ipv4Route::new(
                crate::Ipv4Cidr::new(crate::Ipv4Address::new([0, 0, 0, 0]), 0),
                crate::Ipv4Address::new([127, 0, 0, 1]),
            )]))
        }
    }
}

#[cfg(feature = "wasmtime-runtime")]
pub(crate) use network::{TestClosedStreams, TestNetworkService};

/// A [`crate::ComponentHostNetworkService`] over a fresh
/// [`TestNetworkService`], for a test that does not inspect what the
/// service was asked to retire.
#[cfg(feature = "wasmtime-runtime")]
pub(crate) fn test_network_service() -> crate::ComponentHostNetworkService {
    crate::ComponentHostNetworkService::from_service(TestNetworkService::new())
}

/// The same, paired with the log of the streams it retires.
#[cfg(feature = "wasmtime-runtime")]
pub(crate) fn recording_network_service() -> (
    crate::ComponentHostNetworkService,
    triomphe::Arc<TestClosedStreams>,
) {
    let service = TestNetworkService::new();
    let closed = service.closed();
    (
        crate::ComponentHostNetworkService::from_service(service),
        closed,
    )
}

/// The same again, paired with the log of the datagram sockets it
/// retires.
#[cfg(feature = "wasmtime-runtime")]
pub(crate) fn recording_udp_network_service() -> (
    crate::ComponentHostNetworkService,
    triomphe::Arc<TestClosedStreams>,
) {
    let service = TestNetworkService::new();
    let closed = service.closed_udp_sockets();
    (
        crate::ComponentHostNetworkService::from_service(service),
        closed,
    )
}
