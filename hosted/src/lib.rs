use std::fmt::{self, Write};
use std::time::Instant as StdInstant;

use helios_hal::Platform;
use helios_hal::cpu::{Cpu, HartId, Instant};
use helios_hal::memory::MemoryRegion;
use helios_kernel::init;

const HOSTED_HEAP_SIZE: usize = 16 * 1024 * 1024;

struct HostedCpu {
    started_at: StdInstant,
}

impl HostedCpu {
    fn new() -> Self {
        Self {
            started_at: StdInstant::now(),
        }
    }
}

impl Cpu for HostedCpu {
    fn current_hart(&self) -> HartId {
        HartId::new(0)
    }

    fn hart_count(&self) -> usize {
        1
    }

    fn bootstrap_hart(&self) -> HartId {
        HartId::new(0)
    }

    fn park_current(&self) {
        std::thread::park();
    }

    fn unpark(&self, _hart: HartId) {}

    fn now(&self) -> Instant {
        Instant::new(self.started_at.elapsed().as_nanos() as u64)
    }

    fn set_deadline(&self, _deadline: Instant) {}

    fn shutdown(&self) -> ! {
        std::process::exit(0);
    }

    fn reboot(&self) -> ! {
        std::process::exit(1);
    }
}

struct StdoutConsole;

impl Write for StdoutConsole {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        use std::io::Write as _;

        let mut stdout = std::io::stdout().lock();
        stdout.write_all(s.as_bytes()).map_err(|_| fmt::Error)?;
        stdout.flush().map_err(|_| fmt::Error)
    }
}

pub fn main() {
    std::panic::set_hook(Box::new(|info| {
        let message = info.payload_as_str().unwrap_or("non-string panic payload");
        helios_kernel::panic_log_message(message, info.location());
    }));

    let heap = vec![0; HOSTED_HEAP_SIZE].into_boxed_slice();
    let heap = Box::leak(heap);
    let memory_regions = [MemoryRegion::from(heap)];
    init(Platform::new(
        StdoutConsole,
        memory_regions,
        HostedCpu::new(),
    ));
}
