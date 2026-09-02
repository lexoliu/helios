//! Fixtures the kernel's own unit tests share.
//!
//! A platform value is the first thing most kernel code asks for, and a
//! test that needs one should not have to spell a whole `Cpu`
//! implementation out again.

use helios_hal::cpu::{Cpu, Instant, ProcessorId};
use helios_hal::entropy::{EntropyQuality, EntropyUnavailable};

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
