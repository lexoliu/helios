//! Timer allocation baseline.
//!
//! `Sleep::poll` now acquires a refcounted slab slot inside
//! `TimerShared` on the first pending poll. The bench captures the
//! allocation profile of the pool-backed path so the old
//! per-`Sleep` heap allocation does not return.
//!
//! Workload shape:
//!
//!   * `sleep_create_only` — construct a Sleep but never poll. Models
//!     timeout candidates that lose a select/race before reaching a
//!     pending state. These already do not allocate today.
//!   * `sleep_first_poll_pending` — poll once with a non-zero
//!     deadline and a noop waker so the future returns Pending. This
//!     exercises pool acquisition. The divan allocator profile shows
//!     per-iteration bytes / count.
//!   * `sleep_register_then_drop` — first-pending poll, then drop the
//!     future without firing. Mirrors a TCP retransmit timer that is
//!     cancelled when an ACK arrives in time. This path must stay
//!     zero-allocation after warm pool setup.

extern crate alloc;

use alloc::string::String;
use core::future::Future;
use core::pin::Pin;
use core::task::Context;

use divan::counter::ItemsCount;
use divan::{AllocProfiler, Bencher, black_box};
use futures::task::noop_waker_ref;
use helios_hal::cpu::{Cpu, HardwarePerfCounters, Instant, ProcessorId};
use helios_kernel::Timer;

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

fn main() {
    divan::main();
}

#[derive(Clone)]
struct BenchCpu;

impl Cpu for BenchCpu {
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

    fn start_processor(&self, _processor: ProcessorId) {}

    fn wake_processor(&self, _processor: ProcessorId) {}

    fn now(&self) -> Instant {
        Instant::new(0)
    }

    fn timer_frequency(&self) -> u64 {
        1_000_000
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
    let timer = Timer::new(BenchCpu);
    bencher.counter(ItemsCount::new(count)).bench_local(|| {
        for _ in 0..count {
            black_box(timer.sleep_for(core::time::Duration::from_micros(500)));
        }
    });
}

#[divan::bench(args = [1usize, 64, 1024])]
fn sleep_first_poll_pending(bencher: Bencher, count: usize) {
    let timer = Timer::new(BenchCpu);
    bencher.counter(ItemsCount::new(count)).bench_local(|| {
        let mut waker = Context::from_waker(noop_waker_ref());
        for _ in 0..count {
            let mut sleep = timer.sleep_for(core::time::Duration::from_secs(1));
            // SAFETY: the Sleep is owned by this stack frame for the
            // duration of the poll call.
            let pinned = unsafe { Pin::new_unchecked(&mut sleep) };
            let _ = pinned.poll(&mut waker);
            black_box(sleep);
        }
        // Drain the timer inbox into the wheel so the bounded
        // inbox never fills across iterations. With `now()` pinned
        // at 0 no entry actually fires; this just resolves the
        // pending submissions.
        timer.fire_expired();
    });
}

#[divan::bench(args = [1usize, 64, 1024])]
fn sleep_register_then_drop(bencher: Bencher, count: usize) {
    let timer = Timer::new(BenchCpu);
    bencher.counter(ItemsCount::new(count)).bench_local(|| {
        let mut waker = Context::from_waker(noop_waker_ref());
        for _ in 0..count {
            let mut sleep = timer.sleep_for(core::time::Duration::from_secs(1));
            let pinned = unsafe { Pin::new_unchecked(&mut sleep) };
            let _ = pinned.poll(&mut waker);
            // Drop the sleep before its deadline. Phase 3.1 must
            // return its slot to the pool here without leaking.
            drop(sleep);
        }
        timer.fire_expired();
    });
}

// Keeps the linker from stripping String which the AllocProfiler
// expects from divan's bench harness on first allocation.
#[allow(dead_code)]
fn _force_string_link() -> String {
    String::from("link-anchor")
}
