//! virtio-blk: the block device every backend brings up.
//!
//! The driver implements the whole request set the kernel needs — read,
//! write, cache flush, device identification, discard and write-zeroes —
//! and negotiates the feature bits that describe the device's geometry so
//! callers address it in its own logical blocks rather than in 512-byte
//! sectors the device may not use natively.
//!
//! # A request outlives the future that submitted it
//!
//! The device reads and writes request memory by DMA until it publishes
//! the answer, and nothing calls a request back once it is in the ring.
//! So the driver owns every request it hands the device, in a slot of the
//! queue's request table named by the chain's descriptor identifier: the
//! header, the status byte and the identifier bytes the device writes
//! live in the slot, and so does the caller's [`BlockBuffer`]. Whoever
//! drains the used ring finishes the requests it pops: it reads the
//! status, calls the buffer's [`BlockBuffer::complete`], and either hands
//! the result to the waiter or, when the waiter was dropped, drops the
//! buffer and frees the slot. Dropping a waiter only marks its slot
//! detached.
//!
//! That is Linux's shape. `virtblk_done` drains the virtqueue from the
//! vring interrupt and completes each request it pops, and the request's
//! bio, which holds its pages, runs `bi_end_io` from there whether or
//! not a task still waits; io_uring's owned buffers, as tokio-uring and
//! monoio use them, keep a cancelled operation's buffer with the kernel's
//! completion rather than with the future.
//!
//! # Concurrency contract
//!
//! The device is programmed once during single-processor bring-up.
//! Afterwards every request goes through one of the per-processor
//! queues. Each queue's ring is a spin lock held only to write one chain
//! and kick the device, or to pop what the device published; it is never
//! held across an `.await` and never while a request is finished.
//!
//! The ring is drained by the interrupt handler and by any task that
//! waits on a request or for room on the queue. The interrupt handler
//! only ever *tries* the ring lock: when it finds it held, it leaves a
//! drain request the holder honours as it lets go, so an answer is never
//! stranded and the handler never spins on a lock its own processor may
//! hold. A request slot moves through idle, pending, complete and
//! detached with single atomic transitions, so finishing a request — in
//! interrupt context or in a task — takes no lock: the waiter and the
//! drainer settle who frees the slot with one compare-and-swap. A
//! request never migrates between queues: the queue is chosen when the
//! chain is written and the request finishes on that same queue.

use alloc::boxed::Box;
use alloc::vec::Vec;
use arrayvec::ArrayVec;
use async_lock::Mutex as AsyncMutex;
use atomic_waker::AtomicWaker;
use core::cell::UnsafeCell;
use core::cmp;
use core::future::Future;
use core::hint::spin_loop;
use core::mem::{size_of, size_of_val};
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering, fence};
use core::task::{Context, Poll};
use crossbeam_utils::CachePadded;
use spin::{Mutex, MutexGuard};

use helios_hal::cpu::{Cpu, current_processor};
use helios_hal::fs::{
    BlockBuffer, BlockCompletion, BlockDevice, BlockDeviceCapabilities, BlockDeviceRights,
    BlockGeometry, BlockQueueTopology, BlockRange, BlockSerial, OwnedBlockBuffer,
};
use helios_hal::io::{IoError, IoResult};
use helios_hal::resource::KernelResource;
use helios_hal::vmm::SwapBackend;

use crate::features::{NegotiatedFeatures, RING_FEATURES, negotiate, renegotiate};
use crate::notify::Notify;
use crate::queue::{MAX_CHAIN_BUFFERS, VirtQueue};
use crate::resume::{DeviceReset, QuiesceError, ReinitError, Resumed, conclude, driver_ok, settle};
use crate::transport::{DeviceStatus, DeviceType, VirtioTransport};

/// virtio-blk addresses every request in 512-byte sectors regardless of
/// the logical block size the device reports (virtio 1.2 §5.2.6).
pub const SECTOR_SIZE: usize = 512;

/// Longest request pipeline the driver keeps on one queue.
///
/// The depth is what makes the device useful under concurrent load: each
/// in-flight chain owns a descriptor identifier and a request slot, and
/// a deeper queue only helps once the device can actually overlap that
/// many requests. 128 is the depth QEMU's virtio-blk exposes by default,
/// so the driver never asks for more ring than the device offers.
const BLOCK_MAX_QUEUE_SIZE: u16 = 128;
const BLOCK_QUEUE_SLOTS: usize = BLOCK_MAX_QUEUE_SIZE as usize;

/// Segments one discard or write-zeroes request carries.
///
/// Every segment is a 16-byte descriptor of a sector run, and all of them
/// travel in a single read-only buffer, so this bounds an array in the
/// request slot rather than a descriptor chain.
const DEALLOCATE_SEGMENTS: usize = 8;

/// Bytes of the identifier a device reports for `VIRTIO_BLK_T_GET_ID`.
const BLOCK_ID_BYTES: usize = 20;

/// What a request's status byte holds until the device writes it. Not a
/// status virtio defines, so a device that answers without writing one
/// is reported as a fault.
const STATUS_UNWRITTEN: u8 = 0xff;

// virtio 1.2 §5.2.3: virtio-blk feature bits.
const BLK_F_SIZE_MAX: u64 = 1 << 1;
const BLK_F_SEG_MAX: u64 = 1 << 2;
const BLK_F_RO: u64 = 1 << 5;
const BLK_F_BLK_SIZE: u64 = 1 << 6;
const BLK_F_FLUSH: u64 = 1 << 9;
const BLK_F_TOPOLOGY: u64 = 1 << 10;
const BLK_F_CONFIG_WCE: u64 = 1 << 11;
const BLK_F_MQ: u64 = 1 << 12;
const BLK_F_DISCARD: u64 = 1 << 13;
const BLK_F_WRITE_ZEROES: u64 = 1 << 14;

/// Every virtio-blk feature this driver implements.
const BLK_FEATURES: u64 = BLK_F_SIZE_MAX
    | BLK_F_SEG_MAX
    | BLK_F_RO
    | BLK_F_BLK_SIZE
    | BLK_F_FLUSH
    | BLK_F_TOPOLOGY
    | BLK_F_CONFIG_WCE
    | BLK_F_MQ
    | BLK_F_DISCARD
    | BLK_F_WRITE_ZEROES;

// virtio 1.2 §5.2.4: `struct virtio_blk_config` field offsets.
const CFG_CAPACITY_LOW: usize = 0;
const CFG_CAPACITY_HIGH: usize = 4;
const CFG_SIZE_MAX: usize = 8;
const CFG_SEG_MAX: usize = 12;
const CFG_BLK_SIZE: usize = 20;
const CFG_PHYSICAL_BLOCK_EXP: usize = 24;
const CFG_MIN_IO_SIZE: usize = 26;
const CFG_OPT_IO_SIZE: usize = 28;
const CFG_WRITEBACK: usize = 32;
const CFG_NUM_QUEUES: usize = 34;
const CFG_MAX_DISCARD_SECTORS: usize = 36;
const CFG_MAX_DISCARD_SEG: usize = 40;
const CFG_MAX_WRITE_ZEROES_SECTORS: usize = 48;
const CFG_MAX_WRITE_ZEROES_SEG: usize = 52;

// The states of a request slot.
/// No request uses the slot.
const IDLE: u8 = 0;
/// The device has the request; a waiter may be parked on it.
const PENDING: u8 = 1;
/// The request is finished and its result waits for the waiter.
const COMPLETE: u8 = 2;
/// The device has the request and nobody waits for it: whoever drains
/// its answer frees the slot.
const DETACHED: u8 = 3;

// The states of the device as the driver sees it.
/// The device keeps the used-ring contract; requests are accepted.
const RUNNING: u8 = 0;
/// The device broke the used-ring contract and is being reset; it may
/// still be writing into the buffers it was handed.
const RESETTING: u8 = 1;
/// The reset is complete, so the device touches no buffer any more:
/// every request still in flight is finished with a device fault.
const STOPPED: u8 = 2;

/// How a multiqueue driver decides which queue a request belongs on.
///
/// The driver needs exactly two facts about the platform — how many
/// processors there are and which one is running the caller — so this is
/// the contract it asks for rather than the whole [`Cpu`] surface. Every
/// backend's `Cpu` satisfies it; tests supply a two-line fake.
pub trait QueueAffinity: Send + Sync + 'static {
    /// Index of the processor running the caller.
    fn current_processor(&self) -> usize;

    /// Processors the platform exposes.
    fn processor_count(&self) -> usize;
}

impl<C: Cpu> QueueAffinity for C {
    fn current_processor(&self) -> usize {
        usize::from(current_processor().id())
    }

    fn processor_count(&self) -> usize {
        Cpu::processor_count(self)
    }
}

/// virtio 1.2 §5.2.6: request types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
enum ReqType {
    In = 0,
    Out = 1,
    Flush = 4,
    GetId = 8,
    Discard = 11,
    WriteZeroes = 13,
}

/// Everything one request slot holds besides its state.
///
/// Written by the submitter while the slot is idle, by the device while
/// the request is pending, by whoever drains the answer while it is
/// pending or detached, and read by the waiter once it is complete. The
/// state word, and the ring lock the submitter and the drainer hold,
/// order those accesses.
struct RequestBody<B> {
    kind: ReqType,
    header: BlkReq,
    deallocate: [BlkDeallocate; DEALLOCATE_SEGMENTS],
    deallocate_len: usize,
    id: [u8; BLOCK_ID_BYTES],
    status: u8,
    /// Payload bytes the device must write for a read to be whole.
    expected: usize,
    buffer: Option<B>,
    /// Payload bytes the device wrote, once the request is finished.
    result: IoResult<usize>,
}

/// One request the driver handed the device, in the slot its chain's
/// descriptor identifier names.
struct Request<B> {
    state: AtomicU8,
    /// The waiter's, while it is parked on a pending request.
    waker: AtomicWaker,
    body: UnsafeCell<RequestBody<B>>,
}

// SAFETY: `body` is touched by one context at a time, as the slot's state
// and the ring lock decide (see `RequestBody`); every other field is
// atomic.
unsafe impl<B: Send> Sync for Request<B> {}

impl<B> Request<B> {
    fn idle() -> Self {
        Self {
            state: AtomicU8::new(IDLE),
            waker: AtomicWaker::new(),
            body: UnsafeCell::new(RequestBody {
                kind: ReqType::Flush,
                header: BlkReq::default(),
                deallocate: [BlkDeallocate::default(); DEALLOCATE_SEGMENTS],
                deallocate_len: 0,
                id: [0; BLOCK_ID_BYTES],
                status: STATUS_UNWRITTEN,
                expected: 0,
                buffer: None,
                result: Ok(0),
            }),
        }
    }
}

/// One request queue: the ring, and the requests in flight on it.
struct BlockQueue<T: VirtioTransport, B> {
    ring: Mutex<VirtQueue<T>>,
    /// Set by a drainer that found the ring locked; whoever holds it
    /// drains on the drainer's behalf as it lets go.
    drain_pending: AtomicBool,
    /// One slot per descriptor identifier the ring hands out.
    requests: Box<[Request<B>]>,
    /// Submitters parked because the ring is full or because the slot
    /// they would be handed is still occupied.
    blocked: AtomicUsize,
    /// Wakes those submitters: a chain or a slot was freed.
    available: Notify,
}

impl<T: VirtioTransport, B> BlockQueue<T, B> {
    /// Reports that `count` chains or slots became free.
    ///
    /// The notification is a broadcast, so one is enough however many
    /// were freed: every parked submitter re-tests the ring and its slot
    /// for itself. Nothing is published when no submitter is parked.
    fn note_released(&self, count: usize) {
        if count == 0 || self.blocked.load(Ordering::SeqCst) == 0 {
            return;
        }
        self.available.notify_all();
    }

    fn request(&self, id: u16) -> &Request<B> {
        self.requests.get(usize::from(id)).unwrap_or_else(|| {
            panic!(
                "virtio-blk descriptor {id} is outside the {}-slot request table",
                self.requests.len()
            )
        })
    }
}

/// Requests the device has completed, by kind.
#[derive(Debug, Default)]
struct RequestCounters {
    reads: AtomicU64,
    writes: AtomicU64,
    flushes: AtomicU64,
    discards: AtomicU64,
    write_zeroes: AtomicU64,
}

impl RequestCounters {
    fn snapshot(&self) -> BlockRequestCounts {
        BlockRequestCounts {
            reads: self.reads.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
            flushes: self.flushes.load(Ordering::Relaxed),
            discards: self.discards.load(Ordering::Relaxed),
            write_zeroes: self.write_zeroes.load(Ordering::Relaxed),
        }
    }

    fn count(&self, request: ReqType) {
        let counter = match request {
            ReqType::In | ReqType::GetId => &self.reads,
            ReqType::Out => &self.writes,
            ReqType::Flush => &self.flushes,
            ReqType::Discard => &self.discards,
            ReqType::WriteZeroes => &self.write_zeroes,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Requests this device has issued, by kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockRequestCounts {
    pub reads: u64,
    pub writes: u64,
    pub flushes: u64,
    pub discards: u64,
    pub write_zeroes: u64,
}

/// The request shapes the device accepts, decoded from its configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RequestLimits {
    /// 512-byte sectors in one logical block.
    sectors_per_block: usize,
    /// Data buffers one request chain may carry.
    data_segments: usize,
    /// Bytes one data buffer may carry.
    segment_bytes: usize,
    /// Sectors one discard segment may cover.
    discard_sectors: u32,
    /// Discard segments one request may carry.
    discard_segments: usize,
    /// Sectors one write-zeroes segment may cover.
    write_zeroes_sectors: u32,
    /// Write-zeroes segments one request may carry.
    write_zeroes_segments: usize,
}

/// The data runs of one request, as device-visible memory: addresses and
/// lengths, cut at the device's segment size.
type DataRuns = ArrayVec<(usize, usize), MAX_CHAIN_BUFFERS>;

/// What one request asks of the device, besides the caller's buffer.
struct Command {
    kind: ReqType,
    sector: u64,
    deallocate: ArrayVec<BlkDeallocate, DEALLOCATE_SEGMENTS>,
    runs: DataRuns,
}

impl Command {
    fn bare(kind: ReqType) -> Self {
        Self {
            kind,
            sector: 0,
            deallocate: ArrayVec::new(),
            runs: ArrayVec::new(),
        }
    }

    /// Buffers the chain carries: the header, the payload and the status
    /// byte.
    fn buffers(&self) -> usize {
        let payload = match self.kind {
            ReqType::In | ReqType::Out => self.runs.len(),
            ReqType::GetId => 1,
            ReqType::Discard | ReqType::WriteZeroes => 1,
            ReqType::Flush => 0,
        };
        2 + payload
    }
}

pub struct VirtioBlockDevice<T: VirtioTransport, C: QueueAffinity, B> {
    transport: T,
    cpu: C,
    /// One queue per processor when the device offers enough of them.
    /// A request is bound to the queue it was written into, so a task
    /// that migrates still collects its answer where it left it.
    queues: Box<[CachePadded<BlockQueue<T, B>>]>,
    features: NegotiatedFeatures,
    geometry: BlockGeometry,
    capabilities: BlockDeviceCapabilities,
    limits: RequestLimits,
    /// Whether the device's write cache is write-back, as it reported.
    writeback: bool,
    readonly: bool,
    counters: RequestCounters,
    /// [`RUNNING`], [`RESETTING`] or [`STOPPED`].
    state: AtomicU8,
}

pub struct VirtioBlockResource<T: VirtioTransport, C: QueueAffinity, B> {
    resource: KernelResource<VirtioBlockDevice<T, C, B>, BlockDeviceRights>,
}

pub struct VirtioBlockSwapBackend<D: BlockDevice> {
    device: D,
    state: AsyncMutex<VirtioBlockSwapState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtioBlockSwapToken {
    start_block: usize,
    block_count: usize,
    byte_len: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VirtioBlockSwapError {
    #[error("swap payload is empty")]
    EmptyPayload,
    #[error("swap device reports zero-sized blocks")]
    InvalidBlockSize,
    #[error("swap extent is empty")]
    EmptyExtent,
    #[error(
        "swap extent [{start_block}, +{block_count}) exceeds device block count {device_blocks}"
    )]
    ExtentOutOfBounds {
        start_block: usize,
        block_count: usize,
        device_blocks: usize,
    },
    #[error("swap device has {available_blocks} free blocks, requested {requested_blocks}")]
    OutOfSwap {
        requested_blocks: usize,
        available_blocks: usize,
    },
    #[error("swap-in destination length {actual} does not match token byte length {expected}")]
    InvalidDestination { expected: usize, actual: usize },
    #[error("a swap payload of {len} bytes does not fit one block request buffer")]
    PayloadTooLarge { len: usize },
    #[error("swap block I/O failed: {0}")]
    Io(#[from] IoError),
}

#[derive(Default)]
struct VirtioBlockSwapState {
    free: Vec<SwapExtent>,
}

#[derive(Clone, Copy)]
struct SwapExtent {
    start_block: usize,
    block_count: usize,
}

/// virtio 1.2 §5.2.6: `struct virtio_blk_req` header.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BlkReq {
    type_: u32,
    reserved: u32,
    sector: u64,
}

impl BlkReq {
    fn new(request: ReqType, sector: u64) -> Self {
        Self {
            type_: (request as u32).to_le(),
            reserved: 0,
            sector: sector.to_le(),
        }
    }
}

/// virtio 1.2 §5.2.6: `struct virtio_blk_discard_write_zeroes`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BlkDeallocate {
    sector: u64,
    num_sectors: u32,
    flags: u32,
}

impl BlkDeallocate {
    fn new(sector: u64, num_sectors: u32) -> Self {
        Self {
            sector: sector.to_le(),
            num_sectors: num_sectors.to_le(),
            // Neither discard nor write-zeroes asks the device to unmap:
            // the kernel uses discard to say the blocks are free and
            // write-zeroes to make them read back as zeroes, and the
            // unmap flag is only meaningful for the latter.
            flags: 0,
        }
    }
}

/// What a finished request hands its waiter.
struct Finished<B> {
    result: IoResult<usize>,
    buffer: Option<B>,
    id: [u8; BLOCK_ID_BYTES],
}

/// Where a [`RequestWaiter`] stands.
enum Flight<B> {
    /// The device has the request in slot `id` of queue `queue`.
    Issued { queue: usize, id: u16 },
    /// The driver refused the request before the device saw it; the
    /// buffer was completed with the error already.
    Refused { error: IoError, buffer: Option<B> },
    /// The waiter took its answer.
    Collected,
}

/// Waits for one request's answer. Dropping it detaches the request,
/// which then finishes on its own.
struct RequestWaiter<'a, T: VirtioTransport, C: QueueAffinity, B: BlockBuffer> {
    device: &'a VirtioBlockDevice<T, C, B>,
    flight: Flight<B>,
}

// The waiter never pins the buffer it may hold: it only moves it out.
impl<T: VirtioTransport, C: QueueAffinity, B: BlockBuffer> Unpin for RequestWaiter<'_, T, C, B> {}

impl<'a, T: VirtioTransport, C: QueueAffinity, B: BlockBuffer> RequestWaiter<'a, T, C, B> {
    /// A request the driver refused: the buffer hears the error now, and
    /// the waiter answers it at once.
    fn refused(
        device: &'a VirtioBlockDevice<T, C, B>,
        error: IoError,
        mut buffer: Option<B>,
    ) -> Self {
        if let Some(buffer) = buffer.as_mut() {
            buffer.complete(Err(error));
        }
        Self {
            device,
            flight: Flight::Refused { error, buffer },
        }
    }

    /// Takes the answer out of a complete slot and frees it.
    fn collect(&mut self, queue: usize, id: u16) -> Finished<B> {
        let queue = &self.device.queues[queue];
        let request = queue.request(id);
        // SAFETY: the slot is complete, so the drainer is done with the
        // body and nobody else touches it until it is idle again.
        let body = unsafe { &mut *request.body.get() };
        let finished = Finished {
            result: body.result,
            buffer: body.buffer.take(),
            id: body.id,
        };
        request.state.store(IDLE, Ordering::Release);
        queue.note_released(1);
        self.flight = Flight::Collected;
        finished
    }
}

impl<T: VirtioTransport, C: QueueAffinity, B: BlockBuffer> Future for RequestWaiter<'_, T, C, B> {
    type Output = Finished<B>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match &mut this.flight {
            Flight::Collected => panic!("a virtio-blk request was polled after it was answered"),
            Flight::Refused { error, buffer } => {
                let finished = Finished {
                    result: Err(*error),
                    buffer: buffer.take(),
                    id: [0; BLOCK_ID_BYTES],
                };
                this.flight = Flight::Collected;
                Poll::Ready(finished)
            }
            &mut Flight::Issued { queue, id } => {
                let block_queue = &this.device.queues[queue];
                let request = block_queue.request(id);
                // Registered before the state is tested, so a drainer
                // that completes the request in between wakes this task.
                request.waker.register(context.waker());
                if request.state.load(Ordering::Acquire) != COMPLETE {
                    // Drain on everyone's behalf: the answer may already
                    // be in the used ring with no interrupt to come.
                    this.device.reap(block_queue);
                    if request.state.load(Ordering::Acquire) != COMPLETE {
                        return Poll::Pending;
                    }
                }
                Poll::Ready(this.collect(queue, id))
            }
        }
    }
}

impl<T: VirtioTransport, C: QueueAffinity, B: BlockBuffer> Drop for RequestWaiter<'_, T, C, B> {
    fn drop(&mut self) {
        let Flight::Issued { queue, id } = self.flight else {
            return;
        };
        let request = self.device.queues[queue].request(id);
        match request
            .state
            .compare_exchange(PENDING, DETACHED, Ordering::AcqRel, Ordering::Acquire)
        {
            // Whoever drains the answer frees the slot.
            Ok(_) => {}
            // Finished already: its buffer was completed, and goes now.
            Err(COMPLETE) => drop(self.collect(queue, id)),
            Err(state) => {
                panic!("virtio-blk request {id} was abandoned in slot state {state}")
            }
        }
    }
}

/// The waiter of one data request: the [`BlockDevice::Waiter`] of
/// [`VirtioBlockResource`].
pub struct VirtioBlockWaiter<'a, T: VirtioTransport, C: QueueAffinity, B: BlockBuffer> {
    request: RequestWaiter<'a, T, C, B>,
}

impl<T: VirtioTransport, C: QueueAffinity, B: BlockBuffer> Future
    for VirtioBlockWaiter<'_, T, C, B>
{
    type Output = BlockCompletion<B>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.get_mut().request)
            .poll(context)
            .map(|finished| BlockCompletion {
                result: finished.result.map(drop),
                buffer: finished
                    .buffer
                    .expect("a virtio-blk data request carries a buffer"),
            })
    }
}

impl<T: VirtioTransport, C: QueueAffinity, B: BlockBuffer> VirtioBlockDevice<T, C, B> {
    pub fn new(transport: T, cpu: C) -> IoResult<Self> {
        if transport.device_type() != DeviceType::Block {
            return Err(IoError::Unsupported);
        }

        let features = negotiate(&transport, RING_FEATURES | BLK_FEATURES)?;

        let DeviceConfig {
            geometry,
            limits,
            writeback,
        } = read_config(&transport, features)?;
        let RequestLimits {
            data_segments,
            segment_bytes,
            ..
        } = limits;
        let logical_block_bytes = geometry.logical_block_bytes;
        let capacity_blocks = geometry.capacity_blocks;

        let mut capabilities = BlockDeviceCapabilities::empty();
        if features.device(BLK_F_FLUSH) {
            capabilities |= BlockDeviceCapabilities::FLUSH;
        }
        if features.device(BLK_F_DISCARD) {
            capabilities |= BlockDeviceCapabilities::DISCARD;
        }
        if features.device(BLK_F_WRITE_ZEROES) {
            capabilities |= BlockDeviceCapabilities::WRITE_ZEROES;
        }

        let queue_count = queue_count(&transport, &cpu, features);
        let mut queues = Vec::with_capacity(queue_count);
        for index in 0..queue_count {
            let index = u16::try_from(index).map_err(|_| IoError::DeviceFault)?;
            let queue_size = transport.queue_max_size(index).min(BLOCK_MAX_QUEUE_SIZE);
            if queue_size == 0 || !queue_size.is_power_of_two() {
                return Err(IoError::Unsupported);
            }
            let chain_limit = u16::try_from(data_segments + 2).map_err(|_| IoError::DeviceFault)?;
            // A ring that cannot hold one whole request would make every
            // submission wait for descriptors that can never arrive.
            if queue_size < chain_limit {
                return Err(IoError::InvalidDeviceConfig(
                    "virtio-blk queue is too small to carry one request chain",
                ));
            }
            queues.push(CachePadded::new(BlockQueue {
                ring: Mutex::new(VirtQueue::new(
                    &transport,
                    index,
                    queue_size,
                    chain_limit,
                    features,
                )?),
                drain_pending: AtomicBool::new(false),
                requests: (0..queue_size).map(|_| Request::idle()).collect(),
                blocked: AtomicUsize::new(0),
                available: Notify::new(),
            }));
        }

        transport.set_status(
            DeviceStatus::ACKNOWLEDGE
                | DeviceStatus::DRIVER
                | DeviceStatus::FEATURES_OK
                | DeviceStatus::DRIVER_OK,
        );

        let readonly = features.device(BLK_F_RO);
        tracing::info!(
            capacity_blocks,
            block_bytes = logical_block_bytes,
            physical_block_bytes = geometry.physical_block_bytes(),
            queues = queues.len(),
            queue_depth = usize::from(BLOCK_MAX_QUEUE_SIZE),
            segments = data_segments,
            segment_bytes,
            readonly,
            flush = capabilities.contains(BlockDeviceCapabilities::FLUSH),
            discard = capabilities.contains(BlockDeviceCapabilities::DISCARD),
            write_zeroes = capabilities.contains(BlockDeviceCapabilities::WRITE_ZEROES),
            writeback,
            "virtio-blk configured"
        );

        Ok(Self {
            transport,
            cpu,
            queues: queues.into_boxed_slice(),
            features,
            geometry,
            capabilities,
            limits,
            writeback,
            readonly,
            counters: RequestCounters::default(),
            state: AtomicU8::new(RUNNING),
        })
    }

    /// The feature set this device negotiated.
    pub fn features(&self) -> NegotiatedFeatures {
        self.features
    }

    /// Geometry the device reported for itself.
    pub fn geometry(&self) -> BlockGeometry {
        self.geometry
    }

    /// Operations the device implements beyond read and write.
    pub fn capabilities(&self) -> BlockDeviceCapabilities {
        self.capabilities
    }

    /// Whether the device refuses writes.
    pub fn is_readonly(&self) -> bool {
        self.readonly
    }

    /// Request queues in use: one per processor when the device offered
    /// enough of them.
    pub fn queue_count(&self) -> usize {
        self.queues.len()
    }

    /// Requests one queue may keep in flight.
    pub fn queue_depth(&self) -> usize {
        BLOCK_QUEUE_SLOTS
    }

    /// Requests this device has issued, by kind.
    pub fn request_counts(&self) -> BlockRequestCounts {
        self.counters.snapshot()
    }

    pub fn into_resource(self, rights: BlockDeviceRights) -> VirtioBlockResource<T, C, B> {
        VirtioBlockResource {
            resource: KernelResource::new(self, rights),
        }
    }

    pub fn new_resource(
        transport: T,
        cpu: C,
        rights: BlockDeviceRights,
    ) -> IoResult<VirtioBlockResource<T, C, B>> {
        Self::new(transport, cpu).map(|device| device.into_resource(rights))
    }

    /// The device's interrupt: acknowledges it and finishes every request
    /// the device answered.
    ///
    /// This is the completion path, as `virtblk_done` is Linux's: it
    /// runs each answered buffer's [`BlockBuffer::complete`] and frees
    /// the slots nobody waits on, so a request finishes whether or not
    /// its submitter is still there. It never spins on a ring lock: a
    /// ring another context holds is drained by that context as it lets
    /// go.
    ///
    /// The transport acknowledges the whole interrupt-status word, so a
    /// configuration-change notification (bit 1 of the ISR, virtio 1.2
    /// §4.1.4.5) is cleared but not decoded: the capacity this driver
    /// reports is the one the device published when it was opened.
    /// Re-reading it needs the transport to report which bit fired,
    /// which is a change to the interrupt path rather than to this
    /// driver.
    pub fn handle_interrupt(&self) {
        self.transport.ack_interrupt();
        for queue in self.queues.iter() {
            self.reap(queue);
        }
    }

    /// Waits for every request the device owns, on every queue, to
    /// complete, so the device may be reset. See [`crate::resume`] for
    /// the contract; `expired` says when the caller's deadline has
    /// passed.
    ///
    /// Each answer is finished the way the interrupt path finishes it:
    /// its buffer hears its result and its waiter is woken, so the waiter
    /// finds a complete slot when the machine runs again. A device the
    /// driver already stopped holds nothing.
    pub fn quiesce(&self, mut expired: impl FnMut() -> bool) -> Result<(), QuiesceError> {
        let device = DeviceType::Block;
        for (index, queue) in self.queues.iter().enumerate() {
            let index = u16::try_from(index).expect("queue indices were built from a u16");
            settle(device, index, &mut expired, || {
                if self.state.load(Ordering::SeqCst) != RUNNING {
                    return Ok(0);
                }
                self.reap(queue);
                queue
                    .ring
                    .try_lock()
                    .map(|ring| ring.in_flight_chains())
                    .ok_or(QuiesceError::QueueBusy {
                        device,
                        queue: index,
                    })
            })?;
        }
        Ok(())
    }

    /// Brings a reset device back with the features it had, after
    /// [`Self::quiesce`]: every queue is re-programmed at its ring with
    /// fresh indices. Block keeps no state outside its rings, so there is
    /// nothing else to restore; what the device reports about itself is
    /// compared with what the driver brought up.
    pub fn reinitialize(&self) -> Result<DeviceReset, ReinitError> {
        if self.state.load(Ordering::SeqCst) != RUNNING {
            return Err(ReinitError::StateNotRestorable {
                device: DeviceType::Block,
                what: "a device the driver stopped after it broke the used-ring contract",
            });
        }
        conclude(&self.transport, self.restore(), |resumed| {
            if resumed == Resumed::Failed {
                // Nothing is in flight after a quiesce, so what has to
                // stop is the next request: the driver refuses every
                // one from here, and a submitter parked for room is woken
                // to see that.
                self.state.store(STOPPED, Ordering::SeqCst);
                for queue in self.queues.iter() {
                    queue.available.notify_all();
                }
            }
        })
    }

    fn restore(&self) -> Result<DeviceReset, ReinitError> {
        let device = DeviceType::Block;
        renegotiate(&self.transport, self.features)?;
        self.check_identity()?;
        for (index, queue) in self.queues.iter().enumerate() {
            let index = u16::try_from(index).expect("queue indices were built from a u16");
            let mut ring = queue.ring.try_lock().ok_or(ReinitError::QueueBusy {
                device,
                queue: index,
            })?;
            ring.reinitialize(&self.transport)?;
            drop(ring);
            queue.drain_pending.store(false, Ordering::SeqCst);
        }
        driver_ok(&self.transport);
        Ok(DeviceReset { device })
    }

    /// Compares what the device reports about itself with what the
    /// driver brought up. A device is untrusted, and a disk of another
    /// size is not the disk the image was taken against.
    fn check_identity(&self) -> Result<(), ReinitError> {
        let device = DeviceType::Block;
        let changed = |what| ReinitError::DeviceChanged { device, what };
        let found = read_config(&self.transport, self.features)
            .map_err(|_| changed("its configuration space"))?;
        let (was, now) = (&self.geometry, &found.geometry);
        if now.logical_block_bytes != was.logical_block_bytes {
            return Err(changed("logical block size"));
        }
        if now.capacity_blocks != was.capacity_blocks {
            return Err(changed("capacity"));
        }
        if (now.physical_block_exp, now.min_io_blocks, now.opt_io_blocks)
            != (was.physical_block_exp, was.min_io_blocks, was.opt_io_blocks)
        {
            return Err(changed("topology"));
        }
        let (was, now) = (&self.limits, &found.limits);
        if (now.data_segments, now.segment_bytes) != (was.data_segments, was.segment_bytes) {
            return Err(changed("segment limits"));
        }
        if (now.discard_sectors, now.discard_segments)
            != (was.discard_sectors, was.discard_segments)
        {
            return Err(changed("discard limits"));
        }
        if (now.write_zeroes_sectors, now.write_zeroes_segments)
            != (was.write_zeroes_sectors, was.write_zeroes_segments)
        {
            return Err(changed("write-zeroes limits"));
        }
        if found.writeback != self.writeback {
            return Err(changed("write cache mode"));
        }
        if queue_count(&self.transport, &self.cpu, self.features) != self.queues.len() {
            return Err(changed("queue count"));
        }
        Ok(())
    }

    /// The first sector of a logical block.
    fn sector_of(&self, block_id: usize) -> u64 {
        (block_id as u64) * self.limits.sectors_per_block as u64
    }

    /// Bytes one data segment carries: the device's segment limit,
    /// rounded down to whole logical blocks so that a request cut at a
    /// segment boundary still ends on a block.
    fn segment_stride(&self) -> usize {
        let block_bytes = self.geometry.logical_block_bytes;
        self.limits.segment_bytes / block_bytes * block_bytes
    }

    /// Cuts `buffer` into the data segments one request carries, and
    /// answers them with their total length. A buffer that needs more
    /// segments than one request carries is refused.
    fn data_runs(&self, buffer: &B) -> IoResult<(DataRuns, usize)> {
        let stride = self.segment_stride();
        let mut runs = DataRuns::new();
        let mut total = 0_usize;
        for segment in buffer.segments() {
            let mut address = segment.cast::<u8>().as_ptr() as usize;
            let mut len = segment.len();
            if len == 0 {
                return Err(IoError::InvalidBufferLength {
                    required_multiple: self.geometry.logical_block_bytes,
                    actual: 0,
                });
            }
            while len != 0 {
                let take = len.min(stride);
                if runs.len() == self.limits.data_segments {
                    return Err(IoError::OutOfBounds);
                }
                runs.push((address, take));
                address += take;
                len -= take;
                total = total.checked_add(take).ok_or(IoError::OutOfBounds)?;
            }
        }
        Ok((runs, total))
    }

    /// Checks a data request against the handle's rights and the device,
    /// and builds its command.
    fn data_command(
        &self,
        kind: ReqType,
        rights: BlockDeviceRights,
        block_id: usize,
        buffer: &B,
    ) -> IoResult<Command> {
        let required = match kind {
            ReqType::In => BlockDeviceRights::READ,
            _ => BlockDeviceRights::WRITE,
        };
        if !rights.contains(required) {
            return Err(IoError::PermissionDenied);
        }
        let (runs, len) = self.data_runs(buffer)?;
        validate_request(rights, self.geometry, block_id, len, required)?;
        if kind == ReqType::Out && self.readonly {
            return Err(IoError::ReadOnly);
        }
        Ok(Command {
            kind,
            sector: self.sector_of(block_id),
            deallocate: ArrayVec::new(),
            runs,
        })
    }

    /// Submits a data request, or refuses it with its buffer completed.
    async fn submit_data(
        &self,
        kind: ReqType,
        rights: BlockDeviceRights,
        block_id: usize,
        buffer: B,
    ) -> RequestWaiter<'_, T, C, B> {
        match self.data_command(kind, rights, block_id, &buffer) {
            Ok(command) => self.issue(command, Some(buffer)).await,
            Err(error) => RequestWaiter::refused(self, error, Some(buffer)),
        }
    }

    async fn flush_cache(&self) -> IoResult<()> {
        // virtio 1.2 §5.2.5: without VIRTIO_BLK_F_FLUSH the device has no
        // volatile write cache, so a completed write is already durable
        // and there is nothing to commit.
        if !self.capabilities.contains(BlockDeviceCapabilities::FLUSH) {
            return Ok(());
        }
        self.issue(Command::bare(ReqType::Flush), None)
            .await
            .await
            .result
            .map(drop)
    }

    async fn read_serial(&self) -> IoResult<BlockSerial> {
        let finished = self.issue(Command::bare(ReqType::GetId), None).await.await;
        let reported = finished.result?;
        BlockSerial::new(&finished.id[..reported]).ok_or(IoError::DeviceFault)
    }

    /// Issues one deallocation request kind over `range`.
    ///
    /// Both discard and write-zeroes carry the same payload: a list of
    /// sector runs in a single read-only buffer, bounded by the per-run
    /// and per-request limits the device reported.
    async fn deallocate(&self, request: ReqType, range: BlockRange) -> IoResult<()> {
        let (capability, max_sectors, max_segments) = match request {
            ReqType::Discard => (
                BlockDeviceCapabilities::DISCARD,
                self.limits.discard_sectors,
                self.limits.discard_segments,
            ),
            ReqType::WriteZeroes => (
                BlockDeviceCapabilities::WRITE_ZEROES,
                self.limits.write_zeroes_sectors,
                self.limits.write_zeroes_segments,
            ),
            other => panic!("{other:?} is not a virtio-blk deallocation request"),
        };
        if !self.capabilities.contains(capability) {
            return Err(IoError::Unsupported);
        }
        if range.is_empty() {
            return Ok(());
        }

        let mut sector = self.sector_of(range.start_block);
        let mut remaining = (range.block_count as u64) * self.limits.sectors_per_block as u64;
        while remaining != 0 {
            let mut command = Command::bare(request);
            while remaining != 0 && command.deallocate.len() < max_segments {
                let run = cmp::min(remaining, u64::from(max_sectors));
                let run = u32::try_from(run).map_err(|_| IoError::DeviceFault)?;
                command.deallocate.push(BlkDeallocate::new(sector, run));
                sector += u64::from(run);
                remaining -= u64::from(run);
            }
            self.issue(command, None).await.await.result?;
        }
        Ok(())
    }

    /// Writes one request into a slot of this processor's queue and kicks
    /// the device, answering the waiter of that slot.
    ///
    /// Two conditions make a submission wait, and neither is a device
    /// fault: the ring has no room for the chain, or the slot the queue
    /// would hand out next still holds an answer its waiter has not
    /// collected. Either way the submitter drains what the device has
    /// published, which frees descriptors and finishes requests, and then
    /// parks until a chain or a slot is freed. `buffer` stays with this
    /// future until the device has the request, so dropping the future
    /// before then drops it uncompleted.
    async fn issue(&self, command: Command, buffer: Option<B>) -> RequestWaiter<'_, T, C, B> {
        let queue_index = self.cpu.current_processor() % self.queues.len();
        let queue = &self.queues[queue_index];
        let buffers = command.buffers();
        let mut buffer = buffer;
        let mut announced = false;
        let outcome = loop {
            // Armed before the ring is tested: a release that lands in
            // between must not be slept through.
            let notified = queue.available.notified();
            {
                let mut ring = queue.ring.lock();
                // Tested under the ring lock: a reset that lands later
                // finds this request pending when it takes the lock.
                if self.state.load(Ordering::SeqCst) != RUNNING {
                    self.unlock(queue, ring);
                    break Err((IoError::DeviceFault, buffer.take()));
                }
                let id = ring.next_free_descriptor();
                let request = queue.request(id);
                if ring.has_room_for(buffers) && request.state.load(Ordering::Acquire) == IDLE {
                    // SAFETY: the slot is idle and this task holds the
                    // ring that hands its identifier out, so nobody else
                    // touches the body until the state says otherwise.
                    let body = unsafe { &mut *request.body.get() };
                    body.kind = command.kind;
                    body.header = BlkReq::new(command.kind, command.sector);
                    body.deallocate[..command.deallocate.len()]
                        .copy_from_slice(&command.deallocate);
                    body.deallocate_len = command.deallocate.len();
                    body.status = STATUS_UNWRITTEN;
                    body.expected = match command.kind {
                        ReqType::In => command.runs.iter().map(|&(_, len)| len).sum(),
                        _ => 0,
                    };
                    body.buffer = buffer.take();
                    body.result = Ok(0);
                    match write_chain(&mut ring, &self.transport, body, &command.runs) {
                        Ok(token) => {
                            assert_eq!(
                                token, id,
                                "virtio-blk chain took descriptor {token}, not the slot {id} it was written for"
                            );
                            // Pending before the ring is let go: no drain
                            // can pop the answer before this.
                            request.state.store(PENDING, Ordering::Release);
                            ring.notify(&self.transport);
                            self.unlock(queue, ring);
                            break Ok(id);
                        }
                        Err(error) => {
                            let buffer = body.buffer.take();
                            self.unlock(queue, ring);
                            break Err((error, buffer));
                        }
                    }
                }
                self.unlock(queue, ring);
            }
            if self.reap(queue) != 0 {
                continue;
            }
            if !announced {
                // Announced before the ring is re-tested, so a drainer
                // that frees it in between cannot read the count as
                // zero and leave this task asleep.
                queue.blocked.fetch_add(1, Ordering::SeqCst);
                announced = true;
                continue;
            }
            notified.await;
        };
        if announced {
            queue.blocked.fetch_sub(1, Ordering::SeqCst);
        }
        match outcome {
            Ok(id) => RequestWaiter {
                device: self,
                flight: Flight::Issued {
                    queue: queue_index,
                    id,
                },
            },
            Err((error, buffer)) => RequestWaiter::refused(self, error, buffer),
        }
    }

    /// Lets go of `queue`'s ring, draining it first if a drainer found it
    /// held meanwhile.
    fn unlock(&self, queue: &BlockQueue<T, B>, ring: MutexGuard<'_, VirtQueue<T>>) {
        drop(ring);
        // Pairs with the fence in `reap`: either that drainer's request is
        // seen here, or its next attempt finds the ring free.
        fence(Ordering::SeqCst);
        if queue.drain_pending.load(Ordering::SeqCst) {
            self.reap(queue);
        }
    }

    /// Pops every answer the device published on `queue` and finishes
    /// those requests, answering how many it finished.
    ///
    /// Never spins on the ring lock: a ring someone else holds is left
    /// to its holder with a drain request.
    fn reap(&self, queue: &BlockQueue<T, B>) -> usize {
        let mut finished = 0;
        loop {
            queue.drain_pending.store(true, Ordering::SeqCst);
            fence(Ordering::SeqCst);
            let Some(mut ring) = queue.ring.try_lock() else {
                return finished;
            };
            queue.drain_pending.store(false, Ordering::SeqCst);
            match self.state.load(Ordering::SeqCst) {
                RUNNING => {}
                // The context resetting the device drains every queue
                // once the reset is done.
                RESETTING => return finished,
                _ => {
                    finished += self.fail_in_flight(queue);
                    drop(ring);
                    queue.available.notify_all();
                    return finished;
                }
            }
            let mut answered: ArrayVec<(u16, u32), BLOCK_QUEUE_SLOTS> = ArrayVec::new();
            let drained = ring.drain_used(|id, used| answered.push((id, used)));
            drop(ring);
            // Every popped chain gave its descriptors back to the ring;
            // answers popped before a fault stand.
            let mut released = answered.len();
            for &(id, used) in &answered {
                released += self.finish(queue, id, used);
            }
            queue.note_released(released);
            finished += answered.len();
            if drained.is_err() {
                self.stop();
                return finished;
            }
            fence(Ordering::SeqCst);
            if !queue.drain_pending.load(Ordering::SeqCst) {
                return finished;
            }
        }
    }

    /// Finishes the request in slot `id`, which the device answered with
    /// `used` bytes written: reads its status, completes its buffer, and
    /// hands the result to its waiter or, with none left, frees the slot.
    /// Answers how many slots it freed.
    fn finish(&self, queue: &BlockQueue<T, B>, id: u16, used: u32) -> usize {
        let request = queue.request(id);
        // SAFETY: the device answered and this context popped the answer,
        // which no other context can; the waiter reads the body only once
        // the state says complete. The body is this context's.
        let body = unsafe { &mut *request.body.get() };
        // SAFETY: the status byte is in the body this context owns; the
        // device wrote it by DMA, so it is read as such.
        let status = unsafe { core::ptr::read_volatile(&body.status) };
        let result = map_block_status(status).and_then(|()| {
            // The used length counts everything the device wrote into the
            // chain, which is the payload plus the status byte.
            let written = usize::try_from(used)
                .map_err(|_| IoError::DeviceFault)?
                .checked_sub(size_of::<u8>())
                .ok_or(IoError::DeviceFault)?;
            match body.kind {
                // A short read is a device fault rather than a partial
                // success: the caller asked for whole blocks, and the
                // tail would otherwise keep whatever was in the buffer.
                ReqType::In if written != body.expected => Err(IoError::DeviceFault),
                ReqType::GetId => Ok(written.min(BLOCK_ID_BYTES)),
                _ => Ok(written),
            }
        });
        body.result = result;
        if let Some(buffer) = body.buffer.as_mut() {
            buffer.complete(result.map(drop));
        }
        self.counters.count(body.kind);
        match request
            .state
            .compare_exchange(PENDING, COMPLETE, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                request.waker.wake();
                0
            }
            Err(DETACHED) => {
                drop(body.buffer.take());
                request.state.store(IDLE, Ordering::Release);
                1
            }
            Err(state) => {
                panic!("virtio-blk device answered descriptor {id}, whose slot is in state {state}")
            }
        }
    }

    /// Gives up on a device that broke the used-ring contract.
    ///
    /// The device is reset first, because only a device that has been
    /// reset is done with the buffers it was handed (virtio 1.2, device
    /// status field: a reset is complete once the status reads back
    /// zero). Then every request still in flight on every queue finishes
    /// with a device fault and hands its buffer back, and the driver
    /// refuses every request from then on.
    fn stop(&self) {
        if self
            .state
            .compare_exchange(RUNNING, RESETTING, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        tracing::error!("virtio-blk device broke the used-ring contract; resetting it");
        self.transport.reset();
        while !self.transport.status().is_empty() {
            spin_loop();
        }
        self.state.store(STOPPED, Ordering::SeqCst);
        // A queue someone else holds is failed by its holder as it lets
        // go, through the drain request `reap` leaves.
        for queue in self.queues.iter() {
            self.reap(queue);
        }
    }

    /// Finishes every request in flight on `queue` with a device fault,
    /// answering how many it finished. The caller holds the ring, and
    /// the device has been reset.
    fn fail_in_flight(&self, queue: &BlockQueue<T, B>) -> usize {
        let mut failed = 0;
        for request in queue.requests.iter() {
            if !matches!(request.state.load(Ordering::Acquire), PENDING | DETACHED) {
                continue;
            }
            // SAFETY: the device has the request and the ring is held, so
            // no other drainer touches the body, and the reset device
            // writes into it no more; a waiter reads it only once complete.
            let body = unsafe { &mut *request.body.get() };
            body.result = Err(IoError::DeviceFault);
            if let Some(buffer) = body.buffer.as_mut() {
                buffer.complete(Err(IoError::DeviceFault));
            }
            match request.state.compare_exchange(
                PENDING,
                COMPLETE,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => request.waker.wake(),
                Err(DETACHED) => {
                    drop(body.buffer.take());
                    request.state.store(IDLE, Ordering::Release);
                }
                Err(state) => {
                    panic!("virtio-blk request was failed in slot state {state}")
                }
            }
            failed += 1;
        }
        failed
    }
}

/// Writes one request's chain from its slot: the header, the payload,
/// and the status byte last.
fn write_chain<T: VirtioTransport, B>(
    ring: &mut VirtQueue<T>,
    transport: &T,
    body: &mut RequestBody<B>,
    runs: &DataRuns,
) -> IoResult<u16> {
    let RequestBody {
        kind,
        header,
        deallocate,
        deallocate_len,
        id,
        status,
        ..
    } = body;
    let mut inputs: ArrayVec<&[u8], MAX_CHAIN_BUFFERS> = ArrayVec::new();
    let mut outputs: ArrayVec<&mut [u8], MAX_CHAIN_BUFFERS> = ArrayVec::new();
    inputs.push(as_bytes(header));
    match kind {
        ReqType::Out => {
            for &(address, len) in runs {
                // SAFETY: the run is part of the buffer the slot owns,
                // which the contract leaves unwritten until it completes.
                inputs.push(unsafe { core::slice::from_raw_parts(address as *const u8, len) });
            }
        }
        ReqType::In => {
            for &(address, len) in runs {
                // SAFETY: the run is part of the buffer the slot owns,
                // which the contract leaves to the device alone until it
                // completes.
                outputs.push(unsafe { core::slice::from_raw_parts_mut(address as *mut u8, len) });
            }
        }
        ReqType::GetId => outputs.push(id),
        ReqType::Discard | ReqType::WriteZeroes => {
            inputs.push(as_bytes_slice(&deallocate[..*deallocate_len]));
        }
        ReqType::Flush => {}
    }
    outputs.push(core::slice::from_mut(status));
    ring.submit(transport, &inputs, &mut outputs)
}

impl<T: VirtioTransport, C: QueueAffinity, B> Drop for VirtioBlockDevice<T, C, B> {
    fn drop(&mut self) {
        for queue in self.queues.iter_mut() {
            queue.ring.get_mut().shutdown(&self.transport);
        }
    }
}

/// Queues to program: one per processor, bounded by what the device
/// offers. Without VIRTIO_BLK_F_MQ the device has exactly one.
fn queue_count<T: VirtioTransport, C: QueueAffinity>(
    transport: &T,
    cpu: &C,
    features: NegotiatedFeatures,
) -> usize {
    if !features.device(BLK_F_MQ) {
        return 1;
    }
    let device_queues = usize::from(transport.read_config_u16(CFG_NUM_QUEUES));
    device_queues.clamp(1, cpu.processor_count().max(1))
}

/// Everything the device's configuration space says that the driver
/// builds requests or reports geometry from.
///
/// Read once at bring-up and again after a reset, where the two are
/// compared: a device that reports anything else is not the one the
/// driver's requests were sized for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DeviceConfig {
    geometry: BlockGeometry,
    limits: RequestLimits,
    writeback: bool,
}

/// Reads and validates the device's configuration space.
fn read_config<T: VirtioTransport>(
    transport: &T,
    features: NegotiatedFeatures,
) -> IoResult<DeviceConfig> {
    let capacity_sectors = u64::from(transport.read_config_u32(CFG_CAPACITY_LOW))
        | (u64::from(transport.read_config_u32(CFG_CAPACITY_HIGH)) << 32);
    let logical_block_bytes = if features.device(BLK_F_BLK_SIZE) {
        usize::try_from(transport.read_config_u32(CFG_BLK_SIZE))
            .map_err(|_| IoError::DeviceFault)?
    } else {
        SECTOR_SIZE
    };
    if logical_block_bytes == 0 || !logical_block_bytes.is_multiple_of(SECTOR_SIZE) {
        return Err(IoError::InvalidDeviceConfig(
            "virtio-blk logical block size is not a multiple of the 512-byte sector",
        ));
    }
    let sectors_per_block = logical_block_bytes / SECTOR_SIZE;
    let capacity_blocks =
        usize::try_from(capacity_sectors).map_err(|_| IoError::DeviceFault)? / sectors_per_block;

    let (physical_block_exp, min_io_blocks, opt_io_blocks) = if features.device(BLK_F_TOPOLOGY) {
        (
            transport.read_config_u8(CFG_PHYSICAL_BLOCK_EXP),
            u32::from(transport.read_config_u16(CFG_MIN_IO_SIZE)),
            transport.read_config_u32(CFG_OPT_IO_SIZE),
        )
    } else {
        (0, 0, 0)
    };

    // virtio 1.2 §5.2.5: a device that does not offer SEG_MAX states
    // no limit of its own, and the driver has to pick one; a single
    // data buffer is the only value that is correct for every device.
    let device_segments = if features.device(BLK_F_SEG_MAX) {
        transport.read_config_u32(CFG_SEG_MAX).max(1)
    } else {
        1
    };
    let data_segments = usize::try_from(device_segments)
        .unwrap_or(usize::MAX)
        .min(MAX_CHAIN_BUFFERS - 2);
    let segment_bytes = if features.device(BLK_F_SIZE_MAX) {
        let size_max = transport.read_config_u32(CFG_SIZE_MAX);
        if size_max == 0 {
            return Err(IoError::InvalidDeviceConfig(
                "virtio-blk offered SIZE_MAX with a zero maximum segment size",
            ));
        }
        usize::try_from(size_max).unwrap_or(usize::MAX)
    } else {
        usize::try_from(u32::MAX).unwrap_or(usize::MAX)
    };
    if segment_bytes < logical_block_bytes {
        return Err(IoError::InvalidDeviceConfig(
            "virtio-blk cannot carry a whole logical block in one segment",
        ));
    }

    let (discard_sectors, discard_segments) = if features.device(BLK_F_DISCARD) {
        deallocate_limits(
            transport,
            CFG_MAX_DISCARD_SECTORS,
            CFG_MAX_DISCARD_SEG,
            "discard",
        )?
    } else {
        (0, 0)
    };
    let (write_zeroes_sectors, write_zeroes_segments) = if features.device(BLK_F_WRITE_ZEROES) {
        deallocate_limits(
            transport,
            CFG_MAX_WRITE_ZEROES_SECTORS,
            CFG_MAX_WRITE_ZEROES_SEG,
            "write-zeroes",
        )?
    } else {
        (0, 0)
    };
    let writeback =
        features.device(BLK_F_CONFIG_WCE) && transport.read_config_u8(CFG_WRITEBACK) != 0;

    Ok(DeviceConfig {
        geometry: BlockGeometry {
            logical_block_bytes,
            physical_block_exp,
            min_io_blocks,
            opt_io_blocks,
            // What one request this driver builds can carry, which is
            // fewer than the device allows once the chain bound applies.
            max_segments: u32::try_from(data_segments).unwrap_or(u32::MAX),
            max_segment_bytes: u32::try_from(segment_bytes).unwrap_or(u32::MAX),
            capacity_blocks,
        },
        limits: RequestLimits {
            sectors_per_block,
            data_segments,
            segment_bytes,
            discard_sectors,
            discard_segments,
            write_zeroes_sectors,
            write_zeroes_segments,
        },
        writeback,
    })
}

/// Reads the per-run and per-request limits of a deallocation request
/// kind the device just claimed to support.
fn deallocate_limits<T: VirtioTransport>(
    transport: &T,
    sectors_offset: usize,
    segments_offset: usize,
    kind: &'static str,
) -> IoResult<(u32, usize)> {
    let sectors = transport.read_config_u32(sectors_offset);
    let segments = transport.read_config_u32(segments_offset);
    if sectors == 0 || segments == 0 {
        tracing::error!(kind, sectors, segments, "virtio-blk deallocation limit");
        return Err(IoError::InvalidDeviceConfig(
            "virtio-blk offered a deallocation feature with a zero limit",
        ));
    }
    Ok((
        sectors,
        usize::try_from(segments)
            .unwrap_or(usize::MAX)
            .min(DEALLOCATE_SEGMENTS),
    ))
}

impl<D: BlockDevice> VirtioBlockSwapBackend<D> {
    pub fn new(
        device: D,
        start_block: usize,
        block_count: usize,
    ) -> Result<Self, VirtioBlockSwapError> {
        let device_blocks = device.block_count();
        if device.block_size() == 0 {
            return Err(VirtioBlockSwapError::InvalidBlockSize);
        }
        if block_count == 0 {
            return Err(VirtioBlockSwapError::EmptyExtent);
        }
        let end_block = start_block.checked_add(block_count).ok_or(
            VirtioBlockSwapError::ExtentOutOfBounds {
                start_block,
                block_count,
                device_blocks,
            },
        )?;
        if end_block > device_blocks {
            return Err(VirtioBlockSwapError::ExtentOutOfBounds {
                start_block,
                block_count,
                device_blocks,
            });
        }

        Ok(Self {
            device,
            state: AsyncMutex::new(VirtioBlockSwapState {
                free: Vec::from([SwapExtent {
                    start_block,
                    block_count,
                }]),
            }),
        })
    }

    pub fn from_entire_device(device: D) -> Result<Self, VirtioBlockSwapError> {
        let block_count = device.block_count();
        Self::new(device, 0, block_count)
    }

    async fn allocate_token(
        &self,
        byte_len: usize,
    ) -> Result<VirtioBlockSwapToken, VirtioBlockSwapError> {
        if byte_len == 0 {
            return Err(VirtioBlockSwapError::EmptyPayload);
        }
        let block_size = self.device.block_size();
        if block_size == 0 {
            return Err(VirtioBlockSwapError::InvalidBlockSize);
        }
        let block_count = byte_len.div_ceil(block_size);
        let mut state = self.state.lock().await;
        let start_block =
            state
                .allocate(block_count)
                .ok_or_else(|| VirtioBlockSwapError::OutOfSwap {
                    requested_blocks: block_count,
                    available_blocks: state.available_blocks(),
                })?;
        Ok(VirtioBlockSwapToken {
            start_block,
            block_count,
            byte_len,
        })
    }

    async fn release_token(&self, token: VirtioBlockSwapToken) {
        self.state.lock().await.release(SwapExtent {
            start_block: token.start_block,
            block_count: token.block_count,
        });
    }
}

impl VirtioBlockSwapState {
    fn allocate(&mut self, requested_blocks: usize) -> Option<usize> {
        let index = self
            .free
            .iter()
            .position(|extent| extent.block_count >= requested_blocks)?;
        let extent = &mut self.free[index];
        let start_block = extent.start_block;
        extent.start_block += requested_blocks;
        extent.block_count -= requested_blocks;
        if extent.block_count == 0 {
            self.free.swap_remove(index);
        }
        Some(start_block)
    }

    fn release(&mut self, extent: SwapExtent) {
        if extent.block_count == 0 {
            return;
        }
        self.free.push(extent);
        self.free.sort_by_key(|extent| extent.start_block);

        let mut index = 0;
        while index + 1 < self.free.len() {
            let current = self.free[index];
            let next = self.free[index + 1];
            let current_end = current.start_block + current.block_count;
            if current_end >= next.start_block {
                let next_end = next.start_block + next.block_count;
                self.free[index].block_count =
                    cmp::max(current_end, next_end) - current.start_block;
                self.free.remove(index + 1);
            } else {
                index += 1;
            }
        }
    }

    fn available_blocks(&self) -> usize {
        self.free.iter().map(|extent| extent.block_count).sum()
    }
}

impl<T: VirtioTransport, C: QueueAffinity, B: BlockBuffer> VirtioBlockResource<T, C, B> {
    pub fn rights(&self) -> BlockDeviceRights {
        self.resource.rights()
    }

    pub fn derive(&self, rights: BlockDeviceRights) -> Option<Self> {
        self.resource
            .derive(rights)
            .map(|resource| Self { resource })
    }

    pub fn handle_interrupt(&self) {
        self.resource.object().handle_interrupt();
    }

    /// Waits for every request the device owns to complete. See
    /// [`VirtioBlockDevice::quiesce`].
    pub fn quiesce(&self, expired: impl FnMut() -> bool) -> Result<(), QuiesceError> {
        self.object().quiesce(expired)
    }

    /// Brings a reset device back. See [`VirtioBlockDevice::reinitialize`].
    pub fn reinitialize(&self) -> Result<DeviceReset, ReinitError> {
        self.object().reinitialize()
    }

    /// The feature set the underlying device negotiated.
    pub fn features(&self) -> NegotiatedFeatures {
        self.object().features()
    }

    /// Request queues in use.
    pub fn queue_count(&self) -> usize {
        self.object().queue_count()
    }

    /// Requests one queue may keep in flight.
    pub fn queue_depth(&self) -> usize {
        self.object().queue_depth()
    }

    /// Requests issued so far, by kind.
    pub fn request_counts(&self) -> BlockRequestCounts {
        self.object().request_counts()
    }

    fn object(&self) -> &VirtioBlockDevice<T, C, B> {
        self.resource.object()
    }

    /// Checks the rights and the addressing of a request that names a
    /// range of blocks.
    fn authorize_range(
        &self,
        range: BlockRange,
        required_right: BlockDeviceRights,
    ) -> IoResult<()> {
        if !self.rights().contains(required_right) {
            return Err(IoError::PermissionDenied);
        }
        if required_right.contains(BlockDeviceRights::WRITE) && self.object().readonly {
            return Err(IoError::ReadOnly);
        }
        let end_block = range.end_block().ok_or(IoError::OutOfBounds)?;
        if end_block > self.object().geometry.capacity_blocks {
            return Err(IoError::OutOfBounds);
        }
        Ok(())
    }
}

impl<T: VirtioTransport, C: QueueAffinity, B> Clone for VirtioBlockResource<T, C, B> {
    fn clone(&self) -> Self {
        Self {
            resource: self.resource.clone(),
        }
    }
}

impl<T: VirtioTransport, C: QueueAffinity, B: BlockBuffer> BlockDevice
    for VirtioBlockResource<T, C, B>
{
    type Buffer = B;
    type Waiter<'a>
        = VirtioBlockWaiter<'a, T, C, B>
    where
        Self: 'a;

    async fn submit_read(&self, block_id: usize, buffer: B) -> Self::Waiter<'_> {
        VirtioBlockWaiter {
            request: self
                .object()
                .submit_data(ReqType::In, self.rights(), block_id, buffer)
                .await,
        }
    }

    async fn submit_write(&self, block_id: usize, buffer: B) -> Self::Waiter<'_> {
        VirtioBlockWaiter {
            request: self
                .object()
                .submit_data(ReqType::Out, self.rights(), block_id, buffer)
                .await,
        }
    }

    async fn flush(&self) -> IoResult<()> {
        if !self.rights().contains(BlockDeviceRights::WRITE) {
            return Err(IoError::PermissionDenied);
        }
        self.object().flush_cache().await
    }

    async fn discard(&self, range: BlockRange) -> IoResult<()> {
        self.authorize_range(range, BlockDeviceRights::WRITE)?;
        self.object().deallocate(ReqType::Discard, range).await
    }

    async fn write_zeroes(&self, range: BlockRange) -> IoResult<()> {
        self.authorize_range(range, BlockDeviceRights::WRITE)?;
        self.object().deallocate(ReqType::WriteZeroes, range).await
    }

    async fn serial(&self) -> IoResult<BlockSerial> {
        if !self.rights().contains(BlockDeviceRights::READ) {
            return Err(IoError::PermissionDenied);
        }
        self.object().read_serial().await
    }

    fn geometry(&self) -> BlockGeometry {
        self.object().geometry
    }

    fn capabilities(&self) -> BlockDeviceCapabilities {
        self.object().capabilities
    }

    fn queue_topology(&self) -> BlockQueueTopology {
        BlockQueueTopology {
            queues: self.object().queue_count(),
            depth: self.object().queue_depth(),
        }
    }

    /// Writes are refused when the device negotiated
    /// `VIRTIO_BLK_F_RO` or this handle was derived without the write
    /// right: either way no write through it can succeed.
    fn is_read_only(&self) -> bool {
        self.object().readonly || !self.rights().contains(BlockDeviceRights::WRITE)
    }
}

impl<D> SwapBackend for VirtioBlockSwapBackend<D>
where
    D: BlockDevice + 'static,
    D::Buffer: OwnedBlockBuffer,
{
    type Token = VirtioBlockSwapToken;
    type Error = VirtioBlockSwapError;

    /// Writes `bytes` into a freshly allocated extent.
    ///
    /// The caller lends its bytes for the call only, and the request has
    /// to own what the device reads until the device answers, so the
    /// bytes are copied into a buffer of the device's, zero-padded to
    /// whole blocks. No flush follows. Swap is scratch storage whose only
    /// reader is the kernel that wrote it: once the device has taken the
    /// write it will hand the same bytes back, and nothing in swap has to
    /// outlive the kernel. A flush per page would buy durability nobody
    /// wants at the price of a host fsync on every page evicted.
    async fn swap_out(&self, bytes: &[u8]) -> Result<Self::Token, Self::Error> {
        let token = self.allocate_token(bytes.len()).await?;
        if let Err(error) = self.write_extent(token, bytes).await {
            self.release_token(token).await;
            return Err(error);
        }
        Ok(token)
    }

    /// Reads a swapped-out page back into `dst` and frees its extent.
    ///
    /// The device fills a buffer of its own, which is copied into `dst`
    /// once it answered. The extent is released only once the page is
    /// back: a failed read leaves the token valid so the caller can try
    /// again rather than losing the only copy of the page.
    async fn swap_in(&self, token: Self::Token, dst: &mut [u8]) -> Result<(), Self::Error> {
        if dst.len() != token.byte_len {
            return Err(VirtioBlockSwapError::InvalidDestination {
                expected: token.byte_len,
                actual: dst.len(),
            });
        }
        let buffer = self.extent_buffer(token)?;
        let read = self.device.read(token.start_block, buffer).await;
        read.result?;
        read.buffer.copy_to(dst);
        self.release_token(token).await;
        Ok(())
    }

    async fn release(&self, token: Self::Token) {
        self.release_token(token).await;
    }
}

impl<D> VirtioBlockSwapBackend<D>
where
    D: BlockDevice,
    D::Buffer: OwnedBlockBuffer,
{
    /// A zeroed buffer covering the whole extent of `token`.
    fn extent_buffer(
        &self,
        token: VirtioBlockSwapToken,
    ) -> Result<D::Buffer, VirtioBlockSwapError> {
        let block_size = self.device.block_size();
        if block_size == 0 {
            return Err(VirtioBlockSwapError::InvalidBlockSize);
        }
        D::Buffer::zeroed(token.block_count * block_size).ok_or(
            VirtioBlockSwapError::PayloadTooLarge {
                len: token.byte_len,
            },
        )
    }

    async fn write_extent(
        &self,
        token: VirtioBlockSwapToken,
        bytes: &[u8],
    ) -> Result<(), VirtioBlockSwapError> {
        let mut buffer = self.extent_buffer(token)?;
        buffer.copy_from(bytes);
        self.device.write(token.start_block, buffer).await.result?;
        Ok(())
    }
}

fn validate_request(
    current_rights: BlockDeviceRights,
    geometry: BlockGeometry,
    block_id: usize,
    len: usize,
    required_right: BlockDeviceRights,
) -> IoResult<()> {
    if !current_rights.contains(required_right) {
        return Err(IoError::PermissionDenied);
    }

    let block_bytes = geometry.logical_block_bytes;
    if len == 0 || !len.is_multiple_of(block_bytes) {
        return Err(IoError::InvalidBufferLength {
            required_multiple: block_bytes,
            actual: len,
        });
    }

    let requested_blocks = len / block_bytes;
    let end_block = block_id
        .checked_add(requested_blocks)
        .ok_or(IoError::OutOfBounds)?;
    if end_block > geometry.capacity_blocks {
        return Err(IoError::OutOfBounds);
    }

    Ok(())
}

fn map_block_status(status: u8) -> IoResult<()> {
    match status {
        0 => Ok(()),
        1 => Err(IoError::DeviceFault),
        2 => Err(IoError::Unsupported),
        _ => Err(IoError::DeviceFault),
    }
}

fn as_bytes<T>(value: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

fn as_bytes_slice<T>(values: &[T]) -> &[u8] {
    unsafe { core::slice::from_raw_parts(values.as_ptr().cast::<u8>(), size_of_val(values)) }
}

#[cfg(test)]
mod tests;
