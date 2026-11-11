//! Timer allocation baseline.
//!
//! A pending `Sleep` carries its own wheel node, so its first pending
//! poll links memory the sleep already is into its timer's wheel. The
//! bench captures the allocation profile of that path so a per-`Sleep`
//! heap allocation does not return.
//!
//! Workload shape:
//!
//!   * `sleep_create_only` — construct a Sleep but never poll. Models
//!     timeout candidates that lose a select/race before reaching a
//!     pending state.
//!   * `sleep_first_poll_pending` — poll once with a non-zero
//!     deadline and a noop waker so the future returns Pending, which
//!     registers it, then drop it. The divan allocator profile shows
//!     per-iteration bytes / count.
//!   * `sleep_register_then_drop` — first-pending poll, then drop the
//!     future without firing. Mirrors a TCP retransmit timer that is
//!     cancelled when an ACK arrives in time: the drop unlinks the node
//!     at once and leaves nothing for a later fire.
//!
//! Every path here must stay zero-allocation.

extern crate alloc;

use alloc::string::String;
use core::future::Future;
use core::pin::pin;
use core::task::Context;

use divan::counter::ItemsCount;
use divan::{AllocProfiler, Bencher, black_box};
use futures::task::noop_waker_ref;
use helios_hal::cpu::{Cpu, HardwarePerfCounters, Instant, ProcessorId, Timebase, TimerInterrupt};
use helios_hal::watchdog::ProgressCounter;
use helios_kernel::{Executor, Timer};

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

fn main() {
    divan::main();
}

#[unsafe(no_mangle)]
extern "Rust" fn helios_current_processor() -> ProcessorId {
    ProcessorId::new(0)
}

/// The kernel timer masks local interrupts around its deadline record;
/// a benchmark runs as a host process, where nothing interrupts a
/// thread, so there is nothing to mask.
struct BenchInterruptMask;

impl helios_hal::critical_section::LocalInterruptMask for BenchInterruptMask {
    fn mask() -> bool {
        false
    }

    unsafe fn restore(_was_enabled: bool) {}
}

helios_hal::critical_section::set_local_interrupt_mask_impl!(BenchInterruptMask);

/// Processor 0's timer, as its executor builds it.
fn bench_timer() -> Timer<BenchCpu> {
    Executor::new(ProgressCounter::new(), 1, ProcessorId::new(0)).timer(BenchCpu::new())
}

#[derive(Clone)]
struct BenchCpu {
    timebase: Timebase,
}

impl BenchCpu {
    fn new() -> Self {
        Self {
            timebase: Timebase::new(1_000_000),
        }
    }
}

impl Cpu for BenchCpu {
    const TIMER_INTERRUPT: TimerInterrupt = TimerInterrupt::Deadline;

    fn processor_count(&self) -> usize {
        1
    }

    fn bootstrap_processor(&self) -> ProcessorId {
        ProcessorId::new(0)
    }

    fn park_current(&self) {}

    fn start_processor(&self, _processor: ProcessorId) {}

    fn wake_processor(&self, _processor: ProcessorId) {}

    fn now(&self) -> Instant {
        Instant::new(0)
    }

    fn timebase(&self) -> Timebase {
        self.timebase
    }

    fn hardware_perf_counters(&self) -> HardwarePerfCounters {
        HardwarePerfCounters::default()
    }

    fn set_deadline(&self, _deadline: Instant) {}

    fn publish_executable(&self, _ptr: *const u8, _len: usize) {}

    fn unpublish_executable(&self, _ptr: *const u8, _len: usize) {}

    fn native_feature_probe(&self) -> Option<fn(&str) -> Option<bool>> {
        None
    }

    fn shutdown(&self) -> ! {
        panic!("benchmark CPU cannot shut down")
    }

    fn reboot(&self) -> ! {
        panic!("benchmark CPU cannot reboot")
    }
}

#[divan::bench(args = [1usize, 64, 1024])]
fn sleep_create_only(bencher: Bencher, count: usize) {
    let timer = bench_timer();
    bencher.counter(ItemsCount::new(count)).bench_local(|| {
        for _ in 0..count {
            black_box(timer.sleep_for(core::time::Duration::from_micros(500)));
        }
    });
}

#[divan::bench(args = [1usize, 64, 1024])]
fn sleep_first_poll_pending(bencher: Bencher, count: usize) {
    let timer = bench_timer();
    bencher.counter(ItemsCount::new(count)).bench_local(|| {
        let mut waker = Context::from_waker(noop_waker_ref());
        for _ in 0..count {
            let mut sleep = pin!(timer.sleep_for(core::time::Duration::from_secs(1)));
            let _ = sleep.as_mut().poll(&mut waker);
            black_box(sleep);
        }
    });
}

#[divan::bench(args = [1usize, 64, 1024])]
fn sleep_register_then_drop(bencher: Bencher, count: usize) {
    let timer = bench_timer();
    bencher.counter(ItemsCount::new(count)).bench_local(|| {
        let mut waker = Context::from_waker(noop_waker_ref());
        for _ in 0..count {
            // Dropped at the end of the iteration, before its deadline:
            // the drop unlinks it from the wheel.
            let sleep = pin!(timer.sleep_for(core::time::Duration::from_secs(1)));
            let _ = sleep.poll(&mut waker);
        }
    });
}

// Keeps the linker from stripping String which the AllocProfiler
// expects from divan's bench harness on first allocation.
#[allow(dead_code)]
fn _force_string_link() -> String {
    String::from("link-anchor")
}
