use std::sync::Arc;
use std::thread;

use helios_hal::cpu::{Cpu, Instant, ProcessorId};

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
        Self { processor, machine }
    }
}

impl Cpu for HostedCpu {
    fn current_processor(&self) -> ProcessorId {
        self.processor
    }

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

    fn shutdown(&self) -> ! {
        std::process::exit(0)
    }

    fn reboot(&self) -> ! {
        std::process::exit(1)
    }
}
