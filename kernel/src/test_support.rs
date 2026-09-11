//! Fixtures the kernel's own unit tests share.
//!
//! A platform value is the first thing most kernel code asks for, and a
//! test that needs one should not have to spell a whole `Cpu`
//! implementation out again.

use core::sync::atomic::{AtomicU64, Ordering};

use helios_hal::cpu::{Cpu, Instant, ProcessorId};

/// The processor-local interrupt mask for the kernel's own unit tests.
///
/// `with_local_interrupts_masked` reaches its implementation by
/// linkage, so a binary that links this crate has to install one. A
/// bare-metal backend installs its `InterruptOps`; a test binary runs
/// as a host process, where nothing preempts a thread and then
/// allocates, so there is nothing to mask and the spin lock inside each
/// `IrqSafeMutex` is the whole exclusion. This is the same arrangement
/// the `critical-section` dev-dependency's `std` feature provides for
/// `critical_section::with` here.
struct TestInterruptMask;

impl helios_hal::critical_section::LocalInterruptMask for TestInterruptMask {
    fn mask() -> bool {
        false
    }

    unsafe fn restore(was_enabled: bool) {
        debug_assert!(
            !was_enabled,
            "the host test mask never reports interrupts as enabled"
        );
    }
}

helios_hal::critical_section::set_local_interrupt_mask_impl!(TestInterruptMask);
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
        crate::test_processor_identity::set(ProcessorId::new(current));
        Self {
            base: TestCpu::without_entropy(),
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

/// A detached [`crate::ProfileSink`] for a subsystem under test.
///
/// Nothing reads it back; the histories exist so the subsystem's own
/// record calls run the same path they run on a live kernel.
pub(crate) fn test_profile_sink() -> crate::ProfileSink {
    crate::ProfileSink::new(
        crate::DEFAULT_PROFILE_STACK_CAPACITY,
        crate::DEFAULT_PERF_METRIC_CAPACITY,
    )
}

/// The uptime clock a test service reads, at a 1 GHz timebase whose
/// origin is the boot tick, so a tick is a nanosecond and a test can
/// state deadlines in either.
pub(crate) fn test_uptime_clock() -> crate::UptimeClock {
    crate::UptimeClock::new(0, 1_000_000_000)
}

/// Runtime state that answers the few questions a store asks and
/// records nothing.
///
/// Uptime is the raw tick count, which is what every other kernel test
/// fixture does: the tests that use this assert on ordering between
/// events, never on wall time. The network service is the one a
/// store's socket retirements are closed through, so a test that
/// drives a store's teardown can watch what it closed.
#[cfg(feature = "wasmtime-runtime")]
#[derive(Clone, Default)]
pub(crate) struct TestRuntimeState {
    network: Option<TestNetworkService>,
}

#[cfg(feature = "wasmtime-runtime")]
impl TestRuntimeState {
    pub(crate) fn with_network(network: TestNetworkService) -> Self {
        Self {
            network: Some(network),
        }
    }
}

#[cfg(feature = "wasmtime-runtime")]
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

    /// A test runtime publishes no devices; the registry is empty
    /// and every claim through it reports the device is not there.
    fn device_grants(&self) -> &crate::device::DeviceGrantRegistry {
        static EMPTY: crate::device::DeviceGrantRegistry =
            crate::device::DeviceGrantRegistry::new();
        &EMPTY
    }

    /// A test machine has no display device, and a claim on one is
    /// refused rather than trapping.
    fn display_service(&self) -> Option<crate::display::DisplayService> {
        None
    }

    fn input_service(&self) -> Option<crate::input::InputService> {
        None
    }

    fn gpu3d_service(&self) -> Option<crate::gpu::Gpu3dService> {
        None
    }

    fn surface_service(&self) -> crate::surface::SurfaceService {
        crate::surface::SurfaceService::new()
    }

    fn audio_service(&self) -> Option<crate::audio::AudioService> {
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

    fn retire_network_handles(&self, retired: &crate::SocketRetirementQueue) {
        match self.network.as_ref() {
            Some(service) => {
                crate::retire_queued_handles(retired, service);
            }
            None => assert!(
                retired.is_empty(),
                "a network-less test runtime state was handed a socket to retire"
            ),
        }
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

/// What one queue pair hands the next drain, in the order the device
/// produced it.
enum PendingReceive {
    /// A frame the drain takes off the ring.
    Frame(helios_netstack::RxFrame),
    /// A completion the driver refuses. The drain stops there and
    /// reports the error beside the frames it has already taken, which
    /// is what a virtio-net chain this driver cannot reconstruct does.
    Refusal(helios_hal::io::IoError),
}

struct RecordingInterfaceState {
    /// Events each queue pair has reported.
    queues: alloc::vec::Vec<AtomicU64>,
    /// What each queue pair is holding for the next drain, in arrival
    /// order. A test that wants to prove the kernel takes a frame off
    /// the device has to put one there first.
    pending: alloc::vec::Vec<spin::Mutex<alloc::collections::VecDeque<PendingReceive>>>,
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
            .push_back(PendingReceive::Frame(helios_netstack::RxFrame::new(
                bytes::Bytes::copy_from_slice(frame),
            )));
        self.complete_on(queue_idx);
    }

    /// Puts a completion the driver refuses behind whatever one queue
    /// pair is already holding, so a test can drain a batch of good
    /// frames into a refusal the way a malformed mergeable chain
    /// arrives behind good ones.
    pub(crate) fn refuse_on(&self, queue_idx: usize, error: helios_hal::io::IoError) {
        self.inner.pending[queue_idx]
            .lock()
            .push_back(PendingReceive::Refusal(error));
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
    ) -> Option<helios_netstack::RxDrain>
    where
        'a: 'slots,
    {
        let mut pending = self.inner.pending[queue_idx].lock();
        let mut received = 0;
        for slot in slots.iter_mut() {
            match pending.pop_front() {
                Some(PendingReceive::Frame(frame)) => {
                    *slot = Some(frame);
                    received += 1;
                }
                Some(PendingReceive::Refusal(error)) => {
                    return Some(helios_netstack::RxDrain::refused(received, error));
                }
                None => break,
            }
        }
        Some(helios_netstack::RxDrain::completed(received))
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
/// them need the same double, and the component host's own tests
/// substitute it for the machine's network service.
#[cfg(feature = "wasmtime-runtime")]
mod network {
    use alloc::vec;
    use alloc::vec::Vec;

    use bytes::Bytes;

    use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

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
        closed_listeners: Arc<TestClosedStreams>,
        /// The failure switch a stream test arms: once set, the calls a
        /// device poll sits behind — `tcp_read`, `tcp_write_all_bytes`,
        /// `tcp_write_ready` — fail the way a device fault fails them,
        /// while the synchronous probes keep answering: `tcp_try_read`
        /// still drains and the send side reports a full queue, since a
        /// device that cannot be polled also cannot drain one.
        drive_failure: Arc<AtomicBool>,
        /// Bytes `tcp_write_all` accepted — the evidence a parked batch
        /// was retried rather than dropped.
        bytes_written: Arc<AtomicU64>,
    }

    impl TestNetworkService {
        pub(crate) fn new() -> Self {
            Self::default()
        }

        /// Arm the driving calls to fail — see `drive_failure`.
        pub(crate) fn fail_drives(&self) {
            self.drive_failure.store(true, Ordering::Release);
        }

        /// Disarm the switch, so a retried write can show what a parked
        /// batch kept.
        pub(crate) fn heal_drives(&self) {
            self.drive_failure.store(false, Ordering::Release);
        }

        /// Total bytes `tcp_write_all` has accepted.
        pub(crate) fn bytes_written(&self) -> u64 {
            self.bytes_written.load(Ordering::Acquire)
        }

        /// The error a real device fault surfaces as from a driving
        /// call, when the switch is armed.
        fn drive_error(&self) -> Option<crate::TcpError> {
            self.drive_failure
                .load(Ordering::Acquire)
                .then_some(crate::TcpError {
                    kind: crate::TcpErrorKind::Internal,
                    detail: crate::NetworkErrorDetail::VirtioAdvanceFailed,
                })
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

        /// The listener retirement log, kept apart from the stream one
        /// because a listener and the connections it accepted are
        /// separate handles with separate lifetimes.
        pub(crate) fn closed_listeners(&self) -> Arc<TestClosedStreams> {
            self.closed_listeners.clone()
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
            bytes: &[u8],
            _: u64,
        ) -> impl core::future::Future<Output = Result<(), crate::TcpError>> + Send + '_ {
            let written = bytes.len() as u64;
            let failure = self.drive_error();
            core::future::ready(match failure {
                Some(error) => Err(error),
                None => {
                    self.bytes_written.fetch_add(written, Ordering::AcqRel);
                    Ok(())
                }
            })
        }

        fn tcp_read(
            &self,
            _: Self::TcpStream,
            _: u32,
            _: u64,
        ) -> impl core::future::Future<Output = Result<Option<Bytes>, crate::TcpError>> + Send + '_
        {
            let failure = self.drive_error();
            core::future::ready(match failure {
                Some(error) => Err(error),
                None => Ok(Some(Bytes::from_static(&[4, 2]))),
            })
        }

        fn tcp_try_read(
            &self,
            _: Self::TcpStream,
            _: usize,
        ) -> Result<crate::TcpReadProgress, crate::TcpError> {
            Ok(crate::TcpReadProgress::Data(Bytes::from_static(&[4, 2])))
        }

        fn tcp_send_room(
            &self,
            _: Self::TcpStream,
        ) -> Result<crate::TcpWriteProgress, crate::TcpError> {
            Ok(match self.drive_failure.load(Ordering::Acquire) {
                // A device whose polls fail drains nothing: the send
                // queue reads as full.
                true => crate::TcpWriteProgress::Pending,
                false => crate::TcpWriteProgress::Room(usize::MAX),
            })
        }

        fn tcp_try_write(
            &self,
            _: Self::TcpStream,
            bytes: &mut Bytes,
        ) -> Result<usize, crate::TcpError> {
            if self.drive_failure.load(Ordering::Acquire) {
                return Ok(0);
            }
            let written = bytes.len();
            bytes.clear();
            Ok(written)
        }

        fn tcp_write_ready(
            &self,
            _: Self::TcpStream,
        ) -> impl core::future::Future<Output = Result<(), crate::TcpError>> + Send + '_ {
            let failure = self.drive_error();
            core::future::ready(match failure {
                Some(error) => Err(error),
                None => Ok(()),
            })
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

        fn tcp_listener_close(&self, listener: Self::TcpListener) {
            self.closed_listeners.record(listener);
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

/// A fresh [`TestNetworkService`], for a test that does not inspect
/// what the service was asked to retire.
#[cfg(feature = "wasmtime-runtime")]
pub(crate) fn test_network_service() -> TestNetworkService {
    TestNetworkService::new()
}

/// The same, paired with the log of the streams it retires.
#[cfg(feature = "wasmtime-runtime")]
pub(crate) fn recording_network_service() -> (TestNetworkService, triomphe::Arc<TestClosedStreams>)
{
    let service = TestNetworkService::new();
    let closed = service.closed();
    (service, closed)
}

/// A store's end of the socket retirement queue, for a test with no
/// store around it.
///
/// A component's socket resource holds no service: it queues what it
/// owns when it dies, and the store closes it on its next turn. A
/// lifetime test therefore asserts on both halves — the drop queued the
/// id, and the drain closed it through this service — and drains
/// through [`crate::retire_queued_handles`], the same function the live
/// kernel's runtime state calls.
#[cfg(feature = "wasmtime-runtime")]
pub(crate) struct TestSocketRetirement {
    queue: crate::SocketRetirementQueue,
    service: TestNetworkService,
}

#[cfg(feature = "wasmtime-runtime")]
impl TestSocketRetirement {
    pub(crate) fn new(service: TestNetworkService) -> Self {
        Self {
            queue: crate::SocketRetirementQueue::new(),
            service,
        }
    }

    /// The sender a socket resource this fixture stands behind holds.
    pub(crate) fn sender(&self) -> crate::SocketRetirementSender {
        self.queue.sender()
    }

    /// Whether a dying socket has queued anything the store has yet to
    /// close.
    pub(crate) fn queued(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Closes everything queued, answering how many handles it closed.
    pub(crate) fn drain(&self) -> usize {
        crate::retire_queued_handles(&self.queue, &self.service)
    }
}

/// The same again, paired with the log of the listeners it retires.
#[cfg(feature = "wasmtime-runtime")]
pub(crate) fn recording_listener_network_service()
-> (TestNetworkService, triomphe::Arc<TestClosedStreams>) {
    let service = TestNetworkService::new();
    let closed = service.closed_listeners();
    (service, closed)
}

/// The same again, paired with the log of the datagram sockets it
/// retires.
#[cfg(feature = "wasmtime-runtime")]
pub(crate) fn recording_udp_network_service()
-> (TestNetworkService, triomphe::Arc<TestClosedStreams>) {
    let service = TestNetworkService::new();
    let closed = service.closed_udp_sockets();
    (service, closed)
}
