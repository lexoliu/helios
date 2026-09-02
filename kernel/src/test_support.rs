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
