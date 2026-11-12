//! The boot report of which memory the machine tag-checks.
//!
//! # Concurrency contract
//!
//! The report is read-only and runs once, on the bootstrap processor, after
//! every other processor has been started. A backend enables checking on a
//! processor before [`Cpu::start_processor`] returns for it, so the line is
//! emitted after every processor's enable. The only input is the backend's
//! [`MemoryTagging`], fixed before the `Cpu` reached the kernel. Nothing here
//! takes a lock or reaches another processor.

use helios_hal::cpu::Cpu;
use helios_hal::tagging::{MemoryTagging, TagChecking};

/// What the boot report says of a backend's [`MemoryTagging`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Report {
    /// Every processor checks tags synchronously.
    Enabled { granule_bytes: usize },
    /// The backend enabled nothing.
    NotEnabled,
}

impl Report {
    fn of(tagging: MemoryTagging) -> Self {
        match tagging.checking {
            Some(TagChecking { granule_bytes }) => Self::Enabled { granule_bytes },
            None => Self::NotEnabled,
        }
    }
}

/// Emits the boot line for the memory tagging `cpu` enabled.
pub(crate) fn log<CpuImpl: Cpu>(cpu: &CpuImpl) {
    match Report::of(cpu.memory_tagging()) {
        Report::Enabled { granule_bytes } => tracing::info!(
            granule_bytes,
            checks = "synchronous",
            "memory tagging enabled on every processor"
        ),
        Report::NotEnabled => tracing::info!("memory tagging not enabled by this backend"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_reported_not_enabled() {
        assert_eq!(Report::of(MemoryTagging::NONE), Report::NotEnabled);
    }

    #[test]
    fn checking_is_reported_enabled_with_its_granule() {
        let tagging = MemoryTagging {
            checking: Some(TagChecking { granule_bytes: 16 }),
        };
        assert_eq!(Report::of(tagging), Report::Enabled { granule_bytes: 16 });
    }

    /// The test processor enables nothing, so it takes the default.
    #[test]
    fn a_backend_that_overrides_nothing_enables_nothing() {
        let cpu = crate::test_support::TestCpu::without_entropy();
        assert_eq!(cpu.memory_tagging(), MemoryTagging::NONE);
        assert_eq!(Report::of(cpu.memory_tagging()), Report::NotEnabled);
    }
}
