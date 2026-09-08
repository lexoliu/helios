use std::sync::Arc;
use std::thread;

use helios_hal::cpu::{Cpu, Instant, ProcessorId};
use helios_hal::entropy::{EntropyQuality, EntropyUnavailable};

use crate::runtime::HostedMachine;

/// Hosted CPU adapter that exposes one OS thread as one logical processor.
///
/// The processor id is fixed when the thread starts, which makes
/// `current_processor()` trivial and avoids any TLS lookup in the hot path.
#[derive(Clone)]
pub struct HostedCpu {
    processor: ProcessorId,
    machine: Arc<HostedMachine>,
}

impl HostedCpu {
    pub fn new(processor: ProcessorId, machine: Arc<HostedMachine>) -> Self {
        set_current_processor(processor);
        Self { processor, machine }
    }
}

thread_local! {
    /// This thread's logical processor, seeded when the thread's
    /// [`HostedCpu`] is built and read by the identity contract below.
    static CURRENT_PROCESSOR: core::cell::Cell<Option<ProcessorId>> =
        const { core::cell::Cell::new(None) };
}

/// Declares which logical processor the calling thread stands in for.
///
/// The bare-metal backends seed a register in their boot path; a hosted
/// processor is an OS thread, so it seeds a thread-local instead. A
/// thread that runs kernel code without doing this is a bug in the
/// hosted machine's thread setup, and the contract below says so rather
/// than answering with processor zero.
pub fn set_current_processor(processor: ProcessorId) {
    CURRENT_PROCESSOR.with(|slot| slot.set(Some(processor)));
}

/// The processor identity `hal` publishes as a linkage contract.
#[unsafe(no_mangle)]
extern "Rust" fn helios_current_processor() -> ProcessorId {
    CURRENT_PROCESSOR.with(|slot| {
        slot.get()
            .expect("this thread runs kernel code without a hosted processor identity")
    })
}

impl Cpu for HostedCpu {
    fn processor_count(&self) -> usize {
        self.machine.processor_count()
    }

    fn bootstrap_processor(&self) -> ProcessorId {
        self.machine.bootstrap_processor()
    }

    fn park_current(&self) {
        thread::park();
    }

    fn start_processor(&self, processor: ProcessorId) {
        self.machine.start_processor(processor);
    }

    fn wake_processor(&self, processor: ProcessorId) {
        self.machine.wake_processor(processor);
    }

    fn now(&self) -> Instant {
        self.machine.now()
    }

    fn timer_frequency(&self) -> u64 {
        self.machine.timer_frequency()
    }

    fn set_deadline(&self, deadline: Instant) {
        self.machine.set_deadline(self.processor, deadline);
    }

    fn publish_executable(&self, _ptr: *const u8, _len: usize) {}

    fn unpublish_executable(&self, _ptr: *const u8, _len: usize) {}

    fn native_feature_probe(&self) -> Option<fn(&str) -> Option<bool>> {
        None
    }

    fn fill_entropy(&self, buffer: &mut [u8]) -> Result<EntropyQuality, EntropyUnavailable> {
        getrandom::fill(buffer)
            .unwrap_or_else(|error| panic!("host entropy source failed: {error}"));
        Ok(EntropyQuality::Cryptographic)
    }

    fn has_lazy_commit_virtual_memory(&self) -> bool {
        // Hosted runs as a regular host process: libc `mmap(PROT_
        // NONE)` reserves arbitrary virtual ranges with no physical
        // commit until the host kernel page-faults a touched page
        // back in. That is the exact "petabyte-scale lazy-commit"
        // capability the trait talks about — kernel-side runtimes
        // such as the Wasmtime pooling allocator can pre-reserve
        // 4 GiB+ per instance without burning RAM.
        true
    }

    fn shutdown(&self) -> ! {
        std::process::exit(0)
    }

    fn reboot(&self) -> ! {
        std::process::exit(1)
    }
}
