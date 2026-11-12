extern crate alloc;

use alloc::boxed::Box;
use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::future::Future;
use core::marker::PhantomData;
use core::mem::{align_of, size_of};
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, Poll};

use async_task::{Builder, Runnable, Task};
use concurrent_queue::{ConcurrentQueue, PopError, PushError};
use crossbeam_utils::CachePadded;
use helios_hal::cpu::{Cpu, ProcessorId, current_processor};
use helios_hal::numa::NodeId;
use helios_hal::watchdog::ProgressCounter;
use spin::Once;
use triomphe::Arc as NoWeakArc;

use crate::exec::idle::{
    IDLE_POLL_GROW_START, IDLE_POLL_MAX, IdleOutcome, IdleState, ProcessorIdle,
};
use crate::exec::sync::Notify;
use crate::exec::timer::Timer;
use crate::instance::{InstanceHome, RegisteredInstance};
use crate::memory::{
    LocalMemory, Locality, NodeMemory, PerProcessor, UserMemoryPool, task_arena_bytes_for,
};

type ReadyQueue = ConcurrentQueue<Runnable>;
pub type JoinHandle<T> = Task<T>;
pub const READY_BATCH_TASKS: usize = 1024;
const READY_QUEUE_CAPACITY: usize = READY_BATCH_TASKS * 4;
const TASK_ARENA_ALIGN: usize = 64;
/// Power-of-two block classes from the alignment granule (64 B) up to
/// the largest future the arena places (256 KiB).
const TASK_ARENA_CLASS_COUNT: usize = 13;
/// The largest block the arena serves, and the unit its two sub-arenas
/// are measured in.
///
/// Buddy merging never leaves the top block a piece was split out of,
/// so a sub-arena that is a whole number of top blocks is a boundary no
/// block can straddle and no merge can cross.
const TASK_ARENA_TOP_BYTES: usize = TASK_ARENA_ALIGN << (TASK_ARENA_CLASS_COUNT - 1);
/// Arena bytes only kernel-funded tasks may occupy.
///
/// Sized as one block of the largest class the arena serves, so the
/// kernel can always place a task of any size it spawns even when
/// instance-funded work has taken every byte it is allowed to take.
/// Without a reserve, user-mode load decides whether the kernel can
/// spawn, which is what made a spawn storm fatal.
const TASK_ARENA_KERNEL_RESERVE_BYTES: usize = TASK_ARENA_TOP_BYTES;
/// Empty-list and end-of-chain sentinel for block offsets.
const TASK_ARENA_BLOCK_NULL: usize = usize::MAX;
static EXECUTOR_GROUP: Once<NoWeakArc<ExecutorGroup>> = Once::new();

/// Which share of a processor's task arena a spawn draws from.
///
/// The distinction is ownership, not size: kernel-funded work is work
/// the kernel decided to do — its own tasks and the components it
/// provisions itself (system components, kernel plugins) — and running
/// out of arena for it is a kernel resource failure, which is fatal.
/// Instance-funded work is work a user-mode program asked for, and
/// running out of arena for it is a typed refusal handed back to that
/// program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskFunding {
    Kernel,
    Instance,
}

/// A task the executor refused to place because the instance share of
/// the arena is full.
///
/// Carries what a refusal has to say for itself: how much of the share
/// the spawn asked for and how many instance-funded tasks are already
/// live on this processor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "executor task arena instance share is full: {requested_bytes} bytes requested, {live_instance_tasks} instance tasks live in {share_bytes} bytes"
)]
pub struct TaskCapacityError {
    pub requested_bytes: usize,
    pub live_instance_tasks: usize,
    pub share_bytes: usize,
}

// A padded counter is one cache line by itself on every target, so a
// wake on one processor's counter can never invalidate another's.
const _: () = assert!(align_of::<CachePadded<AtomicUsize>>() >= 64);

/// The queues and counters every processor's executor shares.
///
/// # Concurrency contract
///
/// Every operation is lock-free. A processor's local queue is pushed by
/// any processor that wakes a local task it owns and popped by the
/// owner alone. A node's queue ([`NodeQueue`]) is pushed by whichever
/// processor schedules a global task whose home is that node, and
/// popped by every processor: its own node's processors drain it right
/// after their local queue, and every other processor steals from it
/// once its local queue and its own node's queue are both empty, in its
/// node's [`Locality::scheduling_order`]. A wake fans out to at most one
/// other processor per push (`GlobalScheduler::wake_one_processor_for`).
/// `locality` is immutable.
///
/// Cache-line contract: a processor's ready counter is written by
/// whichever processor wakes a task it owns and polled by the owner
/// on every run-loop pass, so each one sits on its own line rather
/// than eight to a line. Each node's queue is on lines of its own, and
/// inside it the ready count and the wake cursor each have one: both
/// are written by every processor that pushes to the node, and the
/// count is read by every processor on every park test. Without that
/// padding a push to one node would invalidate the lines another
/// node's processors poll, and the `Box` pointers every scheduler
/// dereferences.
///
/// A machine with one node has one node queue, which is exactly the
/// single machine-wide queue every processor drained before nodes
/// existed.
struct ExecutorGroup {
    /// One block per processor, in memory of the processor's node.
    processors: PerProcessor<ProcessorQueues>,
    /// Index: [`NodeId`].
    nodes: Box<[CachePadded<NodeQueue>]>,
    locality: &'static Locality,
}

/// What belongs to one processor: its local queue, the count of tasks
/// on it, its idle state and its task arena.
///
/// The queue is pushed by any processor that wakes a local task the
/// owner holds and popped by the owner alone. `ready` is written by
/// whichever processor wakes such a task and polled by the owner on
/// every run-loop pass, so it has a line of its own. `idle` is written
/// only by the owner on its way into and out of idle and read by every
/// processor that queues work for it; the wake handshake it serves is in
/// `idle`'s module docs. The arena's header is the one shared
/// allocation every future placed in it keeps a reference to; its bytes
/// are in the owner's node's memory.
struct ProcessorQueues {
    queue: ReadyQueue,
    ready: CachePadded<AtomicUsize>,
    idle: CachePadded<ProcessorIdle>,
    task_arena: NoWeakArc<TaskArena>,
}

/// One node's queue of global tasks whose home is that node.
///
/// Written by every processor that schedules such a task (the queue,
/// `ready` and `wake_cursor`) and popped by any processor (the queue and
/// `ready`); see [`ExecutorGroup`] for who pops when.
struct NodeQueue {
    queue: ReadyQueue,
    /// Tasks pushed and not yet popped. Incremented before the push and
    /// decremented after the pop, so a zero here means the queue is
    /// empty; it is the sender half of the idle handshake for the
    /// node's tasks.
    ready: CachePadded<AtomicUsize>,
    /// Which of the node's processors the next push considers waking.
    wake_cursor: CachePadded<AtomicUsize>,
}

/// Where a global task is queued: the node whose [`NodeQueue`] every
/// schedule of it pushes to.
///
/// Variation is through this trait rather than an enum: a kernel task's
/// home is fixed when it is spawned ([`FixedHome`]); an instance's task
/// follows its instance's home (`crate::instance::InstanceHome`). The
/// home is read on every schedule, from whichever processor woke the
/// task, so `node` answers without a lock.
pub(crate) trait TaskHome: Send + Sync + 'static {
    fn node(&self, locality: &Locality) -> NodeId;
}

/// A kernel task's home: the node of the processor that spawned it, for
/// the life of the task. A kernel task stays near where it was created
/// instead of drifting with whichever processor last woke it.
#[derive(Clone, Copy)]
struct FixedHome(NodeId);

impl TaskHome for FixedHome {
    fn node(&self, _: &Locality) -> NodeId {
        self.0
    }
}

/// Per-processor buddy arena for task futures.
///
/// # Concurrency contract
///
/// The arena belongs to one processor. A spawn is served by the arena
/// of the processor it is running on — [`current_processor`] picks
/// it, because a [`Spawner`] travels with the task that holds it — so
/// every allocation, split, merge and free-list mutation happens on the
/// owning processor. The metadata is single-owner and plain: no lock,
/// no atomic and no critical section stands on the spawn path. Nothing
/// re-enters an allocation from underneath one, either: an interrupt
/// handler notifies and queues, and a device handler holds no spawner.
///
/// A task does not end where it started. A global task spawned on one
/// processor is finished and dropped on whichever processor ran it
/// last, so a free cannot touch the metadata; it publishes the block on
/// a lock-free MPSC stack instead, and the owner drains that stack and
/// merges everything on it at the start of its next allocation. Local
/// and remote frees take the same path because the drop site cannot
/// name the processor it runs on — a future would have to carry a
/// `Cpu`, which is a clone and a drop of a refcounted backend handle on
/// every spawn — so "local" here means only that the push happened to
/// be uncontended.
///
/// The push is a CAS on the head with the link written into the freed
/// block itself; the drain is one swap of the whole chain. Neither
/// needs an ABA tag: a stack whose consumer never pops a single node
/// has no pop for ABA to race with.
///
/// The two halves sit on different cache lines, because different
/// processors write them: the buddy trees are the owner's, and the free
/// stack and the live counts belong to whichever processor a task ends
/// on. Without that padding every task ending anywhere in the machine
/// would invalidate the line its owner allocates out of, which is the
/// whole spawn hot path.
///
/// # Fungible bytes
///
/// Blocks are buddy-split and buddy-merged over the arena's 64-byte
/// granules, so freed bytes are fungible across size classes: a
/// returned 8 KiB block serves thirty-two 256-byte spawns, and two free
/// buddies merge back into the block they came from. That is #142.
/// Before it, each power-of-two class had its own free list and a bump
/// pointer that never rewound, so a class whose list was empty could
/// not be served while the rest of the arena sat free.
///
/// # The two sub-arenas
///
/// The kernel reserve is a sub-arena with its own buddy tree, so a
/// block that came from the reserve — and every piece it is split into
/// — stays a reserve block for the life of the kernel, and an instance
/// share that is full can never take from it. Instance-funded spawns
/// are served from the share and refused with a
/// [`TaskCapacityError`] when it is full. Kernel-funded spawns take the
/// share first and fall back to the reserve, so kernel work does not
/// pin reserve bytes while the share can serve it.
struct TaskArena {
    /// Fixed when the arena is built and never written again.
    bytes: ArenaBytes,
    /// Where the kernel reserve starts, which is also how many bytes
    /// instance-funded tasks may hold. Fixed at construction.
    share_bytes: usize,
    /// Written by the owning processor and by nothing else: the buddy
    /// trees and their free lists. On a cache line of its own, so a
    /// task ending on another processor cannot invalidate the line the
    /// owner allocates out of.
    regions: CachePadded<UnsafeCell<[BuddyRegion; BLOCK_REGION_COUNT]>>,
    /// Written by whichever processor a task placed here ends on, which
    /// is any of them. On a cache line of its own for the same reason,
    /// and its three cells share that line because a release writes all
    /// three and so does an allocation.
    shared: CachePadded<SharedCells>,
}

/// The arena cells any processor writes.
struct SharedCells {
    /// Head of the MPSC stack of freed blocks the owner has not merged
    /// yet, or [`TASK_ARENA_BLOCK_NULL`]. Pushed by the processor the
    /// task ended on, drained by the owner.
    remote_free: AtomicUsize,
    active: AtomicUsize,
    instance_active: AtomicUsize,
}

/// Which side of the instance/kernel split a block sits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockRegion {
    /// Below the kernel reserve: either funding may hold it.
    Instance = 0,
    /// The kernel reserve: kernel-funded tasks only.
    KernelReserve = 1,
}

const BLOCK_REGION_COUNT: usize = 2;

/// The arena's backing bytes, carved at boot from the memory of the node
/// the owning processor is on and never returned: the arena lives as
/// long as the machine, like the group that holds it.
///
/// There is no compile-time array behind an arena any more: how many
/// tasks a processor can hold is a property of the machine's memory,
/// and [`helios_kernel::task_arena_bytes_for`](crate::task_arena_bytes_for)
/// states it.
struct ArenaBytes {
    ptr: NonNull<u8>,
}

/// The header a block carries while it sits on one of its region's free
/// lists, written into the block's own bytes.
///
/// The lists are doubly linked because a merge removes a buddy from the
/// middle of one.
#[derive(Clone, Copy)]
#[repr(C)]
struct FreeNode {
    next: usize,
    prev: usize,
}

/// The header a freed block carries while it waits on the arena's MPSC
/// stack for the owner to drain it.
///
/// The class travels with the block because the owner has no other
/// record of how big it was; the arena keeps no side table of live
/// blocks.
#[derive(Clone, Copy)]
#[repr(C)]
struct RemoteFreeNode {
    next: usize,
    class: usize,
}

/// One buddy tree over a contiguous, top-block-aligned span of the
/// arena.
struct BuddyRegion {
    base: NonNull<u8>,
    start: usize,
    len: usize,
    /// Head of each class's free list, or [`TASK_ARENA_BLOCK_NULL`].
    free_heads: [usize; TASK_ARENA_CLASS_COUNT],
    /// One bit per block per class, set while that block is free and on
    /// its class's list. Whether a buddy is free is the one question a
    /// merge asks that the lists cannot answer in constant time.
    free_bits: Box<[u64]>,
    class_bit_base: [usize; TASK_ARENA_CLASS_COUNT],
}

struct ArenaFuture<Fut> {
    ptr: NonNull<Fut>,
    class: usize,
    funding: TaskFunding,
    arena: NoWeakArc<TaskArena>,
}

// SAFETY: every mutation of the owner-only metadata happens on the
// owning processor, and everything reachable from another processor —
// the remote-free stack and the two counters — is atomic.
unsafe impl Sync for TaskArena {}
unsafe impl Send for TaskArena {}
unsafe impl<Fut: Send> Send for ArenaFuture<Fut> {}

impl ArenaBytes {
    /// `len` bytes carved from the node `local` is on.
    fn carve<Source: NodeMemory>(len: usize, local: &LocalMemory<'_, Source>) -> Self {
        let layout = Layout::from_size_align(len, TASK_ARENA_ALIGN)
            .unwrap_or_else(|error| panic!("a task arena of {len} bytes has no layout: {error}"));
        Self {
            ptr: local.carve(layout),
        }
    }

    fn ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }
}

impl BuddyRegion {
    /// A tree over `start..start + len`, every top block of it free.
    fn new(base: NonNull<u8>, start: usize, len: usize) -> Self {
        assert!(
            len != 0 && len.is_multiple_of(TASK_ARENA_TOP_BYTES),
            "a task arena region of {len} bytes is not a whole number of {TASK_ARENA_TOP_BYTES}-byte top blocks"
        );
        let mut class_bit_base = [0; TASK_ARENA_CLASS_COUNT];
        let mut bits = 0;
        for (class, base_bit) in class_bit_base.iter_mut().enumerate() {
            *base_bit = bits;
            bits += len / TaskArena::class_bytes(class);
        }
        let free_bits = alloc::vec![0_u64; bits.div_ceil(u64::BITS as usize)];
        let mut region = Self {
            base,
            start,
            len,
            free_heads: [TASK_ARENA_BLOCK_NULL; TASK_ARENA_CLASS_COUNT],
            free_bits: free_bits.into_boxed_slice(),
            class_bit_base,
        };
        for offset in (start..start + len).step_by(TASK_ARENA_TOP_BYTES) {
            region.push_free(offset, TASK_ARENA_CLASS_COUNT - 1);
        }
        region
    }

    /// A block of `class`, split out of the smallest free block that
    /// can hold one, or `None` when this region has none.
    fn allocate(&mut self, class: usize) -> Option<usize> {
        let source = (class..TASK_ARENA_CLASS_COUNT)
            .find(|candidate| self.free_heads[*candidate] != TASK_ARENA_BLOCK_NULL)?;
        let offset = self.pop_free(source);
        for split in (class..source).rev() {
            self.push_free(offset + TaskArena::class_bytes(split), split);
        }
        Some(offset)
    }

    /// Returns a block, merging it with its buddy for as long as the
    /// buddy is free. Merging stops at the top class, so a merged block
    /// never leaves the top block it was split out of.
    fn free(&mut self, offset: usize, class: usize) {
        assert!(
            class < TASK_ARENA_CLASS_COUNT,
            "task arena block class {class} does not exist"
        );
        assert!(
            offset >= self.start
                && offset + TaskArena::class_bytes(class) <= self.start + self.len
                && (offset - self.start).is_multiple_of(TaskArena::class_bytes(class)),
            "task arena block at {offset} is not a class-{class} block of this region"
        );
        let mut offset = offset;
        let mut class = class;
        while class + 1 < TASK_ARENA_CLASS_COUNT {
            let buddy = self.buddy_of(offset, class);
            if !self.is_free(buddy, class) {
                break;
            }
            self.remove_free(buddy, class);
            offset = offset.min(buddy);
            class += 1;
        }
        self.push_free(offset, class);
    }

    /// The block `offset` merges with at `class`: its sibling under the
    /// same parent, which is always inside the same top block.
    const fn buddy_of(&self, offset: usize, class: usize) -> usize {
        let size = TaskArena::class_bytes(class);
        self.start + (((offset - self.start) / size) ^ 1) * size
    }

    fn bit_index(&self, offset: usize, class: usize) -> usize {
        self.class_bit_base[class] + (offset - self.start) / TaskArena::class_bytes(class)
    }

    fn is_free(&self, offset: usize, class: usize) -> bool {
        let bit = self.bit_index(offset, class);
        self.free_bits[bit / u64::BITS as usize] & (1 << (bit % u64::BITS as usize)) != 0
    }

    fn set_free(&mut self, offset: usize, class: usize, free: bool) {
        let bit = self.bit_index(offset, class);
        let mask = 1 << (bit % u64::BITS as usize);
        let word = &mut self.free_bits[bit / u64::BITS as usize];
        if free {
            *word |= mask;
        } else {
            *word &= !mask;
        }
    }

    /// The list header inside a free block. Every class is at least one
    /// 64-byte granule, so the header always fits, and every block is
    /// granule-aligned, so it is aligned.
    fn node(&self, offset: usize) -> *mut FreeNode {
        // SAFETY: `offset` is inside the arena, which is one allocation.
        unsafe { self.base.as_ptr().add(offset).cast::<FreeNode>() }
    }

    fn push_free(&mut self, offset: usize, class: usize) {
        let head = self.free_heads[class];
        // SAFETY: the block is free, so its bytes are the region's to
        // write, and the owner is the only writer.
        unsafe {
            self.node(offset).write(FreeNode {
                next: head,
                prev: TASK_ARENA_BLOCK_NULL,
            });
            if head != TASK_ARENA_BLOCK_NULL {
                (*self.node(head)).prev = offset;
            }
        }
        self.free_heads[class] = offset;
        self.set_free(offset, class, true);
    }

    fn pop_free(&mut self, class: usize) -> usize {
        let offset = self.free_heads[class];
        assert!(
            offset != TASK_ARENA_BLOCK_NULL,
            "task arena class {class} was popped while empty"
        );
        self.remove_free(offset, class);
        offset
    }

    fn remove_free(&mut self, offset: usize, class: usize) {
        // SAFETY: the block is on this class's list, so its header is
        // the one this region wrote.
        let node = unsafe { self.node(offset).read() };
        if node.prev == TASK_ARENA_BLOCK_NULL {
            self.free_heads[class] = node.next;
        } else {
            // SAFETY: as above, for the block before this one.
            unsafe {
                (*self.node(node.prev)).next = node.next;
            }
        }
        if node.next != TASK_ARENA_BLOCK_NULL {
            // SAFETY: as above, for the block after this one.
            unsafe {
                (*self.node(node.next)).prev = node.prev;
            }
        }
        self.set_free(offset, class, false);
    }
}

/// One processor's task arena on a machine of `usable_bytes`.
///
/// The boot memory plan states the share
/// ([`task_arena_bytes_for`](crate::memory::task_arena_bytes_for)); the
/// executor rounds it down to whole top blocks, because a sub-arena
/// boundary has to be one no block can straddle.
pub(crate) fn task_arena_bytes(usable_bytes: usize) -> usize {
    task_arena_bytes_for(usable_bytes) / TASK_ARENA_TOP_BYTES * TASK_ARENA_TOP_BYTES
}

impl TaskArena {
    /// An arena of `arena_bytes`, rounded down to whole top blocks, in
    /// its shared allocation.
    ///
    /// The size comes from the boot memory plan, so a processor's task
    /// capacity moves with the machine's memory (#159).
    fn new_shared<Source: NodeMemory>(
        arena_bytes: usize,
        local: &LocalMemory<'_, Source>,
    ) -> NoWeakArc<Self> {
        let total_bytes = arena_bytes / TASK_ARENA_TOP_BYTES * TASK_ARENA_TOP_BYTES;
        assert!(
            total_bytes >= TASK_ARENA_KERNEL_RESERVE_BYTES + TASK_ARENA_TOP_BYTES,
            "a task arena of {arena_bytes} bytes cannot hold the kernel reserve and a share"
        );
        let share_bytes = total_bytes - TASK_ARENA_KERNEL_RESERVE_BYTES;
        let bytes = ArenaBytes::carve(total_bytes, local);
        let base = bytes.ptr;
        NoWeakArc::new(Self {
            regions: CachePadded::new(UnsafeCell::new([
                BuddyRegion::new(base, 0, share_bytes),
                BuddyRegion::new(base, share_bytes, TASK_ARENA_KERNEL_RESERVE_BYTES),
            ])),
            bytes,
            share_bytes,
            shared: CachePadded::new(SharedCells {
                remote_free: AtomicUsize::new(TASK_ARENA_BLOCK_NULL),
                active: AtomicUsize::new(0),
                instance_active: AtomicUsize::new(0),
            }),
        })
    }

    /// Bytes a block of `class` occupies: `64 << class`.
    const fn class_bytes(class: usize) -> usize {
        TASK_ARENA_ALIGN << class
    }

    /// Smallest class whose block fits `size` bytes; panics when the
    /// future exceeds the largest class the arena serves.
    fn block_class(size: usize) -> usize {
        let block = size.max(1).next_power_of_two().max(TASK_ARENA_ALIGN);
        let class = block.trailing_zeros() as usize - TASK_ARENA_ALIGN.trailing_zeros() as usize;
        assert!(
            class < TASK_ARENA_CLASS_COUNT,
            "task future of {size} bytes exceeds the largest executor arena block"
        );
        class
    }

    /// Places a kernel-funded task. Exhaustion here is a kernel
    /// resource failure and stays fatal.
    fn allocate_kernel<Fut>(arena: &NoWeakArc<Self>, future: Fut) -> ArenaFuture<Fut> {
        Self::allocate(arena, future, TaskFunding::Kernel).unwrap_or_else(|_| {
            panic!(
                "executor task arena exhausted: {} live tasks",
                arena.shared.active.load(Ordering::Acquire)
            )
        })
    }

    /// Places an instance-funded task, or refuses it when the instance
    /// share of the arena is full. The future is dropped on refusal;
    /// the caller reports the refusal to the instance that asked for
    /// the task.
    fn allocate_instance<Fut>(
        arena: &NoWeakArc<Self>,
        future: Fut,
    ) -> Result<ArenaFuture<Fut>, TaskCapacityError> {
        Self::allocate(arena, future, TaskFunding::Instance)
    }

    fn allocate<Fut>(
        arena: &NoWeakArc<Self>,
        future: Fut,
        funding: TaskFunding,
    ) -> Result<ArenaFuture<Fut>, TaskCapacityError> {
        let size = size_of::<Fut>();
        let align = align_of::<Fut>().max(1);
        assert!(
            align <= TASK_ARENA_ALIGN,
            "future alignment exceeds executor task arena alignment"
        );
        let class = Self::block_class(size);
        let Some(start) = arena.take_block(class, funding) else {
            return Err(TaskCapacityError {
                requested_bytes: Self::class_bytes(class),
                live_instance_tasks: arena.shared.instance_active.load(Ordering::Acquire),
                share_bytes: arena.share_bytes,
            });
        };
        arena.shared.active.fetch_add(1, Ordering::AcqRel);
        if funding == TaskFunding::Instance {
            arena.shared.instance_active.fetch_add(1, Ordering::AcqRel);
        }
        // SAFETY: the block is this task's until it is released, and it
        // is `class_bytes(class)` bytes of granule-aligned storage,
        // which fits `Fut` by construction.
        let ptr = unsafe { arena.bytes.ptr().add(start).cast::<Fut>() };
        unsafe {
            ptr.write(future);
        }
        Ok(ArenaFuture {
            ptr: NonNull::new(ptr).expect("task arena pointer was null"),
            class,
            funding,
            arena: arena.clone(),
        })
    }

    /// Finds a block of `class` this funding may hold, after merging
    /// back everything freed since the last allocation.
    ///
    /// Kernel-funded work takes the share first and the reserve only
    /// when the share cannot serve it, so the reserve stays whole for
    /// the spawn that has nowhere else to go.
    fn take_block(&self, class: usize, funding: TaskFunding) -> Option<usize> {
        // SAFETY: allocation runs on the processor this arena belongs
        // to, and that processor is the only writer of the metadata.
        let regions = unsafe { &mut *self.regions.get() };
        self.drain_remote_frees(regions);
        let share = &mut regions[BlockRegion::Instance as usize];
        match funding {
            TaskFunding::Instance => share.allocate(class),
            TaskFunding::Kernel => share
                .allocate(class)
                .or_else(|| regions[BlockRegion::KernelReserve as usize].allocate(class)),
        }
    }

    /// Merges every block freed since the last allocation back into the
    /// region it came from.
    fn drain_remote_frees(&self, regions: &mut [BuddyRegion; BLOCK_REGION_COUNT]) {
        let mut cursor = self
            .shared
            .remote_free
            .swap(TASK_ARENA_BLOCK_NULL, Ordering::Acquire);
        while cursor != TASK_ARENA_BLOCK_NULL {
            // SAFETY: the block is free and carries the header its
            // release wrote; the `Acquire` swap published those writes.
            let node = unsafe { self.bytes.ptr().add(cursor).cast::<RemoteFreeNode>().read() };
            regions[self.region_of(cursor) as usize].free(cursor, node.class);
            cursor = node.next;
        }
    }

    /// The region a block belongs to, which its address decides. The
    /// two regions are whole numbers of top blocks and merging never
    /// crosses a top block, so no block ever straddles the boundary.
    const fn region_of(&self, offset: usize) -> BlockRegion {
        if offset < self.share_bytes {
            BlockRegion::Instance
        } else {
            BlockRegion::KernelReserve
        }
    }

    /// Publishes a dropped block on the arena's free stack, from
    /// whichever processor the task ended on.
    fn release(&self, block_offset: usize, class: usize, funding: TaskFunding) {
        // SAFETY: the future has been dropped, so the block's bytes are
        // the arena's again and this is their only writer until the CAS
        // below publishes them.
        let node = unsafe { self.bytes.ptr().add(block_offset).cast::<RemoteFreeNode>() };
        let mut head = self.shared.remote_free.load(Ordering::Relaxed);
        loop {
            // SAFETY: as above.
            unsafe {
                node.write(RemoteFreeNode { next: head, class });
            }
            match self.shared.remote_free.compare_exchange_weak(
                head,
                block_offset,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(current) => head = current,
            }
        }
        if funding == TaskFunding::Instance {
            let previous = self.shared.instance_active.fetch_sub(1, Ordering::AcqRel);
            assert!(previous != 0, "task arena instance task count underflowed");
        }
        let previous = self.shared.active.fetch_sub(1, Ordering::AcqRel);
        assert!(previous != 0, "task arena active count underflowed");
    }
}

impl<Fut> Future for ArenaFuture<Fut>
where
    Fut: Future,
{
    type Output = Fut::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe { Pin::new_unchecked(&mut *self.get_unchecked_mut().ptr.as_ptr()).poll(cx) }
    }
}

impl<Fut> Drop for ArenaFuture<Fut> {
    fn drop(&mut self) {
        unsafe {
            ptr::drop_in_place(self.ptr.as_ptr());
        }
        let offset = self.ptr.as_ptr() as usize - self.arena.bytes.ptr() as usize;
        self.arena.release(offset, self.class, self.funding);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProgressMode {
    Counted,
    Silent,
}

/// Join handle for a task that is constrained to the spawning processor.
///
/// The marker makes the handle `!Send` and `!Sync`, which prevents accidental
/// migration of a local task to a different processor through the type system.
#[must_use = "tasks get canceled when dropped, use `.detach()` to run them in the background"]
pub struct LocalJoinHandle<T> {
    task: Task<T>,
    _not_send_or_sync: PhantomData<*mut ()>,
}

#[derive(Clone)]
pub struct Spawner<CpuImpl: Cpu + Clone> {
    group: NoWeakArc<ExecutorGroup>,
    cpu: CpuImpl,
    owner_processor: ProcessorId,
    progress: ProgressCounter,
    progress_notify: NoWeakArc<Notify>,
}

pub struct Executor {
    group: NoWeakArc<ExecutorGroup>,
    owner_processor: ProcessorId,
    /// The owner's node, whose queue this executor drains right after
    /// its local one.
    node: NodeId,
    progress: ProgressCounter,
    progress_notify: NoWeakArc<Notify>,
}

/// The processor a [`Timer`] belongs to, and the idle state that
/// processor's executor publishes.
///
/// Only the owner programs the timer's hardware deadline; any other
/// processor that publishes an earlier deadline asks this whether the
/// owner has stopped looking ([`IdleState::Parked`]) and wakes it if so.
#[derive(Clone)]
pub(crate) struct TimerOwner {
    group: NoWeakArc<ExecutorGroup>,
    processor: ProcessorId,
}

impl TimerOwner {
    pub(crate) fn processor(&self) -> ProcessorId {
        self.processor
    }

    /// Whether the owner has published [`IdleState::Parked`]. `SeqCst`:
    /// the publisher half of the timer's wake handshake, after the
    /// publisher has lowered the timer's published deadline.
    pub(crate) fn is_parked(&self) -> bool {
        self.group.needs_wake(self.processor)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExecutorRunStats {
    local_runnable_count: usize,
    global_runnable_count: usize,
    local_empty_pop_count: usize,
    global_empty_pop_count: usize,
}

struct ProgressSignal {
    progress: ProgressCounter,
    progress_notify: NoWeakArc<Notify>,
}

// `async_task` stores the scheduler closure inside every heap-allocated task.
// Keep global/local schedulers narrow instead of capturing the whole `Spawner`;
// the executor spawn benchmark tracks this directly as bytes per task.
// The machine's processor count is read from the group rather than
// copied here, which is what makes room for the home.
struct GlobalScheduler<CpuImpl: Cpu + Clone, Home: TaskHome> {
    group: NoWeakArc<ExecutorGroup>,
    cpu: CpuImpl,
    progress: ProgressSignal,
    home: Home,
}

struct LocalScheduler<CpuImpl: Cpu + Clone> {
    group: NoWeakArc<ExecutorGroup>,
    cpu: CpuImpl,
    owner_processor: ProcessorId,
    progress: ProgressSignal,
}

struct LocalSilentScheduler<CpuImpl: Cpu + Clone> {
    group: NoWeakArc<ExecutorGroup>,
    cpu: CpuImpl,
    owner_processor: ProcessorId,
}

impl ExecutorRunStats {
    pub const fn runnable_count(self) -> usize {
        self.local_runnable_count + self.global_runnable_count
    }

    pub const fn local_runnable_count(self) -> usize {
        self.local_runnable_count
    }

    pub const fn global_runnable_count(self) -> usize {
        self.global_runnable_count
    }

    pub const fn local_empty_pop_count(self) -> usize {
        self.local_empty_pop_count
    }

    pub const fn global_empty_pop_count(self) -> usize {
        self.global_empty_pop_count
    }
}

impl Executor {
    /// The executor of `owner_processor` on the machine `pool`
    /// describes, which also says how many processors there are.
    ///
    /// Every processor's executor shares one group of queues, built by
    /// the first call from `pool`'s node memory; every processor passes
    /// the machine's one pool.
    pub fn new(
        progress: ProgressCounter,
        pool: &'static UserMemoryPool,
        owner_processor: ProcessorId,
    ) -> Self {
        Self::in_group(executor_group(pool), progress, owner_processor)
    }

    fn in_group(
        group: NoWeakArc<ExecutorGroup>,
        progress: ProgressCounter,
        owner_processor: ProcessorId,
    ) -> Self {
        let processor_count = group.processor_count();
        assert!(
            usize::from(owner_processor.id()) < processor_count,
            "executor owner processor {} is outside configured processor count {}",
            owner_processor.id(),
            processor_count
        );
        let node = group.locality.node_of(owner_processor);
        Self {
            group,
            owner_processor,
            node,
            progress,
            progress_notify: NoWeakArc::new(Notify::new()),
        }
    }

    pub fn spawner<CpuImpl: Cpu + Clone>(&self, cpu: CpuImpl) -> Spawner<CpuImpl> {
        Spawner {
            group: self.group.clone(),
            cpu,
            owner_processor: self.owner_processor,
            progress: self.progress.clone(),
            progress_notify: self.progress_notify.clone(),
        }
    }

    /// This processor's kernel timer, built on the processor that runs
    /// this executor and owned by it.
    ///
    /// The timer reads the owner's idle state, the one this executor
    /// publishes, to decide whether a deadline published from another
    /// processor needs a wake to reach the owner's comparator.
    pub fn timer<CpuImpl: Cpu + Clone>(&self, cpu: CpuImpl) -> Timer<CpuImpl> {
        Timer::new(
            cpu,
            TimerOwner {
                group: self.group.clone(),
                processor: self.owner_processor,
            },
        )
    }

    /// Runs ready tasks until the queues are empty or a batch has run.
    ///
    /// Each task is taken from the first queue that has one: the local
    /// queue when its count is non-zero, otherwise the owner's node
    /// queue, then the other of those two; then every other node's
    /// queue, nearest first ([`Locality::scheduling_order`]). Remote
    /// work is stolen as soon as the local and own-node queues are
    /// empty, one task at a time, so the next task is looked for close
    /// to home again. The node queues count as global in the stats.
    ///
    /// `timer` is this processor's. Before the first task of a pass runs,
    /// its deadline is pulled to within one scheduler tick
    /// ([`Timer::arm_scheduler_tick`]): the task may be a guest, which is
    /// preempted only from a timer interrupt, and a processor that has
    /// just left a park may have its deadline an idle interval out.
    pub fn run_until_stalled_with_stats<CpuImpl: Cpu + Clone>(
        &self,
        timer: &Timer<CpuImpl>,
    ) -> ExecutorRunStats {
        let mut stats = ExecutorRunStats::default();
        let local_ready_count = &self.group.queues(self.owner_processor).ready;
        let own_node = ReadySource::Node(self.node);
        // The order starts with the owner's node itself.
        let remote_nodes = &self.group.locality.scheduling_order(self.node)[1..];

        while stats.runnable_count() < READY_BATCH_TASKS {
            let (first, second) = if local_ready_count.load(Ordering::Relaxed) != 0 {
                (ReadySource::Local, own_node)
            } else {
                (own_node, ReadySource::Local)
            };
            let Some((runnable, source)) = self
                .pop_ready_source(first, &mut stats)
                .or_else(|| self.pop_ready_source(second, &mut stats))
                .or_else(|| {
                    remote_nodes.iter().find_map(|&node| {
                        self.pop_ready_source(ReadySource::Node(node), &mut stats)
                    })
                })
            else {
                return stats;
            };

            if stats.runnable_count() == 0 {
                timer.arm_scheduler_tick();
            }
            runnable.run();
            match source {
                ReadySource::Local => stats.local_runnable_count += 1,
                ReadySource::Node(_) => stats.global_runnable_count += 1,
            }
        }

        stats
    }

    fn pop_ready_source(
        &self,
        source: ReadySource,
        stats: &mut ExecutorRunStats,
    ) -> Option<(Runnable, ReadySource)> {
        let (queue, ready_count) = match source {
            ReadySource::Local => {
                let local = self.group.queues(self.owner_processor);
                (&local.queue, &*local.ready)
            }
            ReadySource::Node(node) => {
                let node = &self.group.nodes[node.index()];
                (&node.queue, &*node.ready)
            }
        };
        match pop_ready(queue, ready_count) {
            Ok(runnable) => Some((runnable, source)),
            Err(PopError::Empty | PopError::Closed) => {
                match source {
                    ReadySource::Local => stats.local_empty_pop_count += 1,
                    ReadySource::Node(_) => stats.global_empty_pop_count += 1,
                }
                None
            }
        }
    }

    /// Parks this processor until work arrives, polling its ready
    /// queues and its timer for a bounded, adaptive window first.
    ///
    /// `core::hint::spin_loop` is allowed here and nowhere else:
    /// AGENTS.md §4 reserves it for hardware synchronisation, and the
    /// idle loop of a processor that has no task to run is exactly
    /// that — it waits on another processor's queue push or an
    /// interrupt, not on software state inside an async context.
    ///
    /// `park_current` returns on any interrupt (the scheduler tick
    /// included), so a park that returns with no work simply loops
    /// back through `run_until_stalled` and here again.
    ///
    /// `root_ready` is the readiness of whatever the caller polls
    /// outside the ready queues — the bootstrap processor's root
    /// future in `Kernel::run_local_future` — and is tested in the
    /// same two places as the ready counts, so a wake published for
    /// it takes part in the same handshake: the publisher stores its
    /// flag, then reads this processor's idle state; this processor
    /// publishes `Parked`, then reads the flag.
    pub fn park_until_work<CpuImpl: Cpu + Clone>(
        &self,
        timer: &Timer<CpuImpl>,
        root_ready: impl Fn() -> bool,
    ) -> IdleOutcome {
        let cpu = timer.cpu();
        let idle = &self.group.queues(self.owner_processor).idle;
        let limit = idle.poll_limit_ticks();
        idle.store(IdleState::Polling);
        let started = cpu.now();
        if limit != 0 {
            loop {
                let now = cpu.now();
                if self.has_ready_work() || root_ready() || timer.is_due(now) {
                    idle.store(IdleState::Running);
                    return IdleOutcome::Polled {
                        polled_ticks: now.ticks() - started.ticks(),
                    };
                }
                if now.ticks() - started.ticks() >= limit {
                    break;
                }
                core::hint::spin_loop();
            }
        }
        // Publish `Parked`, then re-test: a push that landed between the
        // last poll and this store already incremented a ready count
        // this load observes, and the sender of any later push sees
        // `Parked` and sends the IPI. The SeqCst argument is `idle`'s
        // module doc.
        idle.store(IdleState::Parked);
        let now = cpu.now();
        if self.has_ready_work() || root_ready() || timer.is_due(now) {
            idle.store(IdleState::Running);
            return IdleOutcome::Polled {
                polled_ticks: now.ticks() - started.ticks(),
            };
        }
        // A sleep another processor registered on this timer while this
        // one was running or polling lowered its published deadline
        // without a wake; the same handshake covers it (`Timer`'s
        // `publish_deadline`), and this arms it before the park.
        timer.arm_published_deadline();
        let parked_at = cpu.now();
        cpu.park_current();
        idle.store(IdleState::Running);
        let parked_ticks = cpu.now().ticks() - parked_at.ticks();
        idle.adapt_poll_limit(
            parked_ticks,
            cpu.timebase().duration_to_ticks(IDLE_POLL_GROW_START),
            cpu.timebase().duration_to_ticks(IDLE_POLL_MAX),
        );
        IdleOutcome::Parked {
            polled_ticks: parked_at.ticks() - started.ticks(),
            parked_ticks,
        }
    }

    /// Whether any queue this processor drains has work waiting: its
    /// local queue and every node's, since it steals from all of them.
    /// `SeqCst`: the parker half of the handshake in `idle`'s module
    /// docs.
    fn has_ready_work(&self) -> bool {
        self.group
            .queues(self.owner_processor)
            .ready
            .load(Ordering::SeqCst)
            != 0
            || self
                .group
                .nodes
                .iter()
                .any(|node| node.ready.load(Ordering::SeqCst) != 0)
    }
}

#[derive(Clone, Copy)]
enum ReadySource {
    Local,
    Node(NodeId),
}

impl<CpuImpl: Cpu + Clone> Spawner<CpuImpl> {
    pub(crate) fn progress_counter(&self) -> ProgressCounter {
        self.progress.clone()
    }

    pub(crate) fn progress_notify(&self) -> NoWeakArc<Notify> {
        self.progress_notify.clone()
    }

    /// The task arena of `processor`, the one this spawn is running on.
    ///
    /// A `Spawner` travels with the task that holds it, so the arena a
    /// spawn draws on is chosen at the spawn rather than bound when the
    /// spawner was made: the arena's metadata is owner-only, and the
    /// owner is the processor doing the allocating.
    fn task_arena(&self, processor: ProcessorId) -> &NoWeakArc<TaskArena> {
        &self.group.queues(processor).task_arena
    }

    /// Places `future` in the task arena of `processor`, the one this
    /// spawn is running on, under `funding`.
    fn allocate<Fut>(
        &self,
        processor: ProcessorId,
        future: Fut,
        funding: TaskFunding,
    ) -> Result<ArenaFuture<Fut>, TaskCapacityError> {
        let arena = self.task_arena(processor);
        match funding {
            TaskFunding::Kernel => Ok(TaskArena::allocate_kernel(arena, future)),
            TaskFunding::Instance => TaskArena::allocate_instance(arena, future),
        }
    }

    fn spawn_with_progress<Fut, Home>(
        &self,
        future: ArenaFuture<Fut>,
        home: Home,
    ) -> JoinHandle<Fut::Output>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
        Home: TaskHome,
    {
        let scheduler = self.global_scheduler(home);
        let schedule = move |runnable| scheduler.schedule(runnable);
        let (runnable, task) = Builder::new().spawn(move |_| future, schedule);
        runnable.schedule();
        task
    }

    fn spawn_local_with_progress<Fut>(
        &self,
        future: ArenaFuture<Fut>,
        progress_mode: ProgressMode,
    ) -> LocalJoinHandle<Fut::Output>
    where
        Fut: Future + 'static,
        Fut::Output: 'static,
    {
        if progress_mode == ProgressMode::Silent {
            let scheduler = self.local_silent_scheduler();
            let schedule = move |runnable| scheduler.schedule(runnable);

            // SAFETY: the runnable is always re-enqueued onto the spawning processor's ready
            // queue, and `LocalJoinHandle` is `!Send`, so the task cannot be awaited or
            // dropped from a different processor through safe Rust.
            let (runnable, task) =
                unsafe { Builder::new().spawn_unchecked(move |_| future, schedule) };
            runnable.schedule();
            return LocalJoinHandle {
                task,
                _not_send_or_sync: PhantomData,
            };
        }

        let scheduler = self.local_scheduler();
        let schedule = move |runnable| scheduler.schedule(runnable);

        // SAFETY: the runnable is always re-enqueued onto the spawning processor's ready
        // queue, and `LocalJoinHandle` is `!Send`, so the task cannot be awaited or
        // dropped from a different processor through safe Rust.
        let (runnable, task) = unsafe { Builder::new().spawn_unchecked(move |_| future, schedule) };
        runnable.schedule();
        LocalJoinHandle {
            task,
            _not_send_or_sync: PhantomData,
        }
    }

    fn progress_signal(&self) -> ProgressSignal {
        ProgressSignal {
            progress: self.progress.clone(),
            progress_notify: self.progress_notify.clone(),
        }
    }

    fn global_scheduler<Home: TaskHome>(&self, home: Home) -> GlobalScheduler<CpuImpl, Home> {
        GlobalScheduler {
            group: self.group.clone(),
            cpu: self.cpu.clone(),
            progress: self.progress_signal(),
            home,
        }
    }

    fn local_scheduler(&self) -> LocalScheduler<CpuImpl> {
        LocalScheduler {
            group: self.group.clone(),
            cpu: self.cpu.clone(),
            owner_processor: self.owner_processor,
            progress: self.progress_signal(),
        }
    }

    fn local_silent_scheduler(&self) -> LocalSilentScheduler<CpuImpl> {
        LocalSilentScheduler {
            group: self.group.clone(),
            cpu: self.cpu.clone(),
            owner_processor: self.owner_processor,
        }
    }

    /// A view of this spawner that places `instance`'s tasks: funded
    /// under `funding`, handing back a typed refusal instead of
    /// panicking when the instance share is full, and queued on the
    /// instance's home node.
    pub fn instance_spawner(
        &self,
        funding: TaskFunding,
        instance: &RegisteredInstance,
    ) -> InstanceSpawner<CpuImpl> {
        InstanceSpawner {
            inner: self.clone(),
            funding,
            home: instance.home(),
        }
    }

    /// Spawns a kernel task whose home is the node of the processor
    /// spawning it ([`FixedHome`]).
    pub fn spawn<Fut>(&self, future: Fut) -> JoinHandle<Fut::Output>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        let processor = current_processor();
        let future = TaskArena::allocate_kernel(self.task_arena(processor), future);
        self.spawn_with_progress(future, FixedHome(self.group.locality.node_of(processor)))
    }

    pub fn spawn_detached<Fut>(&self, future: Fut)
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        self.spawn(future).detach();
    }

    pub fn spawn_local<Fut>(&self, future: Fut) -> LocalJoinHandle<Fut::Output>
    where
        Fut: Future + 'static,
        Fut::Output: 'static,
    {
        let future = TaskArena::allocate_kernel(self.task_arena(current_processor()), future);
        self.spawn_local_with_progress(future, ProgressMode::Counted)
    }

    pub fn spawn_local_detached<Fut>(&self, future: Fut)
    where
        Fut: Future + 'static,
        Fut::Output: 'static,
    {
        self.spawn_local(future).detach();
    }

    pub(crate) fn spawn_local_detached_silent<Fut>(&self, future: Fut)
    where
        Fut: Future + 'static,
        Fut::Output: 'static,
    {
        let future = TaskArena::allocate_kernel(self.task_arena(current_processor()), future);
        self.spawn_local_with_progress(future, ProgressMode::Silent)
            .detach();
    }
}

/// The spawner a wasm instance's store holds.
///
/// Every task it places is attributed to the instance that asked for
/// it, so a program that wants more tasks than the machine can serve is
/// refused instead of walking the arena empty under the kernel. The
/// type is the contract: an instance context cannot reach an
/// infallible spawn, because it never holds a [`Spawner`].
///
/// Its global tasks are queued on the instance's home node, which every
/// spawner of the instance shares with the instance's registry entry
/// (`InstanceHome`). Its local tasks stay on the processor that
/// spawned them, as every local task does.
#[derive(Clone)]
pub struct InstanceSpawner<CpuImpl: Cpu + Clone> {
    inner: Spawner<CpuImpl>,
    funding: TaskFunding,
    home: NoWeakArc<InstanceHome>,
}

impl<CpuImpl: Cpu + Clone> InstanceSpawner<CpuImpl> {
    pub fn funding(&self) -> TaskFunding {
        self.funding
    }

    pub(crate) fn progress_counter(&self) -> ProgressCounter {
        self.inner.progress_counter()
    }

    /// The processor-local spawner a *new* instance's funding is drawn
    /// from.
    ///
    /// The launch path needs it to build the child instance's own
    /// [`InstanceSpawner`] on the arena that will hold the child's
    /// tasks. It is not a way back to an infallible spawn for the
    /// instance that holds this one: nothing outside the launch path
    /// takes it.
    pub(crate) fn launch_spawner(&self) -> &Spawner<CpuImpl> {
        &self.inner
    }

    pub fn try_spawn<Fut>(&self, future: Fut) -> Result<JoinHandle<Fut::Output>, TaskCapacityError>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        let future = self
            .inner
            .allocate(current_processor(), future, self.funding)?;
        Ok(self.inner.spawn_with_progress(future, self.home.clone()))
    }

    pub fn try_spawn_detached<Fut>(&self, future: Fut) -> Result<(), TaskCapacityError>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        self.try_spawn(future)?.detach();
        Ok(())
    }

    pub fn try_spawn_local<Fut>(
        &self,
        future: Fut,
    ) -> Result<LocalJoinHandle<Fut::Output>, TaskCapacityError>
    where
        Fut: Future + 'static,
        Fut::Output: 'static,
    {
        let future = self
            .inner
            .allocate(current_processor(), future, self.funding)?;
        Ok(self
            .inner
            .spawn_local_with_progress(future, ProgressMode::Counted))
    }

    pub fn try_spawn_local_detached<Fut>(&self, future: Fut) -> Result<(), TaskCapacityError>
    where
        Fut: Future + 'static,
        Fut::Output: 'static,
    {
        self.try_spawn_local(future)?.detach();
        Ok(())
    }
}

impl ProgressSignal {
    #[inline]
    fn record(&self) {
        self.progress.record_progress();
        self.progress_notify.notify_one_coalesced();
    }
}

impl<CpuImpl: Cpu + Clone, Home: TaskHome> GlobalScheduler<CpuImpl, Home> {
    /// Queues the task on its home node, whichever processor woke it.
    #[inline]
    fn schedule(&self, runnable: Runnable) {
        let node = self.home.node(self.group.locality);
        let queue = &self.group.nodes[node.index()];
        let previous_ready = push_ready(&queue.queue, &queue.ready, runnable);
        self.progress.record();
        // Every processor steals from every node, so the threshold is
        // the machine's: fewer queued tasks than other processors means
        // some processor has not been asked for this one.
        if should_wake_global_processor(previous_ready, self.group.processor_count()) {
            self.wake_one_processor_for(node);
        }
    }

    /// Wakes one processor for a task just pushed to `node`.
    ///
    /// The candidates are `node`'s own processors, then each other
    /// node's in `node`'s [`Locality::scheduling_order`]; a node whose
    /// only processor is the pusher, or that has none, has no candidate,
    /// and the walk moves to the next node. Within a node, the node's
    /// cursor names one processor per push, so consecutive pushes fan out
    /// across its processors. The first candidate decides: its published
    /// state says whether the wake needs an IPI. A polling processor
    /// re-reads every node's ready count on every pass; a running one
    /// drains the node queues in its own `run_until_stalled`, and
    /// re-tests the counts after publishing `Parked` on its way out; only
    /// an already-parked processor has stopped looking.
    #[inline]
    fn wake_one_processor_for(&self, node: NodeId) {
        if self.group.processor_count() <= 1 {
            return;
        }
        let current_processor = current_processor();
        let locality = self.group.locality;
        for &candidate_node in locality.scheduling_order(node) {
            let processors = locality.processors_of(candidate_node);
            if processors.is_empty() {
                continue;
            }
            let start = self.group.nodes[candidate_node.index()]
                .wake_cursor
                .fetch_add(1, Ordering::Relaxed);
            for offset in 0..processors.len() {
                let processor = processors[(start + offset) % processors.len()];
                if processor == current_processor {
                    continue;
                }
                if self.group.needs_wake(processor) {
                    self.cpu.wake_processor(processor);
                }
                return;
            }
        }
    }
}

impl<CpuImpl: Cpu + Clone> LocalScheduler<CpuImpl> {
    #[inline]
    fn schedule(&self, runnable: Runnable) {
        let local = self.group.queues(self.owner_processor);
        let previous_ready = push_ready(&local.queue, &local.ready, runnable);
        self.progress.record();
        if should_wake_owner_processor(previous_ready)
            && current_processor() != self.owner_processor
            && self.group.needs_wake(self.owner_processor)
        {
            self.cpu.wake_processor(self.owner_processor);
        }
    }
}

impl<CpuImpl: Cpu + Clone> LocalSilentScheduler<CpuImpl> {
    #[inline]
    fn schedule(&self, runnable: Runnable) {
        let local = self.group.queues(self.owner_processor);
        let previous_ready = push_ready(&local.queue, &local.ready, runnable);
        if should_wake_owner_processor(previous_ready)
            && current_processor() != self.owner_processor
            && self.group.needs_wake(self.owner_processor)
        {
            self.cpu.wake_processor(self.owner_processor);
        }
    }
}

/// Serializes the tests that drive an [`Executor`].
///
/// [`EXECUTOR_GROUP`] is a process-wide singleton, so every `Executor`
/// a test binary builds shares one set of ready queues and one task
/// arena per processor. Two tests running their own executors at the
/// same time would run each other's tasks and charge each other's
/// arena, so they take this in turn instead.
#[cfg(test)]
pub(crate) fn executor_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static EXECUTOR_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    EXECUTOR_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn executor_group(pool: &'static UserMemoryPool) -> NoWeakArc<ExecutorGroup> {
    EXECUTOR_GROUP
        .call_once(|| NoWeakArc::new(ExecutorGroup::new(pool)))
        .clone()
}

impl ExecutorGroup {
    /// One local queue, ready count, idle state and task arena per
    /// processor, and one queue per node, for the machine `locality`
    /// describes.
    fn new(pool: &'static UserMemoryPool) -> Self {
        let locality = pool.locality();
        assert!(
            locality.processor_count() != 0,
            "executor processor count must be non-zero"
        );
        // Every processor's arena is the same share of the same
        // machine; what it holds is the machine's memory divided by
        // what a live instance costs an arena, not a constant.
        let arena_bytes = task_arena_bytes(crate::machine_usable_bytes());
        let processors = PerProcessor::new(pool, |_, local| ProcessorQueues {
            queue: ready_queue(),
            ready: CachePadded::new(AtomicUsize::new(0)),
            idle: CachePadded::new(ProcessorIdle::new()),
            task_arena: TaskArena::new_shared(arena_bytes, local),
        });
        let nodes = (0..locality.node_count())
            .map(|_| {
                CachePadded::new(NodeQueue {
                    queue: ready_queue(),
                    ready: CachePadded::new(AtomicUsize::new(0)),
                    wake_cursor: CachePadded::new(AtomicUsize::new(0)),
                })
            })
            .collect();
        Self {
            processors,
            nodes,
            locality,
        }
    }

    fn processor_count(&self) -> usize {
        self.processors.len()
    }

    /// `processor`'s queue, ready count, idle state and task arena.
    fn queues(&self, processor: ProcessorId) -> &ProcessorQueues {
        self.processors.get(processor)
    }

    /// Whether `processor` must be sent a wake IPI for work just
    /// pushed. Called after the push. Returns true only when the
    /// target has published `Parked`; a `Polling` or `Running`
    /// processor sees the ready count itself.
    fn needs_wake(&self, processor: ProcessorId) -> bool {
        self.queues(processor).idle.state() == IdleState::Parked
    }
}

fn ready_queue() -> ReadyQueue {
    ConcurrentQueue::bounded(READY_QUEUE_CAPACITY)
}

#[inline]
fn push_ready(queue: &ReadyQueue, ready_count: &AtomicUsize, runnable: Runnable) -> usize {
    // The queue itself publishes the `Runnable`; the counter is also
    // the sender half of the idle handshake — a parking processor
    // stores `Parked` then loads it, this increment precedes the load
    // of the parker's idle state, and both are `SeqCst`, so no wake is
    // lost. On x86 `lock xadd` is already a full barrier, so this
    // costs nothing there.
    let previous_ready = ready_count.fetch_add(1, Ordering::SeqCst);
    match queue.push(runnable) {
        Ok(()) => previous_ready,
        Err(PushError::Full(_)) => {
            rollback_ready_count(ready_count);
            panic!("executor ready queue capacity {READY_QUEUE_CAPACITY} exhausted")
        }
        Err(PushError::Closed(_)) => {
            rollback_ready_count(ready_count);
            panic!("executor ready queue was closed unexpectedly")
        }
    }
}

fn pop_ready(queue: &ReadyQueue, ready_count: &AtomicUsize) -> Result<Runnable, PopError> {
    let runnable = queue.pop()?;
    let previous = ready_count.fetch_sub(1, Ordering::Relaxed);
    assert!(previous != 0, "executor ready count underflowed");
    Ok(runnable)
}

fn rollback_ready_count(ready_count: &AtomicUsize) {
    let previous = ready_count.fetch_sub(1, Ordering::Relaxed);
    assert!(
        previous != 0,
        "executor ready count underflowed while rolling back failed enqueue"
    );
}

const fn should_wake_global_processor(previous_ready: usize, processor_count: usize) -> bool {
    previous_ready < processor_count.saturating_sub(1)
}

const fn should_wake_owner_processor(previous_ready: usize) -> bool {
    previous_ready == 0
}

impl<T> LocalJoinHandle<T> {
    pub fn detach(self) {
        self.task.detach();
    }

    pub async fn cancel(self) -> Option<T> {
        self.task.cancel().await
    }
}

impl<T> Future for LocalJoinHandle<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.task).poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use core::future::Future as _;
    use core::mem::size_of;
    use core::pin::Pin;
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use core::task::{Context, Waker};

    use alloc::sync::Arc;
    use alloc::vec::Vec;

    use super::{
        Executor, ExecutorGroup, FixedHome, GlobalScheduler, IDLE_POLL_GROW_START, IDLE_POLL_MAX,
        IdleOutcome, IdleState, LocalScheduler, LocalSilentScheduler, NoWeakArc,
        READY_QUEUE_CAPACITY, Spawner, TASK_ARENA_CLASS_COUNT, TASK_ARENA_KERNEL_RESERVE_BYTES,
        TASK_ARENA_TOP_BYTES, TaskArena, pop_ready, ready_queue, should_wake_global_processor,
        should_wake_owner_processor, task_arena_bytes,
    };
    use crate::instance::{InstanceActivity, InstanceHome, InstanceRegistry};
    use crate::memory::{PerProcessor, UserMemoryPool};
    use crate::test_support::{
        RecordingSmpCpu, TEST_POOL_NODE_BYTES, TestMachine, single_node_pool,
    };
    use crate::{TaskFunding, test_processor_identity};
    use helios_hal::cpu::{
        Cpu, HardwarePerfCounters, Instant, ProcessorId, Timebase, TimerInterrupt,
    };
    use helios_hal::watchdog::ProgressCounter;

    #[derive(Clone)]
    struct TestCpu {
        timebase: Timebase,
    }

    impl TestCpu {
        fn new() -> Self {
            Self {
                timebase: Timebase::new(1_000_000_000),
            }
        }
    }

    impl Cpu for TestCpu {
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
            panic!("test CPU cannot shut down")
        }

        fn reboot(&self) -> ! {
            panic!("test CPU cannot reboot")
        }
    }

    /// A CPU that moves its own clock: every `now()` steps the
    /// timebase forward by `step`, and `park_current` sleeps
    /// `park_advance` ticks, so a test drives the idle loop's poll
    /// window and park duration deterministically. `forbid_park`
    /// turns `park_current` into a panic for tests that must prove
    /// the park was never reached.
    #[derive(Clone)]
    struct SteppingClockCpu {
        nanos: Arc<AtomicU64>,
        step: u64,
        park_advance: Arc<AtomicU64>,
        may_park: Arc<AtomicBool>,
        timebase: Timebase,
    }

    impl SteppingClockCpu {
        fn new(step: u64) -> Self {
            crate::test_processor_identity::set(ProcessorId::new(0));
            Self {
                nanos: Arc::new(AtomicU64::new(0)),
                step,
                park_advance: Arc::new(AtomicU64::new(0)),
                may_park: Arc::new(AtomicBool::new(true)),
                timebase: Timebase::new(1_000_000_000),
            }
        }

        fn set_park_advance(&self, ticks: u64) {
            self.park_advance.store(ticks, Ordering::Relaxed);
        }

        fn forbid_park(&self) {
            self.may_park.store(false, Ordering::Relaxed);
        }
    }

    impl Cpu for SteppingClockCpu {
        const TIMER_INTERRUPT: TimerInterrupt = TimerInterrupt::Deadline;

        fn processor_count(&self) -> usize {
            2
        }

        fn bootstrap_processor(&self) -> ProcessorId {
            ProcessorId::new(0)
        }

        fn park_current(&self) {
            assert!(
                self.may_park.load(Ordering::Relaxed),
                "park_current reached while work was already queued"
            );
            self.nanos
                .fetch_add(self.park_advance.load(Ordering::Relaxed), Ordering::Relaxed);
        }

        fn start_processor(&self, _processor: ProcessorId) {}

        fn wake_processor(&self, _processor: ProcessorId) {}

        fn now(&self) -> Instant {
            Instant::new(self.nanos.fetch_add(self.step, Ordering::Relaxed))
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
            panic!("test CPU cannot shut down")
        }

        fn reboot(&self) -> ! {
            panic!("test CPU cannot reboot")
        }
    }

    #[test]
    fn global_wake_scales_to_processor_count_not_enqueue_count() {
        assert!(!should_wake_global_processor(0, 1));
        assert!(should_wake_global_processor(0, 4));
        assert!(should_wake_global_processor(2, 4));
        assert!(!should_wake_global_processor(3, 4));
    }

    #[test]
    fn owner_wake_query_only_fires_on_empty_to_nonempty_enqueue() {
        assert!(should_wake_owner_processor(0));
        assert!(!should_wake_owner_processor(1));
    }

    #[test]
    fn ready_queue_is_bounded_to_kernel_capacity() {
        let queue = ready_queue();

        assert_eq!(queue.capacity(), Some(READY_QUEUE_CAPACITY));
    }

    #[test]
    fn scheduler_captures_are_no_wider_than_spawner() {
        assert!(size_of::<GlobalScheduler<TestCpu, FixedHome>>() <= size_of::<Spawner<TestCpu>>());
        assert!(
            size_of::<GlobalScheduler<TestCpu, NoWeakArc<InstanceHome>>>()
                <= size_of::<Spawner<TestCpu>>()
        );
        assert!(size_of::<LocalScheduler<TestCpu>>() <= size_of::<Spawner<TestCpu>>());
        assert!(size_of::<LocalSilentScheduler<TestCpu>>() <= size_of::<Spawner<TestCpu>>());
        assert!(size_of::<LocalSilentScheduler<TestCpu>>() < size_of::<LocalScheduler<TestCpu>>());
    }

    #[test]
    fn global_batch_does_not_probe_empty_local_queue_per_task() {
        let _serialized = super::executor_test_guard();
        let executor = Executor::new(
            ProgressCounter::new(),
            single_node_pool(1),
            ProcessorId::new(0),
        );
        let timer = executor.timer(TestCpu::new());
        let spawner = executor.spawner(TestCpu::new());
        for _ in 0..8 {
            spawner.spawn_detached(async {});
        }

        let stats = executor.run_until_stalled_with_stats(&timer);

        assert_eq!(stats.global_runnable_count(), 8);
        assert_eq!(stats.local_runnable_count(), 0);
        assert_eq!(stats.global_empty_pop_count(), 1);
        assert_eq!(stats.local_empty_pop_count(), 1);
    }

    /// The arena a host test gets, which is the floor: nothing installed
    /// a boot memory plan, so `machine_usable_bytes()` is zero and every
    /// arena is [`TASK_ARENA_MIN_BYTES`].
    const TEST_ARENA_BYTES: usize = crate::memory::TASK_ARENA_MIN_BYTES;
    const TEST_SHARE_BYTES: usize = TEST_ARENA_BYTES - TASK_ARENA_KERNEL_RESERVE_BYTES;

    /// A task arena of `bytes` carved from a test pool's memory, as the
    /// executor carves its own.
    fn test_arena(bytes: usize) -> NoWeakArc<TaskArena> {
        let table = PerProcessor::new(single_node_pool(1), |_, local| {
            TaskArena::new_shared(bytes, local)
        });
        table.get(ProcessorId::new(0)).clone()
    }

    #[test]
    fn task_arena_reuses_blocks_released_around_pinned_tasks() {
        let arena = test_arena(TEST_ARENA_BYTES);
        // An eternal task pins the arena non-empty for the whole test,
        // matching the boot-time pump/transport tasks in a real kernel.
        let pinned = TaskArena::allocate_kernel(&arena, [0_u8; 256]);
        // Churn two orders of magnitude more bytes than the arena holds;
        // a bump pointer that never rewound exhausted after 1 MiB of
        // cumulative spawns and panicked on the next allocation.
        for _ in 0..200_000 {
            drop(TaskArena::allocate_kernel(&arena, [0_u8; 512]));
        }
        // Distinct live allocations still coexist with the reused blocks.
        let overlapped: [_; 8] =
            core::array::from_fn(|_| TaskArena::allocate_kernel(&arena, [0_u8; 512]));
        drop(overlapped);
        drop(pinned);
    }

    /// Issue #142: the share is bytes, not a set of per-class budgets.
    /// A share filled with one class and then emptied serves any other
    /// class, because a freed block is split for a smaller request and
    /// merged with its buddy for a larger one. On a bump arena with
    /// per-class free lists this refused every one of the small tasks
    /// with ~96% of the share free.
    #[test]
    fn freed_bytes_of_one_class_serve_a_different_class() {
        let arena = test_arena(TEST_ARENA_BYTES);
        let large_bytes = TaskArena::class_bytes(TaskArena::block_class(size_of::<[u8; 8192]>()));
        let small_bytes = TaskArena::class_bytes(TaskArena::block_class(size_of::<[u8; 256]>()));

        let mut large = Vec::new();
        while let Ok(task) = TaskArena::allocate_instance(&arena, [0_u8; 8192]) {
            large.push(task);
        }
        assert_eq!(large.len(), TEST_SHARE_BYTES / large_bytes);
        drop(large);

        // Every byte the large tasks held is available to the small
        // class, and to the large class again afterwards.
        let mut small = Vec::new();
        while let Ok(task) = TaskArena::allocate_instance(&arena, [0_u8; 256]) {
            small.push(task);
        }
        assert_eq!(small.len(), TEST_SHARE_BYTES / small_bytes);
        drop(small);

        let mut large_again = Vec::new();
        while let Ok(task) = TaskArena::allocate_instance(&arena, [0_u8; 8192]) {
            large_again.push(task);
        }
        assert_eq!(
            large_again.len(),
            TEST_SHARE_BYTES / large_bytes,
            "freed small blocks did not merge back into large ones"
        );
    }

    /// A task does not end where it started: a global task can finish
    /// on any processor. Its block is published on the arena's free
    /// stack from there and merged by the owner at its next allocation,
    /// so the bytes come back with no lock between the two processors.
    #[test]
    fn a_block_freed_off_the_owning_processor_comes_back_at_the_next_allocation() {
        let arena = test_arena(TEST_ARENA_BYTES);
        let mut live = Vec::new();
        while let Ok(task) = TaskArena::allocate_instance(&arena, [0_u8; 4096]) {
            live.push(task);
        }
        let released = live.pop().expect("the share held at least one task");
        let released_ptr = released.ptr.as_ptr().cast::<u8>();

        // The freeing processor is not the owner, and never touches the
        // owner's block metadata.
        let elsewhere = std::thread::spawn(move || drop(released));
        elsewhere.join().expect("the freeing thread panicked");

        let replacement = TaskArena::allocate_instance(&arena, [0_u8; 4096])
            .expect("the block freed elsewhere is the owner's again");
        assert_eq!(
            replacement.ptr.as_ptr().cast::<u8>(),
            released_ptr,
            "the owner served the next spawn from somewhere other than the freed block"
        );
    }

    /// The spawn storm from issue #94: a user program holding instance
    /// after instance until the share is gone. Before the instance
    /// share existed this panicked the kernel; now the storm is refused
    /// and the kernel's own tasks still have somewhere to go.
    #[test]
    fn instance_task_storm_is_refused_and_leaves_the_kernel_reserve_intact() {
        let arena = test_arena(TEST_ARENA_BYTES);
        let block = TaskArena::class_bytes(TaskArena::block_class(size_of::<[u8; 4096]>()));

        let mut live = Vec::new();
        loop {
            match TaskArena::allocate_instance(&arena, [0_u8; 4096]) {
                Ok(task) => live.push(task),
                Err(error) => {
                    assert_eq!(error.requested_bytes, block);
                    assert_eq!(error.share_bytes, TEST_SHARE_BYTES);
                    assert_eq!(error.live_instance_tasks, live.len());
                    break;
                }
            }
        }

        // The storm stops at the share, not at the arena.
        assert_eq!(live.len(), TEST_SHARE_BYTES / block);
        // And the kernel can still place a task of the largest class it
        // spawns — the property whose absence made a user-mode spawn
        // able to kill the machine.
        let kernel_task =
            TaskArena::allocate_kernel(&arena, [0_u8; TASK_ARENA_KERNEL_RESERVE_BYTES]);
        drop(kernel_task);
        drop(live);
    }

    /// However the share is fragmented, and however many instance
    /// spawns are refused, the reserve is untouched: it is a sub-arena
    /// of its own, so nothing splits a reserve block for an instance
    /// and no merge carries share bytes into it.
    #[test]
    fn the_reserve_is_never_consumed_by_instance_allocations() {
        let arena = test_arena(TEST_ARENA_BYTES);
        let mut small = Vec::new();
        let mut large = Vec::new();
        // Interleave two classes, then drop every other small task, so
        // the share ends up holed rather than uniformly full.
        loop {
            let Ok(large_task) = TaskArena::allocate_instance(&arena, [0_u8; 4096]) else {
                break;
            };
            large.push(large_task);
            let Ok(small_task) = TaskArena::allocate_instance(&arena, [0_u8; 128]) else {
                break;
            };
            small.push(small_task);
        }
        small.retain({
            let mut index = 0;
            move |_| {
                index += 1;
                index % 2 == 0
            }
        });
        // Keep asking until the share refuses, in every class.
        for _ in 0..64 {
            drop(TaskArena::allocate_instance(&arena, [0_u8; 64]));
            drop(TaskArena::allocate_instance(&arena, [0_u8; 8192]));
        }
        assert!(TaskArena::allocate_instance(&arena, [0_u8; 8192]).is_err());

        // The reserve is whole: it still serves a block of the largest
        // class the arena has.
        let kernel_task =
            TaskArena::allocate_kernel(&arena, [0_u8; TASK_ARENA_KERNEL_RESERVE_BYTES]);
        drop(kernel_task);
        drop(small);
        drop(large);
    }

    #[test]
    fn instance_tasks_reclaim_the_share_when_their_instances_exit() {
        let arena = test_arena(TEST_ARENA_BYTES);
        let mut live = Vec::new();
        while let Ok(task) = TaskArena::allocate_instance(&arena, [0_u8; 4096]) {
            live.push(task);
        }

        // One instance exiting is one refusal reversed: capacity is a
        // live-task property, not a cumulative-spawn property.
        live.pop();
        let replacement = TaskArena::allocate_instance(&arena, [0_u8; 4096]);

        assert!(replacement.is_ok());
        assert!(TaskArena::allocate_instance(&arena, [0_u8; 4096]).is_err());
    }

    /// A refusal is not a charge. The arena drops the future it could
    /// not place and records nothing, so the share still holds exactly
    /// what was live before the refusal and admits exactly one more
    /// task once one of them ends (#132).
    #[test]
    fn a_refused_instance_task_charges_nothing_to_the_share() {
        let arena = test_arena(TEST_ARENA_BYTES);
        let mut live = Vec::new();
        while let Ok(task) = TaskArena::allocate_instance(&arena, [0_u8; 4096]) {
            live.push(task);
        }

        let Err(refusal) = TaskArena::allocate_instance(&arena, [0_u8; 4096]) else {
            panic!("the share is full");
        };
        assert_eq!(refusal.live_instance_tasks, live.len());
        // Asking again after a refusal reports the same population: the
        // refused allocation took nothing it has to give back.
        let Err(repeated) = TaskArena::allocate_instance(&arena, [0_u8; 4096]) else {
            panic!("the share is full");
        };
        assert_eq!(repeated.live_instance_tasks, live.len());

        live.pop();
        let replacement =
            TaskArena::allocate_instance(&arena, [0_u8; 4096]).expect("one ended, one fits");
        drop(replacement);
        drop(live);
    }

    /// Kernel-funded churn must not hand the reserve to instances: a
    /// block the kernel took out of the reserve and released stays in
    /// the reserve's own buddy tree.
    #[test]
    fn released_kernel_reserve_blocks_never_reach_instance_tasks() {
        let arena = test_arena(TEST_ARENA_BYTES);
        let mut live = Vec::new();
        while let Ok(task) = TaskArena::allocate_instance(&arena, [0_u8; 4096]) {
            live.push(task);
        }
        // Reach into the reserve, then give the block back.
        drop(TaskArena::allocate_kernel(&arena, [0_u8; 4096]));

        assert!(TaskArena::allocate_instance(&arena, [0_u8; 4096]).is_err());
    }

    /// #159: what a processor can hold is the machine's, not a
    /// constant's. A guest with more memory gets a proportionally
    /// larger share and places proportionally more instance tasks.
    #[test]
    fn the_share_scales_with_the_boot_memory_plan() {
        /// What a QEMU `-m 2G` x86-64 guest's memory map came to (run
        /// 33943692491), and the same machine with four times it.
        const TWO_GIB_USABLE: usize = 1_977_962_496;
        let small = test_arena(task_arena_bytes(TWO_GIB_USABLE));
        let large = test_arena(task_arena_bytes(4 * TWO_GIB_USABLE));

        let place_until_refused = |arena: &_| {
            let mut live = Vec::new();
            while let Ok(task) = TaskArena::allocate_instance(arena, [0_u8; 8192]) {
                live.push(task);
            }
            live.len()
        };
        let small_tasks = place_until_refused(&small);
        let large_tasks = place_until_refused(&large);

        // The hundredth instance of #159 was refused at about ninety
        // launch tasks in a 768 KiB share. The same guest's share now
        // holds the launch tasks of hundreds of instances.
        assert!(
            small_tasks >= 500,
            "a 2 GiB guest's share held only {small_tasks} launch tasks"
        );
        assert!(
            large_tasks >= 3 * small_tasks,
            "four times the machine held {large_tasks} against {small_tasks}"
        );
    }

    #[test]
    fn task_arena_block_classes_round_up_and_reject_oversize() {
        assert_eq!(TaskArena::block_class(1), 0);
        assert_eq!(TaskArena::block_class(64), 0);
        assert_eq!(TaskArena::block_class(65), 1);
        assert_eq!(TaskArena::block_class(512), 3);
        assert_eq!(
            TaskArena::class_bytes(TASK_ARENA_CLASS_COUNT - 1),
            256 * 1024
        );
        assert_eq!(TASK_ARENA_TOP_BYTES, 256 * 1024);
    }

    #[test]
    fn a_parked_processor_is_woken_and_a_polling_one_is_not() {
        let _serialized = super::executor_test_guard();
        // This test thread is processor 0; the executor and its local
        // queue belong to processor 1.
        let cpu = RecordingSmpCpu::new(0, 2);
        let executor = Executor::new(
            ProgressCounter::new(),
            single_node_pool(2),
            ProcessorId::new(1),
        );
        let spawner = executor.spawner(cpu.clone());
        let queues = executor.group.queues(ProcessorId::new(1));
        let idle = &queues.idle;
        let drain = || {
            pop_ready(&queues.queue, &queues.ready)
                .expect("the runnable pushed for processor 1 is still queued")
                .run();
        };

        // Parked: the push needs the IPI.
        idle.store(IdleState::Parked);
        spawner.spawn_local_detached(async {});
        assert_eq!(cpu.woken(), [ProcessorId::new(1)]);

        // Polling: the processor sees its own ready count — no IPI.
        drain();
        idle.store(IdleState::Polling);
        spawner.spawn_local_detached(async {});
        assert_eq!(cpu.woken(), [ProcessorId::new(1)]);

        // Running: the executor drains the queue itself — no IPI.
        drain();
        idle.store(IdleState::Running);
        spawner.spawn_local_detached(async {});
        assert_eq!(cpu.woken(), [ProcessorId::new(1)]);
    }

    /// A sleep registered on processor 0's timer from processor 1
    /// programs no comparator on processor 1 — `set_deadline` reaches
    /// the executing processor, not the timer's owner — and reaches
    /// processor 0 through the idle handshake: a wake when it is parked,
    /// and its own arm before it parks when it was running.
    #[test]
    fn a_foreign_sleep_wakes_the_parked_owner_and_programs_no_other_comparator() {
        let _serialized = super::executor_test_guard();
        let cpu = RecordingSmpCpu::new(0, 2);
        let executor = Executor::new(
            ProgressCounter::new(),
            single_node_pool(2),
            ProcessorId::new(0),
        );
        let timer = executor.timer(cpu.clone());
        let idle = &executor.group.queues(ProcessorId::new(0)).idle;
        let built = cpu.deadlines();
        assert_eq!(built.len(), 1);
        assert_eq!(built[0].0, ProcessorId::new(0));
        let now = cpu.now().ticks();
        let mut context = Context::from_waker(Waker::noop());

        crate::test_processor_identity::set(ProcessorId::new(1));
        idle.store(IdleState::Parked);
        let mut while_parked = timer.sleep_until(Instant::new(now + 500));
        assert!(Pin::new(&mut while_parked).poll(&mut context).is_pending());
        assert_eq!(
            cpu.deadlines(),
            built,
            "a foreign publish programs no comparator"
        );
        assert_eq!(
            cpu.woken(),
            [ProcessorId::new(0)],
            "the parked owner is woken"
        );

        idle.store(IdleState::Running);
        let mut while_running = timer.sleep_until(Instant::new(now + 400));
        assert!(Pin::new(&mut while_running).poll(&mut context).is_pending());
        assert_eq!(
            cpu.deadlines(),
            built,
            "a foreign publish programs no comparator"
        );
        assert_eq!(
            cpu.woken(),
            [ProcessorId::new(0)],
            "a running owner reads the deadline itself"
        );

        crate::test_processor_identity::set(ProcessorId::new(0));
        timer.arm_published_deadline();
        assert_eq!(
            cpu.deadlines().last(),
            Some(&(ProcessorId::new(0), now + 400)),
            "the owner arms the earliest published deadline before it parks"
        );
    }

    /// A processor's deadline is the earlier of its next sleep and, while
    /// it is running work, the next scheduler tick (10 ms): its timer
    /// interrupt re-arms a tick on unless it is parked, when it stretches
    /// to the idle interval (100 ms), and the first executor pass that
    /// runs work after a park pulls it back within a tick, once, not on
    /// every pass.
    #[test]
    fn a_running_processor_is_interrupted_every_tick_and_a_parked_one_keeps_its_long_deadline() {
        let _serialized = super::executor_test_guard();
        let cpu = RecordingSmpCpu::new(0, 1);
        let executor = Executor::new(
            ProgressCounter::new(),
            single_node_pool(1),
            ProcessorId::new(0),
        );
        let timer = executor.timer(cpu.clone());
        let spawner = executor.spawner(cpu.clone());
        let idle = &executor.group.queues(ProcessorId::new(0)).idle;
        let now = cpu.now().ticks();
        let tick = cpu
            .timebase()
            .duration_to_ticks(core::time::Duration::from_millis(10));
        let long = cpu
            .timebase()
            .duration_to_ticks(core::time::Duration::from_millis(100));
        let armed = || cpu.deadlines().last().expect("a deadline was armed").1;
        let programmed = || cpu.deadlines().len();

        assert_eq!(armed(), now + tick, "a processor builds its timer running");

        idle.store(IdleState::Parked);
        timer.handle_interrupt();
        assert_eq!(
            armed(),
            now + long,
            "a parked processor stretches to the idle interval"
        );

        let parked_programs = programmed();
        assert_eq!(
            executor
                .run_until_stalled_with_stats(&timer)
                .runnable_count(),
            0
        );
        assert_eq!(
            programmed(),
            parked_programs,
            "a pass with no work keeps the long deadline"
        );

        idle.store(IdleState::Running);
        spawner.spawn_local_detached(async {});
        assert_eq!(
            executor
                .run_until_stalled_with_stats(&timer)
                .runnable_count(),
            1
        );
        assert_eq!(
            armed(),
            now + tick,
            "work after a park is interrupted within a tick"
        );
        let running_programs = programmed();
        for _ in 0..4 {
            spawner.spawn_local_detached(async {});
            executor.run_until_stalled_with_stats(&timer);
        }
        assert_eq!(
            programmed(),
            running_programs,
            "no arm per pass inside a tick"
        );

        timer.handle_interrupt();
        assert_eq!(
            armed(),
            now + tick,
            "a running processor's interrupt re-arms a tick on"
        );

        let mut context = Context::from_waker(Waker::noop());
        let mut soon = timer.sleep_until(Instant::new(now + tick / 2));
        assert!(Pin::new(&mut soon).poll(&mut context).is_pending());
        assert_eq!(
            armed(),
            now + tick / 2,
            "a sleep sooner than the tick is armed"
        );
        drop(soon);
        timer.fire_expired();

        let mut late = timer.sleep_until(Instant::new(now + long / 2));
        assert!(Pin::new(&mut late).poll(&mut context).is_pending());
        idle.store(IdleState::Parked);
        timer.handle_interrupt();
        assert_eq!(
            armed(),
            now + long / 2,
            "a parked processor wakes for its next sleep inside the idle interval"
        );
        idle.store(IdleState::Running);
    }

    #[test]
    fn the_poll_window_grows_after_a_short_park_and_shrinks_after_a_long_one() {
        let _serialized = super::executor_test_guard();
        // One tick per nanosecond, and `now()` steps a microsecond a
        // read, so a poll window of N ticks costs N/1000 loop passes.
        let cpu = SteppingClockCpu::new(1_000);
        let executor = Executor::new(
            ProgressCounter::new(),
            single_node_pool(2),
            ProcessorId::new(0),
        );
        let timer = executor.timer(cpu.clone());
        let idle = &executor.group.queues(ProcessorId::new(0)).idle;
        let grow_start = cpu.timebase().duration_to_ticks(IDLE_POLL_GROW_START);
        let max = cpu.timebase().duration_to_ticks(IDLE_POLL_MAX);
        assert_eq!(idle.poll_limit_ticks(), 0);

        // A park shorter than the maximum window earns a poll window,
        // which then doubles per short park up to the cap.
        cpu.set_park_advance(max / 20);
        assert!(matches!(
            executor.park_until_work(&timer, || false),
            IdleOutcome::Parked { .. }
        ));
        assert_eq!(idle.poll_limit_ticks(), grow_start);
        for expected in [grow_start * 2, max, max] {
            assert!(matches!(
                executor.park_until_work(&timer, || false),
                IdleOutcome::Parked { .. }
            ));
            assert_eq!(idle.poll_limit_ticks(), expected);
        }

        // A park longer than the maximum window halves it.
        cpu.set_park_advance(max * 50);
        assert!(matches!(
            executor.park_until_work(&timer, || false),
            IdleOutcome::Parked { .. }
        ));
        assert_eq!(idle.poll_limit_ticks(), max / 2);
    }

    /// The root future's readiness is tested where the ready counts
    /// are: a root wake published before the park returns `Polled`
    /// with nothing in any queue. `park_current` panics if reached.
    #[test]
    fn a_ready_root_future_is_seen_before_the_park() {
        let _serialized = super::executor_test_guard();
        let cpu = SteppingClockCpu::new(1_000);
        cpu.forbid_park();
        let executor = Executor::new(
            ProgressCounter::new(),
            single_node_pool(2),
            ProcessorId::new(0),
        );
        let timer = executor.timer(cpu.clone());
        let root_ready = AtomicBool::new(true);

        let outcome = executor.park_until_work(&timer, || root_ready.load(Ordering::SeqCst));

        assert!(matches!(outcome, IdleOutcome::Polled { .. }));
        assert_eq!(
            executor.group.queues(ProcessorId::new(0)).idle.state(),
            IdleState::Running
        );
    }

    /// The push that lands between the `Parked` store and the park is
    /// what the post-store re-test exists for: without it this parks
    /// under queued work. `park_current` panics if it is reached, so
    /// a `Polled` outcome is the re-test firing.
    #[test]
    fn work_pushed_after_parked_is_published_is_seen_before_the_park() {
        let _serialized = super::executor_test_guard();
        let cpu = SteppingClockCpu::new(1_000);
        cpu.forbid_park();
        let executor = Executor::new(
            ProgressCounter::new(),
            single_node_pool(2),
            ProcessorId::new(0),
        );
        let timer = executor.timer(cpu.clone());
        executor.spawner(cpu).spawn_local_detached(async {});

        let outcome = executor.park_until_work(&timer, || false);

        assert!(matches!(outcome, IdleOutcome::Polled { .. }));
        assert_eq!(
            executor.group.queues(ProcessorId::new(0)).idle.state(),
            IdleState::Running
        );
    }

    /// Two nodes, processor 0 on node 0 and processor 1 on node 1, remote
    /// at Linux's default distance.
    fn two_nodes() -> &'static UserMemoryPool {
        TestMachine {
            processor_nodes: alloc::vec![0, 1],
            distances: alloc::vec![10, 20, 20, 10],
        }
        .pool(TEST_POOL_NODE_BYTES)
    }

    /// Three nodes, one processor each, with an asymmetric matrix: from
    /// node 0, node 2 is nearer than node 1, and from node 1, node 2 is
    /// nearer than node 0.
    fn three_nodes() -> &'static UserMemoryPool {
        TestMachine {
            processor_nodes: alloc::vec![0, 1, 2],
            #[rustfmt::skip]
            distances: alloc::vec![
                10, 30, 20,
                30, 10, 20,
                20, 20, 10,
            ],
        }
        .pool(TEST_POOL_NODE_BYTES)
    }

    /// The executor of `owner` over a group of its own for the machine
    /// `locality` describes, rather than over the process-wide group, so
    /// the test chooses the machine.
    fn executor_on(pool: &'static UserMemoryPool, owner: ProcessorId) -> Executor {
        Executor::in_group(
            NoWeakArc::new(ExecutorGroup::new(pool)),
            ProgressCounter::new(),
            owner,
        )
    }

    /// Spawns, as `processor`, a kernel task that records `label` when it
    /// runs. A kernel task's home is the spawning processor's node.
    fn spawn_recording<CpuImpl: Cpu + Clone>(
        spawner: &Spawner<CpuImpl>,
        processor: u16,
        ran: &Arc<spin::Mutex<Vec<u16>>>,
        label: u16,
    ) {
        test_processor_identity::set(ProcessorId::new(processor));
        let ran = ran.clone();
        spawner.spawn_detached(async move { ran.lock().push(label) });
    }

    fn node_ready(executor: &Executor, node: usize) -> usize {
        executor.group.nodes[node].ready.load(Ordering::SeqCst)
    }

    #[test]
    fn a_processor_runs_its_own_nodes_tasks_before_another_nodes() {
        let executor = executor_on(two_nodes(), ProcessorId::new(0));
        let cpu = RecordingSmpCpu::new(0, 2);
        let timer = executor.timer(cpu.clone());
        let spawner = executor.spawner(cpu);
        let ran = Arc::new(spin::Mutex::new(Vec::new()));
        // Node 1's tasks are queued first; queueing order does not
        // decide.
        spawn_recording(&spawner, 1, &ran, 1);
        spawn_recording(&spawner, 1, &ran, 1);
        spawn_recording(&spawner, 0, &ran, 0);
        spawn_recording(&spawner, 0, &ran, 0);
        assert_eq!(node_ready(&executor, 0), 2);
        assert_eq!(node_ready(&executor, 1), 2);

        test_processor_identity::set(ProcessorId::new(0));
        let stats = executor.run_until_stalled_with_stats(&timer);

        assert_eq!(*ran.lock(), [0, 0, 1, 1]);
        assert_eq!(stats.global_runnable_count(), 4);
    }

    #[test]
    fn a_processor_steals_from_the_nearest_node_first() {
        let executor = executor_on(three_nodes(), ProcessorId::new(0));
        let cpu = RecordingSmpCpu::new(0, 3);
        let timer = executor.timer(cpu.clone());
        let spawner = executor.spawner(cpu);
        let ran = Arc::new(spin::Mutex::new(Vec::new()));
        spawn_recording(&spawner, 1, &ran, 1);
        spawn_recording(&spawner, 2, &ran, 2);

        test_processor_identity::set(ProcessorId::new(0));
        executor.run_until_stalled_with_stats(&timer);

        assert_eq!(
            *ran.lock(),
            [2, 1],
            "node 2 is at 20 from node 0, node 1 at 30"
        );
    }

    /// A push to a node whose only processor is the pusher has no
    /// candidate there, so it wakes the nearest node's processor. The
    /// first candidate decides, as within a node: a running one is not
    /// passed over for a parked one further away.
    #[test]
    fn a_push_to_the_pushers_lone_node_wakes_the_nearest_nodes_parked_processor() {
        let executor = executor_on(three_nodes(), ProcessorId::new(0));
        let cpu = RecordingSmpCpu::new(1, 3);
        let spawner = executor.spawner(cpu.clone());
        let idle = |processor: u16| &executor.group.queues(ProcessorId::new(processor)).idle;
        idle(0).store(IdleState::Parked);
        idle(2).store(IdleState::Parked);

        spawner.spawn_detached(async {});
        assert_eq!(node_ready(&executor, 1), 1);
        assert_eq!(cpu.woken(), [ProcessorId::new(2)]);

        idle(2).store(IdleState::Running);
        spawner.spawn_detached(async {});
        assert_eq!(
            cpu.woken(),
            [ProcessorId::new(2)],
            "a running candidate sees the count itself, and decides"
        );
    }

    /// A processor about to park re-tests every node's queue, because it
    /// steals from all of them. `park_current` panics if it is reached.
    #[test]
    fn work_on_another_node_keeps_a_processor_from_parking() {
        let cpu = SteppingClockCpu::new(1_000);
        cpu.forbid_park();
        let executor = executor_on(two_nodes(), ProcessorId::new(0));
        let timer = executor.timer(cpu.clone());
        let spawner = executor.spawner(cpu);
        test_processor_identity::set(ProcessorId::new(1));
        spawner.spawn_detached(async {});
        test_processor_identity::set(ProcessorId::new(0));
        assert_eq!(node_ready(&executor, 1), 1);

        let outcome = executor.park_until_work(&timer, || false);

        assert!(matches!(outcome, IdleOutcome::Polled { .. }));
    }

    /// An instance's tasks are queued on its home, whichever processor
    /// spawns them: provisionally the node of the processor that
    /// registered it, then the node its first turn ran on, which later
    /// turns elsewhere do not move.
    #[test]
    fn an_instances_tasks_queue_on_the_node_its_first_turn_ran_on() {
        let executor = executor_on(two_nodes(), ProcessorId::new(0));
        let cpu = RecordingSmpCpu::new(1, 2);
        let registry = InstanceRegistry::new();
        let instance = registry.register("homed", 0);
        let activity = InstanceActivity::new(instance.clone(), 0);
        let turns = activity.turns();
        test_processor_identity::set(ProcessorId::new(0));
        let timer = executor.timer(cpu.clone());
        let spawner = executor
            .spawner(cpu)
            .instance_spawner(TaskFunding::Instance, &instance);

        spawner
            .try_spawn_detached(async {})
            .expect("the instance share has room");
        assert_eq!(
            (node_ready(&executor, 0), node_ready(&executor, 1)),
            (0, 1),
            "before any turn the home is the registering processor's node"
        );

        turns.begin(1).end(2);
        test_processor_identity::set(ProcessorId::new(1));
        turns.begin(3).end(4);
        test_processor_identity::set(ProcessorId::new(0));
        spawner
            .try_spawn_detached(async {})
            .expect("the instance share has room");
        assert_eq!(
            (node_ready(&executor, 0), node_ready(&executor, 1)),
            (1, 1),
            "the first turn, on processor 0, fixed the home on node 0"
        );

        let stats = executor.run_until_stalled_with_stats(&timer);
        assert_eq!(
            stats.global_runnable_count(),
            2,
            "processor 0 steals node 1's task once its own node is empty"
        );
        drop(turns);
        drop(activity);
    }
}
