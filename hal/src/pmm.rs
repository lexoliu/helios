//! Physical-frame allocator contract.
//!
//! Backends own physical memory; the kernel borrows frames through this
//! trait. The allocator hands out 4 KiB frames (and contiguous runs of
//! them) backed by some pool the backend wired up at boot — typically the
//! buddy heap built from `Platform::memory_regions`.
//!
//! # SMP contract
//!
//! All methods take `&self` and are safe to call from any processor at
//! any time. Implementations are expected to use per-CPU caches that feed
//! from a global freelist so the steady-state path is lock-free; locked
//! contention happens only on cache refill or drain.
//!
//! Frames are stable: the allocator does not relocate them behind the
//! caller's back. Coalescing of the *free* portion of the pool may happen
//! between calls.
//!
//! # Addressing
//!
//! A frame is named by its index in the kernel's direct map of physical
//! memory, which is what a backend hands the allocator when it publishes
//! a memory region. On a backend whose direct map sits at a non-zero
//! offset the translation to a bus address happens where every other
//! buffer's does — at the device boundary — so the allocator, its
//! callers, and the page-table layer all speak one address space.

use core::future::Future;

use thiserror::Error;

/// A single 4 KiB physical frame, identified by its frame number.
///
/// Frame number = physical address / [`PhysFrame::SIZE`]. Construction
/// from a raw physical address uses [`PhysFrame::from_phys_addr`], which
/// asserts page alignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhysFrame(usize);

impl PhysFrame {
    /// Frame size in bytes. Helios is 4 KiB everywhere; huge pages are
    /// a separate concern handled at the page-table layer (see
    /// [`crate::vmm`]).
    pub const SIZE: usize = 4096;

    pub const fn from_index(index: usize) -> Self {
        Self(index)
    }

    pub const fn from_phys_addr(addr: usize) -> Self {
        assert!(
            addr.is_multiple_of(Self::SIZE),
            "physical address must be page-aligned"
        );
        Self(addr / Self::SIZE)
    }

    pub const fn index(self) -> usize {
        self.0
    }

    pub const fn phys_addr(self) -> usize {
        self.0 * Self::SIZE
    }

    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// A contiguous run of physical frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysFrameRange {
    pub start: PhysFrame,
    pub frame_count: usize,
}

impl PhysFrameRange {
    pub const fn single(frame: PhysFrame) -> Self {
        Self {
            start: frame,
            frame_count: 1,
        }
    }

    pub const fn from_phys_addr(start: usize, byte_len: usize) -> Self {
        assert!(
            byte_len.is_multiple_of(PhysFrame::SIZE),
            "physical range length must be a frame multiple"
        );
        Self {
            start: PhysFrame::from_phys_addr(start),
            frame_count: byte_len / PhysFrame::SIZE,
        }
    }

    pub const fn byte_size(self) -> usize {
        self.frame_count * PhysFrame::SIZE
    }

    pub const fn is_empty(self) -> bool {
        self.frame_count == 0
    }
}

/// Failures from [`PhysFrameAllocator::allocate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum FrameAllocError {
    #[error("requested {requested} frames but pool has only {available} free")]
    OutOfFrames { requested: usize, available: usize },
    #[error("requested {requested} contiguous frames; largest free run is {largest}")]
    Fragmented { requested: usize, largest: usize },
}

/// Snapshot of frame-pool utilisation for diagnostics and pressure
/// monitoring. The numbers are an instantaneous read; concurrent
/// allocations may have changed them by the time the caller acts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameAllocStats {
    pub total_frames: usize,
    pub allocated_frames: usize,
    /// Largest contiguous free run, in frames. Used by the kernel
    /// pressure monitor to decide when to invoke compaction.
    pub largest_free_run: usize,
    /// Free frames currently shown to a free-page consumer through
    /// [`PhysFrameAllocator::free_runs`]. They are still the guest's to
    /// allocate; their contents are not.
    pub reported_frames: usize,
}

impl FrameAllocStats {
    pub const fn free_frames(self) -> usize {
        self.total_frames.saturating_sub(self.allocated_frames)
    }

    /// Fragmentation ratio in `[0, 1]` — `1.0` means the largest free
    /// run equals total free; `0.0` means no contiguous run at all.
    pub fn contiguity(self) -> f32 {
        let free = self.free_frames();
        if free == 0 {
            return 1.0;
        }
        (self.largest_free_run as f32) / (free as f32)
    }
}

/// Physical-frame allocator. See module docs for the SMP contract.
pub trait PhysFrameAllocator: Send + Sync + 'static {
    /// Allocate `count` contiguous frames.
    ///
    /// When `zero_first_use` is `true`, the implementation guarantees the
    /// frames contain only zero bytes when this call returns. When
    /// `false`, the caller is responsible for zeroing before exposing
    /// the frames to user code; this is an opt-out for hot paths that
    /// will overwrite the entire region anyway (image map-in, swap-in).
    ///
    /// A run that was handed to [`PhysFrameAllocator::free_runs`] is
    /// zeroed whatever the caller asked for: whoever the run was shown
    /// to is entitled to have discarded its contents, so `false` is not
    /// a promise the allocator can keep for it.
    fn allocate(
        &self,
        count: usize,
        zero_first_use: bool,
    ) -> Result<PhysFrameRange, FrameAllocError>;

    /// Shows contiguous runs of currently free memory to `visit`.
    ///
    /// This is the free-page reporting primitive: a consumer that wants
    /// to know which memory the guest has no use for — a virtio balloon
    /// telling its host, a hypervisor migration hint — needs runs it can
    /// name, and needs them to stay free while it names them. Each run
    /// is reserved before `visit` sees it and released afterwards, so no
    /// other allocation can be handed the same memory mid-report.
    ///
    /// Only runs of at least `min_frames` contiguous frames are visited,
    /// and at most `max_runs` of them; an implementation may visit
    /// fewer, and visits none at all when the pool has no run that
    /// large. Returns how many runs `visit` was called with.
    ///
    /// Every visited run is marked *reported*, which is what makes a
    /// later [`PhysFrameAllocator::allocate`] of it zero the memory.
    ///
    /// The visit is a future because the consumer that matters is a
    /// device: telling a host about a run means putting it on a
    /// virtqueue and waiting for the host to take it. The run stays
    /// reserved across that await, which is the whole point — a run
    /// released back into the pool while the host is still discarding
    /// it would be handed to someone whose writes then vanish.
    fn free_runs<Visit, Visited>(
        &self,
        min_frames: usize,
        max_runs: usize,
        visit: Visit,
    ) -> impl Future<Output = usize> + Send
    where
        Visit: FnMut(PhysFrameRange) -> Visited + Send,
        Visited: Future<Output = ()> + Send;

    /// Return frames to the pool.
    ///
    /// # Safety contract (logical, not `unsafe`)
    /// The caller must guarantee that no live mapping references any
    /// frame in `range`: page-table entries are already cleared and any
    /// TLB shootdown has completed. Returning a still-mapped frame is a
    /// use-after-free.
    fn deallocate(&self, range: PhysFrameRange);

    fn stats(&self) -> FrameAllocStats;
}
