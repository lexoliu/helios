//! The frames a page-fault handler is allowed to allocate.
//!
//! A demand-committed fiber stack faults on first touch by
//! construction, and resolving that fault means putting a physical
//! frame behind the page. The fault runs on the faulting stack's own
//! processor, inside whatever the interrupted code was doing — and
//! kernel host calls run on fiber stacks, so the interrupted code may
//! hold the user-memory pool's lock, or the kernel heap's, or the frame
//! slab's. Asking any of them for a frame from there would spin on a
//! word only the interrupted context can clear, and masking interrupts
//! does nothing for an exception.
//!
//! So the fault path takes its frame from a small per-processor reserve
//! that no lock guards against another processor, and the reserve is
//! refilled from the pool through a non-blocking attempt
//! ([`UserMemoryPool::try_allocate_frame_on`](super::UserMemoryPool)) —
//! free unless the faulting context itself holds the pool. The reserve
//! therefore has to carry a fault only across the bounded stack growth
//! that lock-holding code does; an unbounded growth such as CPython's
//! recursion holds no lock, so every one of its thousands of faults
//! refills successfully.
//!
//! # Concurrency contract
//!
//! One shard per processor, each on its own cache line, and every word
//! in a shard is written by its own processor:
//!
//! - [`take_frame`] runs on the owning processor, in fault context,
//!   with that processor's interrupts masked by the exception entry.
//! - [`top_up`] runs on the owning processor outside fault context: the
//!   executor loop calls it on every iteration and stack creation calls
//!   it once more.
//! - [`configure_processors`] runs once, on the bootstrap processor,
//!   before any secondary is started.
//!
//! A shard is a lock-free stack — one atomic head, pushed and popped
//! with compare-and-swap — and not a locked list, because the one
//! concurrency it does see is its own processor interrupting itself.
//! Stack creation pops a shard from a fiber stack, and a fiber stack
//! faults on first touch, so a pop can be interrupted mid-operation by
//! the very fault that needs the shard: a lock there would be held by
//! the context the fault interrupted, and spinning on it would never
//! end. With compare-and-swap the interrupted pop simply loses its
//! exchange and retries. The same shape carries a foreign push, should
//! one ever arrive. The ABA case the pattern is known for needs a frame
//! to leave the list and come back between a load and its exchange; a
//! frame leaves only into a mapping and returns only through the pool
//! on a stack release, and neither happens inside a fault on the
//! processor that loaded it.
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

use super::user::{allocate_user_frame_zeroed_on, try_allocate_user_frame_zeroed_on};

/// Frames one processor's reserve holds when it is full.
///
/// The reserve exists for the faults a *lock-holding* path takes, and
/// those are bounded by the stack that path uses: the global
/// allocator's growth into the user pool, a virtio completion waking a
/// listener, a host call's own frames. Sixty-four pages is a quarter of
/// a megabyte per processor and far more stack than any of them touch.
/// A path that is not holding a lock refills on the way through and
/// never draws the reserve down at all.
const RESERVE_CAPACITY: usize = 64;

/// The level below which a fault refills the reserve.
///
/// Refilling in one batch rather than one frame at a time is what keeps
/// the lock attempts proportional to the reserve rather than to the
/// number of faults: a stack that grows by two thousand pages takes the
/// pool lock about sixty times, not two thousand.
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

/// One zeroed frame for the page-fault handler running on `processor`.
///
/// Pops the reserve, then refills it when it has fallen below its
/// low-water mark, so the common case is one pop and nothing else. A
/// pop that finds the reserve empty refills first and retries.
///
/// # Panics
///
/// When the reserve is empty and the pool refuses a non-blocking
/// refill. That is the pool's lock being held by the very context this
/// fault interrupted, and it means a lock-holding path grew its stack
/// by more than [`RESERVE_CAPACITY`] pages — the invariant this reserve
/// rests on. Failing here names it; there is no correct way to wait.
pub(crate) fn take_frame(processor: ProcessorId) -> NonNull<u8> {
    let shard = RESERVE.shard(processor);
    if let Some(frame) = shard.pop() {
        if shard.len() < RESERVE_LOW_WATER {
            try_refill(shard, processor);
        }
        return frame;
    }
    try_refill(shard, processor);
    shard.pop().unwrap_or_else(|| {
        panic!(
            "processor {} exhausted its {RESERVE_CAPACITY}-frame page-fault reserve and the user \
             memory pool refused a lock-free refill: a path holding the pool lock grew its stack \
             by more than the reserve holds",
            processor.id()
        )
    })
}

/// Refills `processor`'s reserve from outside fault context.
///
/// The executor loop calls this on every iteration and stack creation
/// calls it once more, so the reserve is normally full before any fault
/// reaches it. Unlike [`take_frame`]'s refill this may wait on the
/// pool's lock, because the caller is not inside a fault and holds
/// nothing. A pool that cannot serve the request leaves the reserve
/// where it is: the frames are not owed to anyone yet, and a program
/// that needs one will fail its own allocation with the typed
/// out-of-memory error it already handles.
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
    // arena's fault handler, so until the arena exists the reserve holds
    // nothing: frames parked here are frames the user pool cannot hand a
    // program.
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

/// Fills `shard` as far as the pool will go without waiting.
///
/// Runs in fault context, so nothing here may wait on a lock: the
/// pool is asked through its non-blocking path, and a shard that
/// refuses a push is a broken invariant rather than a frame to hand
/// back through the pool's locked free path.
fn try_refill(shard: &ReserveShard, processor: ProcessorId) {
    while shard.len() < RESERVE_CAPACITY {
        let Some(frame) = try_allocate_user_frame_zeroed_on(processor) else {
            return;
        };
        assert!(
            shard.push(frame),
            "processor {} found its page-fault frame reserve full underneath a refill it was \
             running itself: something other than the owning processor pushed to it",
            processor.id()
        );
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
    // or a fault below the mark would take the pool lock for one frame
    // and be back below the mark on the very next fault.
    assert!(RESERVE_LOW_WATER < RESERVE_CAPACITY);
    assert!(RESERVE_CAPACITY - RESERVE_LOW_WATER >= RESERVE_CAPACITY / 2);
};
