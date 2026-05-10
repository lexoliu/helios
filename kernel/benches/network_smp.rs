//! SMP scaling baseline for the in-kernel network service.
//!
//! The current production architecture (`kernel/src/network_service.rs`)
//! wraps all socket state in a single `SpinMutex<NetworkState>`. Every
//! TCP/UDP/DNS operation funnels through that lock, so on multi-core
//! systems aggregate throughput plateaus or regresses with concurrency.
//!
//! These benches exercise the contention shape directly without
//! standing up a full kernel: a tiny "socket op" (a contended-state
//! mutation that approximates the lock-section cost of the real
//! `Stack::take_udp` / `Stack::send_udp_ipv4` paths) is invoked from
//! `N` std threads against three concurrency primitives:
//!
//!   * `single_mutex` — one `SpinMutex` shared by every thread.
//!     Models the existing architecture; aggregate ops/sec is bounded
//!     by lock-acquisition latency and does not scale with `N`.
//!   * `sharded_mutex` — `N` independent `SpinMutex`es, one per
//!     thread, with the work permuted so each thread always hits its
//!     local shard. Models the shared-nothing architecture
//!     (Phase 4.1) where each socket's lifetime is tied to a single
//!     CPU; aggregate ops/sec scales near-linearly with `N`.
//!   * `arc_swap_read` — `arc-swap`-backed read of an immutable route
//!     table. Models the read-mostly RCU path Phase 4.1 introduces
//!     for ARP / neighbour / routing tables.
//!
//! The Phase 4.1 success criterion sourced from these benches:
//! `sharded_mutex / N` should stay within 10% of the `N=1` baseline,
//! while `single_mutex / N` falls off well below 50% of baseline at
//! `N=4` and worse at `N=8`. Once Phase 4.1 lands, the production
//! kernel/src/network_service.rs `runtime_network_smp` follow-up bench
//! can replace the synthetic state with the real
//! `helios_kernel::ComponentHostNetworkService` and the curves carry
//! over without re-tuning.

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;

use divan::{AllocProfiler, Bencher, counter::ItemsCount};
use spin::Mutex as SpinMutex;

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

const OPS_PER_THREAD: usize = 4096;

fn main() {
    divan::main();
}

/// Synthetic "socket state" with the same shape as the lock-section
/// cost of a real netstack TCP write: a couple of pointer-touches, a
/// counter increment, an extra cache-line touch to keep the
/// compiler from collapsing the loop into a constant.
#[repr(align(64))]
struct ContendedSocketState {
    counter: u64,
    cache_lines: [u64; 8],
}

impl ContendedSocketState {
    const fn new() -> Self {
        Self {
            counter: 0,
            cache_lines: [0; 8],
        }
    }

    #[inline(always)]
    fn tick(&mut self) -> u64 {
        self.counter = self.counter.wrapping_add(1);
        for slot in &mut self.cache_lines {
            *slot = slot.wrapping_add(self.counter);
        }
        self.counter
    }
}

/// Cache-line-padded shard wrapper so each shard's mutex sits on its
/// own cache line. Without this, four mutexes would share one line on
/// 64-byte cache architectures and ping-pong even when independent.
#[repr(align(64))]
struct CacheAlignedShard {
    state: SpinMutex<ContendedSocketState>,
    _pad: [u8; 0],
}

impl CacheAlignedShard {
    const fn new() -> Self {
        Self {
            state: SpinMutex::new(ContendedSocketState::new()),
            _pad: [],
        }
    }
}

#[divan::bench(args = [1usize, 2, 4, 8])]
fn smp_single_mutex(bencher: Bencher, threads: usize) {
    let state = Arc::new(SpinMutex::new(ContendedSocketState::new()));
    bencher
        .counter(ItemsCount::new(threads * OPS_PER_THREAD))
        .bench_local(|| {
            thread::scope(|scope| {
                for _ in 0..threads {
                    let state = state.clone();
                    scope.spawn(move || {
                        for _ in 0..OPS_PER_THREAD {
                            let mut guard = state.lock();
                            black_box(guard.tick());
                        }
                    });
                }
            });
        });
}

#[divan::bench(args = [1usize, 2, 4, 8])]
fn smp_sharded_mutex(bencher: Bencher, threads: usize) {
    let shards: Arc<[CacheAlignedShard]> = (0..threads.max(1))
        .map(|_| CacheAlignedShard::new())
        .collect();
    bencher
        .counter(ItemsCount::new(threads * OPS_PER_THREAD))
        .bench_local(|| {
            thread::scope(|scope| {
                for tid in 0..threads {
                    let shards = shards.clone();
                    scope.spawn(move || {
                        for _ in 0..OPS_PER_THREAD {
                            let mut guard = shards[tid].state.lock();
                            black_box(guard.tick());
                        }
                    });
                }
            });
        });
}

/// Mixes some cross-shard work into the sharded path: a fraction of
/// ops hash to a non-local shard, modeling the realistic case where
/// the kernel must occasionally migrate a socket or operate on a
/// remote CPU's queue. The Phase 4.1 design must keep this curve
/// close to `smp_sharded_mutex` at the same `N`.
#[divan::bench(args = [1usize, 2, 4, 8])]
fn smp_sharded_mutex_with_cross_shard(bencher: Bencher, threads: usize) {
    let shard_count = threads.max(1);
    let shards: Arc<[CacheAlignedShard]> =
        (0..shard_count).map(|_| CacheAlignedShard::new()).collect();
    bencher
        .counter(ItemsCount::new(threads * OPS_PER_THREAD))
        .bench_local(|| {
            thread::scope(|scope| {
                for tid in 0..threads {
                    let shards = shards.clone();
                    scope.spawn(move || {
                        for op in 0..OPS_PER_THREAD {
                            // 1-in-16 ops cross to the next shard;
                            // matches a realistic cross-CPU
                            // migration / remote-socket access rate.
                            let shard_idx = if op % 16 == 0 {
                                (tid + 1) % shards.len()
                            } else {
                                tid
                            };
                            let mut guard = shards[shard_idx].state.lock();
                            black_box(guard.tick());
                        }
                    });
                }
            });
        });
}

/// Read-mostly path: each thread reads an `arc_swap`-backed route
/// table via `load`. Models the Phase 4.1 routing/ARP/neighbor
/// access pattern. Should scale linearly with `N` (every thread sees
/// independent atomic loads, no shared cache line on the hot read).
#[divan::bench(args = [1usize, 2, 4, 8])]
fn smp_arc_swap_read(bencher: Bencher, threads: usize) {
    use arc_swap::ArcSwap;
    let table: Arc<ArcSwap<RouteTableSnapshot>> =
        Arc::new(ArcSwap::from_pointee(RouteTableSnapshot::sample()));
    bencher
        .counter(ItemsCount::new(threads * OPS_PER_THREAD))
        .bench_local(|| {
            thread::scope(|scope| {
                for _ in 0..threads {
                    let table = table.clone();
                    scope.spawn(move || {
                        for _ in 0..OPS_PER_THREAD {
                            let snapshot = table.load();
                            black_box(snapshot.lookup_default_route());
                        }
                    });
                }
            });
        });
}

/// Approximates a kernel route table: a cache line of next-hop
/// entries plus a default-gateway shortcut.
struct RouteTableSnapshot {
    default_gateway: u64,
    entries: [u64; 8],
}

impl RouteTableSnapshot {
    fn sample() -> Self {
        Self {
            default_gateway: 0x0a00_0001,
            entries: [
                0x0a00_0002,
                0x0a00_0003,
                0x0a00_0004,
                0x0a00_0005,
                0x0a00_0006,
                0x0a00_0007,
                0x0a00_0008,
                0x0a00_0009,
            ],
        }
    }

    #[inline(always)]
    fn lookup_default_route(&self) -> u64 {
        let mut acc = self.default_gateway;
        for entry in self.entries {
            acc ^= entry;
        }
        acc
    }
}

/// Atomic-only baseline: spawns N threads each doing N atomic
/// fetch-add. Quantifies how cheap a contended atomic counter is on
/// the build's atomic profile (ARMv8.0 LL/SC vs ARMv8.1+ LSE) so
/// follow-up regressions on the `+lse` / `+rcpc` toolchain flag are
/// caught immediately.
#[divan::bench(args = [1usize, 2, 4, 8])]
fn smp_atomic_fetch_add(bencher: Bencher, threads: usize) {
    let counter: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    bencher
        .counter(ItemsCount::new(threads * OPS_PER_THREAD))
        .bench_local(|| {
            thread::scope(|scope| {
                for _ in 0..threads {
                    let counter = counter.clone();
                    scope.spawn(move || {
                        for _ in 0..OPS_PER_THREAD {
                            black_box(counter.fetch_add(1, Ordering::Relaxed));
                        }
                    });
                }
            });
        });
}

/// Per-thread atomic baseline (no contention) — should scale
/// perfectly linearly. Establishes the ceiling for the contended
/// numbers above.
#[divan::bench(args = [1usize, 2, 4, 8])]
fn smp_atomic_fetch_add_per_thread(bencher: Bencher, threads: usize) {
    let counters: Arc<[AtomicUsize]> = (0..threads.max(1)).map(|_| AtomicUsize::new(0)).collect();
    bencher
        .counter(ItemsCount::new(threads * OPS_PER_THREAD))
        .bench_local(|| {
            thread::scope(|scope| {
                for tid in 0..threads {
                    let counters = counters.clone();
                    scope.spawn(move || {
                        for _ in 0..OPS_PER_THREAD {
                            black_box(counters[tid].fetch_add(1, Ordering::Relaxed));
                        }
                    });
                }
            });
        });
}
