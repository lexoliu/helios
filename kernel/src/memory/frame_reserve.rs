//! The frames a fiber-stack fault may take without waiting on the user
//! pool.
//!
//! A fiber-stack fault is served according to the context it interrupted
//! (see [`super::fiber_stack`]). When that context had interrupts
//! enabled it holds none of the pool's locks, because every one of them
//! masks its holder's interrupts while it is held, so the fault takes its
//! frames through the pool's ordinary blocking path and this reserve is
//! not touched. When the context had interrupts masked it may be holding
//! a pool lock — kernel host calls run on fiber stacks, and the kernel
//! heap grows out of the pool from inside the global allocator — and
//! asking the pool for a frame from there would spin on a word only the
//! interrupted context can clear. Those faults, and only those, take
//! their frames from this small per-processor reserve, which no lock
//! guards.
//!
//! The reserve is refilled through the pool's ordinary blocking path,
//! and only where nothing is held: [`top_up`], which the executor loop
//! calls on every iteration, and the end of a fiber-stack fault that was
//! served from the pool. A refill waits out another processor's pool
//! critical section like any allocation does, so contention on the pool
//! never leaves the reserve short. Only exhaustion of the pool does, and
//! a refill that the pool refuses stops quietly: the frames are not owed
//! to anyone yet, and a masked fault that needs one panics naming what
//! the reserve held.
//!
//! The reserve therefore has to carry a masked fault only across the
//! bounded stack growth that interrupt-masking code does between two
//! refills: the global allocator's growth into the user pool, a virtio
//! completion waking a listener, a host call's own frames.
//!
//! # Concurrency contract
//!
//! One shard per processor, each on its own cache line, and every word
//! in a shard is written by its own processor:
//!
//! - [`take_frame`] runs on the owning processor, in fault context, only
//!   for a fault whose interrupted context had interrupts masked. It is
//!   lock-free and never touches the pool.
//! - [`top_up`] runs on the owning processor outside any pool lock: the
//!   executor loop calls it on every iteration, and the fault path calls
//!   it after it has served a fault from the pool. It takes the pool's
//!   blocking path.
//! - [`configure_processors`] runs once, on the bootstrap processor,
//!   before any secondary is started.
//!
//! A shard is a lock-free stack — one atomic head, pushed and popped
//! with compare-and-swap — and not a locked list, because the one
//! concurrency it does see is its own processor interrupting itself: a
//! pop or a push can be interrupted mid-operation by a fault that needs
//! the same shard, and a lock there would be held by the context the
//! fault interrupted, which spinning on it would never end. With
//! compare-and-swap the interrupted operation simply loses its exchange
//! and retries. The same shape carries a foreign push, should one ever
//! arrive. The ABA case the pattern is known for needs a frame to leave
//! the list and come back between a load and its exchange; a frame leaves
//! only into a mapping and returns only through the pool on a stack
//! release, and neither happens inside a fault on the processor that
//! loaded it.
//!
//! The free list is threaded through the frames themselves, exactly as
//! [`super::frame_slab`] does, so the reserve allocates nothing of its
//! own. A frame is zeroed on the way in, which leaves only its first
//! word — the link — to clear on the way out.

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crossbeam_utils::CachePadded;
use helios_hal::cpu::ProcessorId;
use helios_hal::pmm::PhysFrame;
use spin::Once;

use super::user::allocate_user_frame_zeroed_on;

/// Frames one processor's reserve holds when it is full.
///
/// The reserve exists for the faults an *interrupt-masking* path takes,
/// and those are bounded by the stack that path uses: the global
/// allocator's growth into the user pool, a virtio completion waking a
/// listener, a host call's own frames. Sixty-four pages is a quarter of
/// a megabyte per processor and far more stack than any of them touch. A
/// path that runs with interrupts enabled is served from the pool and
/// never draws the reserve down at all.
pub(super) const RESERVE_CAPACITY: usize = 64;

/// The level below which a refill tops the reserve up.
///
/// Refilling in one batch rather than one frame at a time keeps the
/// pool's lock acquisitions proportional to the reserve rather than to
/// the number of faults.
const RESERVE_LOW_WATER: usize = 32;

struct FreeFrame {
    next: *mut FreeFrame,
}

/// One processor's frames. `head` and `len` are written only by that
/// processor; the padding around the shard is what keeps a neighbour's
/// pushes off this processor's cache line.
///
/// `len` follows `head` by one atomic step, so a reader that lands
/// between the two sees a count off by one. Every use of it is a
/// threshold — the low-water mark, the capacity — where one frame either
/// way changes nothing.
struct ReserveShard {
    head: AtomicPtr<FreeFrame>,
    len: AtomicUsize,
}

impl ReserveShard {
    const fn new() -> Self {
        Self {
            head: AtomicPtr::new(core::ptr::null_mut()),
            len: AtomicUsize::new(0),
        }
    }

    fn pop(&self) -> Option<NonNull<u8>> {
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            let frame = NonNull::new(head)?;
            // SAFETY: every frame on this list was pushed by `push`,
            // which wrote its link word, and a frame leaves the list
            // only through this exchange, so one that is still the head
            // is still on it.
            let next = unsafe { frame.as_ref().next };
            match self
                .head
                .compare_exchange_weak(head, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    self.len.fetch_sub(1, Ordering::AcqRel);
                    // The link word is the only part of the frame that
                    // is not still the zero `push` left behind.
                    // SAFETY: the frame is now owned by this caller
                    // alone.
                    unsafe {
                        frame.cast::<FreeFrame>().write(FreeFrame {
                            next: core::ptr::null_mut(),
                        });
                    }
                    return Some(frame.cast::<u8>());
                }
                Err(current) => head = current,
            }
        }
    }

    /// Puts a zeroed frame on the list, or reports that the reserve is
    /// already full and the caller must dispose of it.
    fn push(&self, frame: NonNull<u8>) -> bool {
        if self.len.load(Ordering::Acquire) >= RESERVE_CAPACITY {
            return false;
        }
        let mut frame = frame.cast::<FreeFrame>();
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            // SAFETY: the caller handed ownership of this page over, and
            // nothing reads its link word until the exchange below has
            // put it on the list.
            unsafe { frame.as_mut().next = head };
            match self.head.compare_exchange_weak(
                head,
                frame.as_ptr(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.len.fetch_add(1, Ordering::AcqRel);
                    return true;
                }
                Err(current) => head = current,
            }
        }
    }

    fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }
}

/// Per-processor reserves, built once the processor count is known.
struct FrameReserve {
    shards: Once<Box<[CachePadded<ReserveShard>]>>,
}

impl FrameReserve {
    const fn new() -> Self {
        Self {
            shards: Once::new(),
        }
    }

    fn shard(&self, processor: ProcessorId) -> &ReserveShard {
        let shards = self
            .shards
            .get()
            .unwrap_or_else(|| panic!("per-processor frame reserve used before configuration"));
        shards.get(usize::from(processor.id())).unwrap_or_else(|| {
            panic!(
                "processor {} is outside the configured frame reserve shard count {}",
                processor.id(),
                shards.len()
            )
        })
    }
}

static RESERVE: FrameReserve = FrameReserve::new();

/// Builds one reserve per processor. Called once, on the bootstrap
/// processor, before any secondary runs.
pub(crate) fn configure_processors(processor_count: usize) {
    assert!(
        processor_count != 0,
        "frame reserve requires at least one processor"
    );
    RESERVE.shards.call_once(|| {
        let mut shards = Vec::with_capacity(processor_count);
        shards.resize_with(processor_count, || CachePadded::new(ReserveShard::new()));
        shards.into_boxed_slice()
    });
}

/// Whether the reserve exists yet on this machine.
pub(crate) fn is_configured() -> bool {
    RESERVE.shards.get().is_some()
}

/// One zeroed frame for a fiber-stack fault on `processor` whose
/// interrupted context had interrupts masked, or `None` when the reserve
/// is empty. Lock-free: it never touches the user pool.
pub(crate) fn take_frame(processor: ProcessorId) -> Option<NonNull<u8>> {
    RESERVE.shard(processor).pop()
}

/// Refills `processor`'s reserve through the pool's blocking path.
///
/// Callers hold nothing and run with no pool lock held by the context
/// they interrupted: the executor loop, and the end of a fiber-stack
/// fault that was itself served from the pool. Waiting on the pool's
/// locks is therefore safe, and only another processor's critical
/// section — which waits for nothing — can delay it. A pool that cannot
/// serve the request leaves the reserve where it is.
pub(crate) fn top_up(processor: ProcessorId) {
    // The executor calls this on every iteration on every processor, so
    // the full reserve — which is the steady state — has to cost one
    // atomic load and nothing else. The checks are ordered for that.
    if !is_configured() {
        return;
    }
    let shard = RESERVE.shard(processor);
    if shard.len() >= RESERVE_LOW_WATER {
        return;
    }
    // The only path that draws on the reserve is the fiber-stack
    // arena's masked-fault path, so until the arena exists the reserve
    // holds nothing: frames parked here are frames the user pool cannot
    // hand a program.
    if !super::fiber_stack::arena_installed() {
        return;
    }
    while shard.len() < RESERVE_CAPACITY {
        let Ok(frame) = allocate_user_frame_zeroed_on(processor) else {
            return;
        };
        if !shard.push(frame) {
            super::deallocate_user_frame_on(processor, frame);
            return;
        }
    }
}

/// Frames the reserve is holding across every processor, in bytes.
pub fn reserved_bytes() -> usize {
    RESERVE.shards.get().map_or(0, |shards| {
        shards.iter().map(|shard| shard.len()).sum::<usize>() * PhysFrame::SIZE
    })
}

const _: () = {
    // The low-water mark has to leave a batch worth taking behind it,
    // or a refill below the mark would take the pool lock for one frame
    // and be back below the mark on the very next fault.
    assert!(RESERVE_LOW_WATER < RESERVE_CAPACITY);
    assert!(RESERVE_CAPACITY - RESERVE_LOW_WATER >= RESERVE_CAPACITY / 2);
};
