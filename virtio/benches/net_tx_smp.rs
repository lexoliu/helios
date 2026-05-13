//! Virtio-net TX queue contention baseline.
//!
//! `VirtioNetDevice` negotiates `VIRTIO_NET_F_MQ` and owns one TX/RX
//! queue-pair state per live queue. This bench keeps the old single
//! TX-lock curve beside the per-pair curve so queue-pair ownership
//! regressions show up as lost SMP scaling.

use std::hint::black_box;
use std::sync::Arc;
use std::thread;

use divan::{AllocProfiler, Bencher, counter::ItemsCount};
use spin::Mutex as SpinMutex;

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

const OPS_PER_THREAD: usize = 4096;
const TX_DESCRIPTORS: usize = 256;

fn main() {
    divan::main();
}

#[repr(align(64))]
struct TxQueueState {
    available: usize,
    completions: usize,
    bytes: usize,
}

impl TxQueueState {
    const fn new() -> Self {
        Self {
            available: TX_DESCRIPTORS,
            completions: 0,
            bytes: 0,
        }
    }

    #[inline(always)]
    fn submit_frame(&mut self, frame_len: usize) -> usize {
        if self.available == 0 {
            self.available = TX_DESCRIPTORS;
            self.completions = self.completions.wrapping_add(TX_DESCRIPTORS);
        }
        self.available -= 1;
        self.bytes = self.bytes.wrapping_add(frame_len);
        self.bytes ^ self.completions ^ self.available
    }
}

#[repr(align(64))]
struct TxQueuePair {
    state: SpinMutex<TxQueueState>,
}

impl TxQueuePair {
    const fn new() -> Self {
        Self {
            state: SpinMutex::new(TxQueueState::new()),
        }
    }
}

#[divan::bench(args = [1usize, 2, 4, 8])]
fn tx_single_queue(bencher: Bencher, threads: usize) {
    let queue = Arc::new(SpinMutex::new(TxQueueState::new()));
    bencher
        .counter(ItemsCount::new(threads * OPS_PER_THREAD))
        .bench_local(|| {
            thread::scope(|scope| {
                for thread_idx in 0..threads {
                    let queue = queue.clone();
                    scope.spawn(move || {
                        for op in 0..OPS_PER_THREAD {
                            let frame_len = 64 + ((thread_idx + op) & 0x3ff);
                            let mut state = queue.lock();
                            black_box(state.submit_frame(frame_len));
                        }
                    });
                }
            });
        });
}

#[divan::bench(args = [1usize, 2, 4, 8])]
fn tx_per_queue_pair(bencher: Bencher, threads: usize) {
    let pair_count = threads.max(1);
    let queue_pairs: Arc<[TxQueuePair]> = (0..pair_count).map(|_| TxQueuePair::new()).collect();
    bencher
        .counter(ItemsCount::new(threads * OPS_PER_THREAD))
        .bench_local(|| {
            thread::scope(|scope| {
                for thread_idx in 0..threads {
                    let queue_pairs = queue_pairs.clone();
                    scope.spawn(move || {
                        let queue_pair = &queue_pairs[thread_idx % queue_pairs.len()];
                        for op in 0..OPS_PER_THREAD {
                            let frame_len = 64 + ((thread_idx + op) & 0x3ff);
                            let mut state = queue_pair.state.lock();
                            black_box(state.submit_frame(frame_len));
                        }
                    });
                }
            });
        });
}
