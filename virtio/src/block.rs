//! virtio-blk: the block device every backend brings up.
//!
//! The driver implements the whole request set the kernel needs — read,
//! write, cache flush, device identification, discard and write-zeroes —
//! and negotiates the feature bits that describe the device's geometry so
//! callers address it in its own logical blocks rather than in 512-byte
//! sectors the device may not use natively.
//!
//! Concurrency contract: the device is programmed once during
//! single-processor bring-up. Afterwards every request goes through one
//! of the per-processor queues: the submitting task takes that queue's
//! async mutex only long enough to write its chain and kick the device,
//! then parks on the queue's completion table. Completions are routed by
//! descriptor identifier, so whichever task wins the queue lock after an
//! interrupt drains the ring on everyone's behalf. A request never
//! migrates between queues: the queue is chosen when the chain is
//! written and the completion is awaited on that same queue.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use arrayvec::ArrayVec;
use async_lock::Mutex;
use core::cmp;
use core::mem::{size_of, size_of_val};
use core::sync::atomic::{AtomicU64, Ordering};

use helios_hal::cpu::Cpu;
use helios_hal::fs::{
    BlockDevice, BlockDeviceCapabilities, BlockDeviceRights, BlockGeometry, BlockQueueTopology,
    BlockRange, BlockSerial,
};
use helios_hal::io::{IoError, IoResult};
use helios_hal::resource::KernelResource;
use helios_hal::vmm::SwapBackend;

use crate::features::{NegotiatedFeatures, RING_FEATURES, negotiate};
use crate::inflight::{InFlight, await_completion, submit_chain};
use crate::notify::Notify;
use crate::queue::{MAX_CHAIN_BUFFERS, VirtQueue};
use crate::transport::{DeviceStatus, DeviceType, VirtioTransport};

/// virtio-blk addresses every request in 512-byte sectors regardless of
/// the logical block size the device reports (virtio 1.2 §5.2.6).
pub const SECTOR_SIZE: usize = 512;

/// Longest request pipeline the driver keeps on one queue.
///
/// The depth is what makes the device useful under concurrent load: each
/// in-flight chain owns a descriptor identifier and a completion slot, and
/// a deeper queue only helps once the device can actually overlap that
/// many requests. 128 is the depth QEMU's virtio-blk exposes by default,
/// so the driver never asks for more ring than the device offers.
const BLOCK_MAX_QUEUE_SIZE: u16 = 128;
const BLOCK_QUEUE_SLOTS: usize = BLOCK_MAX_QUEUE_SIZE as usize;

/// Segments one discard or write-zeroes request carries.
///
/// Every segment is a 16-byte descriptor of a sector run, and all of them
/// travel in a single read-only buffer, so this bounds a stack array
/// rather than a descriptor chain.
const DEALLOCATE_SEGMENTS: usize = 8;

/// Bytes of the identifier a device reports for `VIRTIO_BLK_T_GET_ID`.
const BLOCK_ID_BYTES: usize = 20;

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
        usize::from(Cpu::current_processor(self).id())
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

/// One request's queue: the ring itself plus the completion slots for the
/// chains in flight on it.
struct BlockQueue<T: VirtioTransport> {
    queue: Mutex<VirtQueue<T>>,
    inflight: InFlight<BLOCK_QUEUE_SLOTS>,
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

impl RequestLimits {
    /// Most bytes one request may transfer, as a whole number of logical
    /// blocks.
    fn max_transfer_bytes(&self, block_bytes: usize) -> usize {
        let per_request = self.segment_bytes.saturating_mul(self.data_segments);
        let blocks = per_request / block_bytes;
        assert!(
            blocks != 0,
            "virtio-blk cannot carry a whole {block_bytes}-byte block in one request"
        );
        blocks * block_bytes
    }
}

pub struct VirtioBlockDevice<T: VirtioTransport, C: QueueAffinity> {
    transport: T,
    cpu: C,
    /// One queue per processor when the device offers enough of them.
    /// A request is bound to the queue it was written into, so a task
    /// that migrates still reaps its completion where it left it.
    queues: Box<[BlockQueue<T>]>,
    interrupts: Notify,
    features: NegotiatedFeatures,
    geometry: BlockGeometry,
    capabilities: BlockDeviceCapabilities,
    limits: RequestLimits,
    readonly: bool,
    counters: RequestCounters,
}

pub struct VirtioBlockResource<T: VirtioTransport, C: QueueAffinity> {
    resource: KernelResource<VirtioBlockDevice<T, C>, BlockDeviceRights>,
}

pub struct VirtioBlockSwapBackend<D: BlockDevice> {
    device: D,
    state: Mutex<VirtioBlockSwapState>,
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

impl<T: VirtioTransport, C: QueueAffinity> VirtioBlockDevice<T, C> {
    pub fn new(transport: T, cpu: C) -> IoResult<Self> {
        if transport.device_type() != DeviceType::Block {
            return Err(IoError::Unsupported);
        }

        let features = negotiate(&transport, RING_FEATURES | BLK_FEATURES)?;

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
        let capacity_blocks = usize::try_from(capacity_sectors)
            .map_err(|_| IoError::DeviceFault)?
            / sectors_per_block;

        let (physical_block_exp, min_io_blocks, opt_io_blocks) = if features.device(BLK_F_TOPOLOGY)
        {
            (
                transport.read_config_u8(CFG_PHYSICAL_BLOCK_EXP),
                u32::from(read_config_u16(&transport, CFG_MIN_IO_SIZE)),
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

        let mut capabilities = BlockDeviceCapabilities::empty();
        if features.device(BLK_F_FLUSH) {
            capabilities |= BlockDeviceCapabilities::FLUSH;
        }
        let (discard_sectors, discard_segments) = if features.device(BLK_F_DISCARD) {
            capabilities |= BlockDeviceCapabilities::DISCARD;
            deallocate_limits(
                &transport,
                CFG_MAX_DISCARD_SECTORS,
                CFG_MAX_DISCARD_SEG,
                "discard",
            )?
        } else {
            (0, 0)
        };
        let (write_zeroes_sectors, write_zeroes_segments) = if features.device(BLK_F_WRITE_ZEROES) {
            capabilities |= BlockDeviceCapabilities::WRITE_ZEROES;
            deallocate_limits(
                &transport,
                CFG_MAX_WRITE_ZEROES_SECTORS,
                CFG_MAX_WRITE_ZEROES_SEG,
                "write-zeroes",
            )?
        } else {
            (0, 0)
        };

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
            queues.push(BlockQueue {
                queue: Mutex::new(VirtQueue::new(
                    &transport,
                    index,
                    queue_size,
                    chain_limit,
                    features,
                )?),
                inflight: InFlight::new(),
            });
        }

        transport.set_status(
            DeviceStatus::ACKNOWLEDGE
                | DeviceStatus::DRIVER
                | DeviceStatus::FEATURES_OK
                | DeviceStatus::DRIVER_OK,
        );

        let geometry = BlockGeometry {
            logical_block_bytes,
            physical_block_exp,
            min_io_blocks,
            opt_io_blocks,
            max_segments: device_segments,
            max_segment_bytes: u32::try_from(segment_bytes).unwrap_or(u32::MAX),
            capacity_blocks,
        };
        let readonly = features.device(BLK_F_RO);
        let writeback =
            features.device(BLK_F_CONFIG_WCE) && transport.read_config_u8(CFG_WRITEBACK) != 0;
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
            interrupts: Notify::new(),
            features,
            geometry,
            capabilities,
            limits: RequestLimits {
                sectors_per_block,
                data_segments,
                segment_bytes,
                discard_sectors,
                discard_segments,
                write_zeroes_sectors,
                write_zeroes_segments,
            },
            readonly,
            counters: RequestCounters::default(),
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

    pub fn into_resource(self, rights: BlockDeviceRights) -> VirtioBlockResource<T, C> {
        VirtioBlockResource {
            resource: KernelResource::new(self, rights),
        }
    }

    pub fn new_resource(
        transport: T,
        cpu: C,
        rights: BlockDeviceRights,
    ) -> IoResult<VirtioBlockResource<T, C>> {
        Self::new(transport, cpu).map(|device| device.into_resource(rights))
    }

    /// Interrupt handlers should only call this method: acknowledge the
    /// device interrupt and wake the tasks parked on its queues.
    ///
    /// The transport acknowledges the whole interrupt-status word, so a
    /// configuration-change notification (virtio 1.2 §4.1.4.5, ISR bit
    /// 1) is cleared but not decoded: the capacity this driver reports
    /// is the one the device published when it was opened. Re-reading it
    /// needs the transport to report which bit fired, which is a change
    /// to the interrupt path rather than to this driver.
    pub fn handle_interrupt(&self) {
        self.transport.ack_interrupt();
        self.interrupts.notify_all();
    }

    /// The first sector of a logical block.
    fn sector_of(&self, block_id: usize) -> u64 {
        (block_id as u64) * self.limits.sectors_per_block as u64
    }

    async fn read_blocks(&self, block_id: usize, buf: &mut [u8]) -> IoResult<()> {
        let block_bytes = self.geometry.logical_block_bytes;
        let stride = self.limits.max_transfer_bytes(block_bytes);
        let mut offset = 0;
        while offset < buf.len() {
            let len = cmp::min(stride, buf.len() - offset);
            let sector = self.sector_of(block_id + offset / block_bytes);
            let request = BlkReq::new(ReqType::In, sector);
            let mut status = 0_u8;
            let mut segments: ArrayVec<&mut [u8], MAX_CHAIN_BUFFERS> = ArrayVec::new();
            for segment in buf[offset..offset + len].chunks_mut(self.limits.segment_bytes) {
                segments.push(segment);
            }
            let used = self
                .execute(ReqType::In, &request, &[], &mut segments, &mut status)
                .await?;
            map_block_status(status)?;
            // The used length counts everything the device wrote into the
            // chain, which is the payload plus the status byte. A short
            // read is a device fault rather than a partial success: the
            // caller asked for whole blocks and the tail would otherwise
            // silently keep whatever was in the buffer.
            let written = usize::try_from(used)
                .map_err(|_| IoError::DeviceFault)?
                .checked_sub(size_of::<u8>())
                .ok_or(IoError::DeviceFault)?;
            if written != len {
                return Err(IoError::DeviceFault);
            }
            offset += len;
        }
        Ok(())
    }

    async fn write_blocks(&self, block_id: usize, buf: &[u8]) -> IoResult<()> {
        let block_bytes = self.geometry.logical_block_bytes;
        let stride = self.limits.max_transfer_bytes(block_bytes);
        let mut offset = 0;
        while offset < buf.len() {
            let len = cmp::min(stride, buf.len() - offset);
            let sector = self.sector_of(block_id + offset / block_bytes);
            let request = BlkReq::new(ReqType::Out, sector);
            let mut status = 0_u8;
            let mut segments: ArrayVec<&[u8], MAX_CHAIN_BUFFERS> = ArrayVec::new();
            for segment in buf[offset..offset + len].chunks(self.limits.segment_bytes) {
                segments.push(segment);
            }
            self.execute(
                ReqType::Out,
                &request,
                &segments,
                &mut ArrayVec::new(),
                &mut status,
            )
            .await?;
            map_block_status(status)?;
            offset += len;
        }
        Ok(())
    }

    async fn flush_cache(&self) -> IoResult<()> {
        // virtio 1.2 §5.2.5: without VIRTIO_BLK_F_FLUSH the device has no
        // volatile write cache, so a completed write is already durable
        // and there is nothing to commit.
        if !self.capabilities.contains(BlockDeviceCapabilities::FLUSH) {
            return Ok(());
        }
        let request = BlkReq::new(ReqType::Flush, 0);
        let mut status = 0_u8;
        self.execute(
            ReqType::Flush,
            &request,
            &[],
            &mut ArrayVec::new(),
            &mut status,
        )
        .await?;
        map_block_status(status)
    }

    async fn read_serial(&self) -> IoResult<BlockSerial> {
        let request = BlkReq::new(ReqType::GetId, 0);
        let mut id = [0_u8; BLOCK_ID_BYTES];
        let mut status = 0_u8;
        let mut segments: ArrayVec<&mut [u8], MAX_CHAIN_BUFFERS> = ArrayVec::new();
        segments.push(&mut id);
        let used = self
            .execute(ReqType::GetId, &request, &[], &mut segments, &mut status)
            .await?;
        // The chain no longer names the buffer, so the identifier bytes
        // can be read back.
        drop(segments);
        map_block_status(status)?;
        let reported = usize::try_from(used)
            .map_err(|_| IoError::DeviceFault)?
            .saturating_sub(size_of::<u8>())
            .min(BLOCK_ID_BYTES);
        BlockSerial::new(&id[..reported]).ok_or(IoError::DeviceFault)
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
            let mut segments: ArrayVec<BlkDeallocate, DEALLOCATE_SEGMENTS> = ArrayVec::new();
            while remaining != 0 && segments.len() < max_segments {
                let run = cmp::min(remaining, u64::from(max_sectors));
                let run = u32::try_from(run).map_err(|_| IoError::DeviceFault)?;
                segments.push(BlkDeallocate::new(sector, run));
                sector += u64::from(run);
                remaining -= u64::from(run);
            }
            let header = BlkReq::new(request, 0);
            let mut status = 0_u8;
            self.execute(
                request,
                &header,
                &[as_bytes_slice(&segments)],
                &mut ArrayVec::new(),
                &mut status,
            )
            .await?;
            map_block_status(status)?;
        }
        Ok(())
    }

    /// Writes one request chain onto this processor's queue and waits for
    /// its completion, returning the bytes the device wrote.
    ///
    /// The completion is routed by descriptor identifier: whichever task
    /// wins the queue lock after an interrupt drains everything the
    /// device published, so a device that finishes requests out of order
    /// — which EVENT_IDX and IN_ORDER both permit — is handled without
    /// any task assuming the completion it sees is its own.
    async fn execute(
        &self,
        kind: ReqType,
        header: &BlkReq,
        data_in: &[&[u8]],
        data_out: &mut ArrayVec<&mut [u8], MAX_CHAIN_BUFFERS>,
        status: &mut u8,
    ) -> IoResult<u32> {
        let mut inputs: ArrayVec<&[u8], MAX_CHAIN_BUFFERS> = ArrayVec::new();
        inputs.push(as_bytes(header));
        for input in data_in {
            inputs.push(input);
        }
        let mut outputs: ArrayVec<&mut [u8], MAX_CHAIN_BUFFERS> = ArrayVec::new();
        for output in data_out.iter_mut() {
            outputs.push(&mut **output);
        }
        outputs.push(core::slice::from_mut(status));

        let queue = self.queue_for_current_processor();
        let token = submit_chain(
            &queue.inflight,
            &queue.queue,
            &self.transport,
            &inputs,
            &mut outputs,
        )
        .await?;
        let used = await_completion(&queue.inflight, &queue.queue, token, || {
            self.interrupts.notified()
        })
        .await;
        self.counters.count(kind);
        Ok(used)
    }

    fn queue_for_current_processor(&self) -> &BlockQueue<T> {
        let processor = self.cpu.current_processor();
        &self.queues[processor % self.queues.len()]
    }
}

impl<T: VirtioTransport, C: QueueAffinity> Drop for VirtioBlockDevice<T, C> {
    fn drop(&mut self) {
        for queue in &mut self.queues {
            queue.queue.get_mut().shutdown(&self.transport);
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
    let device_queues = usize::from(read_config_u16(transport, CFG_NUM_QUEUES));
    device_queues.clamp(1, cpu.processor_count().max(1))
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

fn read_config_u16<T: VirtioTransport>(transport: &T, offset: usize) -> u16 {
    u16::from_le_bytes([
        transport.read_config_u8(offset),
        transport.read_config_u8(offset + 1),
    ])
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
            state: Mutex::new(VirtioBlockSwapState {
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

impl<T: VirtioTransport, C: QueueAffinity> VirtioBlockResource<T, C> {
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

    fn object(&self) -> &VirtioBlockDevice<T, C> {
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

impl<T: VirtioTransport, C: QueueAffinity> Clone for VirtioBlockResource<T, C> {
    fn clone(&self) -> Self {
        Self {
            resource: self.resource.clone(),
        }
    }
}

impl<T: VirtioTransport, C: QueueAffinity> BlockDevice for VirtioBlockResource<T, C> {
    async fn read_block(&self, block_id: usize, buf: &mut [u8]) -> IoResult<()> {
        validate_request(
            self.rights(),
            self.object().geometry,
            block_id,
            buf.len(),
            BlockDeviceRights::READ,
        )?;

        self.object().read_blocks(block_id, buf).await
    }

    async fn write_block(&self, block_id: usize, buf: &[u8]) -> IoResult<()> {
        validate_request(
            self.rights(),
            self.object().geometry,
            block_id,
            buf.len(),
            BlockDeviceRights::WRITE,
        )?;

        if self.object().readonly {
            return Err(IoError::ReadOnly);
        }

        self.object().write_blocks(block_id, buf).await
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
}

impl<D: BlockDevice + 'static> SwapBackend for VirtioBlockSwapBackend<D> {
    type Token = VirtioBlockSwapToken;
    type Error = VirtioBlockSwapError;

    /// Writes `bytes` into a freshly allocated extent.
    ///
    /// The caller's slice goes to the device as it is: only the tail that
    /// does not fill a whole block is copied, into a single zero-padded
    /// block. The extent is committed with a cache flush before the token
    /// is handed back, because a swap-out the device has acknowledged but
    /// not persisted is a page the kernel can no longer reconstruct.
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
    /// The whole blocks land in the caller's buffer directly; only a
    /// partial tail block goes through one block of scratch. The extent
    /// is released only once the page is back: a failed read leaves the
    /// token valid so the caller can try again rather than losing the
    /// only copy of the page.
    async fn swap_in(&self, token: Self::Token, dst: &mut [u8]) -> Result<(), Self::Error> {
        if dst.len() != token.byte_len {
            return Err(VirtioBlockSwapError::InvalidDestination {
                expected: token.byte_len,
                actual: dst.len(),
            });
        }
        let block_size = self.device.block_size();
        if block_size == 0 {
            return Err(VirtioBlockSwapError::InvalidBlockSize);
        }

        let head_bytes = dst.len() - dst.len() % block_size;
        if head_bytes != 0 {
            self.device
                .read_block(token.start_block, &mut dst[..head_bytes])
                .await?;
        }
        if head_bytes != dst.len() {
            let mut tail = vec![0_u8; block_size];
            self.device
                .read_block(token.start_block + head_bytes / block_size, &mut tail)
                .await?;
            let remainder = dst.len() - head_bytes;
            dst[head_bytes..].copy_from_slice(&tail[..remainder]);
        }

        self.release_token(token).await;
        Ok(())
    }
}

impl<D: BlockDevice> VirtioBlockSwapBackend<D> {
    async fn write_extent(
        &self,
        token: VirtioBlockSwapToken,
        bytes: &[u8],
    ) -> Result<(), VirtioBlockSwapError> {
        let block_size = self.device.block_size();
        if block_size == 0 {
            return Err(VirtioBlockSwapError::InvalidBlockSize);
        }

        let head_bytes = bytes.len() - bytes.len() % block_size;
        if head_bytes != 0 {
            self.device
                .write_block(token.start_block, &bytes[..head_bytes])
                .await?;
        }
        if head_bytes != bytes.len() {
            let mut tail = vec![0_u8; block_size];
            tail[..bytes.len() - head_bytes].copy_from_slice(&bytes[head_bytes..]);
            self.device
                .write_block(token.start_block + head_bytes / block_size, &tail)
                .await?;
        }
        self.device.flush().await?;
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
mod tests {
    use super::*;

    use crate::testing::{FakeAffinity, FakeTransport, FakeTransportConfig};
    use crate::transport::VirtioFeatures;
    use core::pin::pin;
    use futures_lite::future::{block_on, poll_once};
    use spin::Mutex as SpinMutex;

    /// A device with 2 MiB of capacity in 512-byte sectors.
    const TEST_CAPACITY_SECTORS: u32 = 4096;

    type TestDevice = VirtioBlockResource<FakeTransport, FakeAffinity<1>>;

    fn device_with(features: u64, setup: impl FnOnce(&FakeTransport)) -> TestDevice {
        let transport = FakeTransport::new(FakeTransportConfig {
            device_type: DeviceType::Block,
            offered_features: VirtioFeatures::VERSION_1.bits() | features,
            queue_size: 8,
            supports_queue_reset: false,
        });
        transport.set_config_u32(CFG_CAPACITY_LOW, TEST_CAPACITY_SECTORS);
        setup(&transport);
        VirtioBlockDevice::new(transport, FakeAffinity::<1>)
            .expect("block device should initialize")
            .into_resource(BlockDeviceRights::READ | BlockDeviceRights::WRITE)
    }

    fn device() -> TestDevice {
        device_with(0, |_| {})
    }

    /// The identifier the next submission on the first queue will take.
    ///
    /// A chain spends one descriptor per buffer, so the identifiers a
    /// driver's requests get depend on the shape of the chains before
    /// them; tests ask the ring rather than assuming a numbering.
    fn next_token(device: &TestDevice) -> u16 {
        device.object().queues[0]
            .queue
            .try_lock()
            .expect("a parked driver does not hold the queue lock")
            .next_free_descriptor()
    }

    /// The bytes the driver made readable in chain `token`.
    fn request_of(device: &TestDevice, token: u16) -> Vec<u8> {
        device.object().queues[0]
            .queue
            .try_lock()
            .expect("a parked driver does not hold the queue lock")
            .device_request(token)
    }

    /// Plays the device: writes `response` into the chain's writable
    /// buffers, publishes the used entry and raises the interrupt.
    fn complete(device: &TestDevice, token: u16, response: &[u8]) {
        {
            let queue = device.object().queues[0]
                .queue
                .try_lock()
                .expect("a parked driver does not hold the queue lock");
            let len = queue.device_respond(token, response);
            queue.device_complete(token, len);
        }
        device.handle_interrupt();
    }

    fn header(request: ReqType, sector: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(request as u32).to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&sector.to_le_bytes());
        bytes
    }

    #[test]
    fn a_wrong_device_type_is_rejected() {
        let rejected = VirtioBlockDevice::new(
            FakeTransport::new(FakeTransportConfig {
                device_type: DeviceType::Entropy,
                ..FakeTransportConfig::default()
            }),
            FakeAffinity::<1>,
        )
        .err();
        assert_eq!(rejected, Some(IoError::Unsupported));
    }

    #[test]
    fn a_plain_device_reports_sector_geometry_and_one_queue() {
        let device = device();

        assert_eq!(
            device.geometry(),
            BlockGeometry {
                logical_block_bytes: SECTOR_SIZE,
                physical_block_exp: 0,
                min_io_blocks: 0,
                opt_io_blocks: 0,
                max_segments: 1,
                max_segment_bytes: u32::MAX,
                capacity_blocks: TEST_CAPACITY_SECTORS as usize,
            }
        );
        assert_eq!(device.capabilities(), BlockDeviceCapabilities::empty());
        assert_eq!(device.queue_count(), 1);
    }

    #[test]
    fn topology_and_block_size_are_read_into_the_geometry() {
        let device = device_with(
            BLK_F_BLK_SIZE | BLK_F_TOPOLOGY | BLK_F_SEG_MAX | BLK_F_SIZE_MAX,
            |transport| {
                transport.set_config_u32(CFG_BLK_SIZE, 4096);
                transport.set_config_u8(CFG_PHYSICAL_BLOCK_EXP, 1);
                transport.set_config_u16(CFG_MIN_IO_SIZE, 2);
                transport.set_config_u32(CFG_OPT_IO_SIZE, 8);
                transport.set_config_u32(CFG_SEG_MAX, 4);
                transport.set_config_u32(CFG_SIZE_MAX, 16384);
            },
        );

        assert_eq!(
            device.geometry(),
            BlockGeometry {
                logical_block_bytes: 4096,
                physical_block_exp: 1,
                min_io_blocks: 2,
                opt_io_blocks: 8,
                max_segments: 4,
                max_segment_bytes: 16384,
                // 4096 512-byte sectors are 512 blocks of 4 KiB.
                capacity_blocks: 512,
            }
        );
        assert_eq!(device.geometry().physical_block_bytes(), 8192);
        assert_eq!(device.geometry().capacity_bytes(), 2 * 1024 * 1024);
    }

    #[test]
    fn a_logical_block_larger_than_a_sector_addresses_in_sectors_on_the_wire() {
        let device = device_with(BLK_F_BLK_SIZE, |transport| {
            transport.set_config_u32(CFG_BLK_SIZE, 4096);
        });

        let mut buffer = [0_u8; 4096];
        let mut read = pin!(device.read_block(3, &mut buffer));
        assert!(block_on(poll_once(read.as_mut())).is_none());

        // Block 3 of a 4 KiB device is sector 24: virtio-blk addresses
        // every request in 512-byte sectors whatever the block size is.
        assert_eq!(request_of(&device, 0), header(ReqType::In, 24));
    }

    #[test]
    fn a_read_carries_the_payload_and_checks_the_used_length() {
        let device = device();
        let mut buffer = [0_u8; 512];
        {
            let mut read = pin!(device.read_block(2, &mut buffer));

            assert!(block_on(poll_once(read.as_mut())).is_none());
            assert_eq!(request_of(&device, 0), header(ReqType::In, 2));

            let mut response = vec![0xa5_u8; 512];
            response.push(0);
            complete(&device, 0, &response);
            assert_eq!(block_on(poll_once(read.as_mut())), Some(Ok(())));
        }
        assert_eq!(buffer, [0xa5_u8; 512]);
        assert_eq!(device.request_counts().reads, 1);
    }

    #[test]
    fn a_short_read_is_a_device_fault() {
        let device = device();
        let mut buffer = [0_u8; 1024];
        let mut read = pin!(device.read_block(0, &mut buffer));

        assert!(block_on(poll_once(read.as_mut())).is_none());
        // The device claims success but only filled half the chain.
        {
            let queue = device.object().queues[0]
                .queue
                .try_lock()
                .expect("a parked driver does not hold the queue lock");
            queue.device_respond(0, &vec![0_u8; 1025]);
            queue.device_complete(0, 513);
        }
        device.handle_interrupt();

        assert_eq!(
            block_on(poll_once(read.as_mut())),
            Some(Err(IoError::DeviceFault))
        );
    }

    #[test]
    fn a_write_puts_the_payload_after_the_header() {
        let device = device();
        let payload = [0x5a_u8; 512];
        let mut write = pin!(device.write_block(7, &payload));

        assert!(block_on(poll_once(write.as_mut())).is_none());
        let mut expected = header(ReqType::Out, 7);
        expected.extend_from_slice(&payload);
        assert_eq!(request_of(&device, 0), expected);

        complete(&device, 0, &[0]);
        assert_eq!(block_on(poll_once(write.as_mut())), Some(Ok(())));
        assert_eq!(device.request_counts().writes, 1);
    }

    #[test]
    fn a_device_error_status_becomes_a_device_fault() {
        let device = device();
        let payload = [0_u8; 512];
        let mut write = pin!(device.write_block(0, &payload));

        assert!(block_on(poll_once(write.as_mut())).is_none());
        complete(&device, 0, &[1]);
        assert_eq!(
            block_on(poll_once(write.as_mut())),
            Some(Err(IoError::DeviceFault))
        );
    }

    #[test]
    fn a_flush_is_a_bare_header_when_the_device_has_a_write_cache() {
        let device = device_with(BLK_F_FLUSH, |_| {});
        assert!(
            device
                .capabilities()
                .contains(BlockDeviceCapabilities::FLUSH)
        );

        let mut flush = pin!(device.flush());
        assert!(block_on(poll_once(flush.as_mut())).is_none());
        assert_eq!(request_of(&device, 0), header(ReqType::Flush, 0));

        complete(&device, 0, &[0]);
        assert_eq!(block_on(poll_once(flush.as_mut())), Some(Ok(())));
        assert_eq!(device.request_counts().flushes, 1);
    }

    #[test]
    fn a_device_without_a_write_cache_has_nothing_to_flush() {
        let device = device();

        assert_eq!(block_on(device.flush()), Ok(()));
        assert_eq!(
            device.object().transport.kick_count(),
            0,
            "a flush with no volatile cache must not reach the device"
        );
    }

    #[test]
    fn the_serial_comes_back_from_a_get_id_request() {
        let device = device();
        let mut serial = pin!(device.serial());

        assert!(block_on(poll_once(serial.as_mut())).is_none());
        assert_eq!(request_of(&device, 0), header(ReqType::GetId, 0));

        let mut response = [0_u8; BLOCK_ID_BYTES + 1];
        response[..11].copy_from_slice(b"helios-data");
        complete(&device, 0, &response);

        let serial = block_on(poll_once(serial.as_mut()))
            .expect("the identifier request has completed")
            .expect("the device answered the identifier request");
        assert_eq!(serial.as_str(), Some("helios-data"));
    }

    #[test]
    fn a_device_that_rejects_get_id_reports_it_as_unsupported() {
        let device = device();
        let token = next_token(&device);
        let mut serial = pin!(device.serial());

        assert!(block_on(poll_once(serial.as_mut())).is_none());
        // A device without identification support writes nothing but the
        // status byte, VIRTIO_BLK_S_UNSUPP, and reports that one byte as
        // the used length.
        {
            let queue = device.object().queues[0]
                .queue
                .try_lock()
                .expect("a parked driver does not hold the queue lock");
            let mut response = [0_u8; BLOCK_ID_BYTES + 1];
            response[BLOCK_ID_BYTES] = 2;
            queue.device_respond(token, &response);
            queue.device_complete(token, 1);
        }
        device.handle_interrupt();

        assert_eq!(
            block_on(poll_once(serial.as_mut())),
            Some(Err(IoError::Unsupported))
        );
    }

    #[test]
    fn discard_carries_one_sector_run_per_segment() {
        let device = device_with(BLK_F_DISCARD, |transport| {
            transport.set_config_u32(CFG_MAX_DISCARD_SECTORS, 8);
            transport.set_config_u32(CFG_MAX_DISCARD_SEG, 4);
        });
        assert!(
            device
                .capabilities()
                .contains(BlockDeviceCapabilities::DISCARD)
        );

        let mut discard = pin!(device.discard(BlockRange::new(16, 12)));
        assert!(block_on(poll_once(discard.as_mut())).is_none());

        let mut expected = header(ReqType::Discard, 0);
        // 12 sectors split into runs of at most 8.
        expected.extend_from_slice(&16_u64.to_le_bytes());
        expected.extend_from_slice(&8_u32.to_le_bytes());
        expected.extend_from_slice(&0_u32.to_le_bytes());
        expected.extend_from_slice(&24_u64.to_le_bytes());
        expected.extend_from_slice(&4_u32.to_le_bytes());
        expected.extend_from_slice(&0_u32.to_le_bytes());
        assert_eq!(request_of(&device, 0), expected);

        complete(&device, 0, &[0]);
        assert_eq!(block_on(poll_once(discard.as_mut())), Some(Ok(())));
        assert_eq!(device.request_counts().discards, 1);
    }

    #[test]
    fn write_zeroes_uses_its_own_limits() {
        let device = device_with(BLK_F_WRITE_ZEROES, |transport| {
            transport.set_config_u32(CFG_MAX_WRITE_ZEROES_SECTORS, 64);
            transport.set_config_u32(CFG_MAX_WRITE_ZEROES_SEG, 1);
        });

        let mut zeroes = pin!(device.write_zeroes(BlockRange::new(0, 8)));
        assert!(block_on(poll_once(zeroes.as_mut())).is_none());

        let mut expected = header(ReqType::WriteZeroes, 0);
        expected.extend_from_slice(&0_u64.to_le_bytes());
        expected.extend_from_slice(&8_u32.to_le_bytes());
        expected.extend_from_slice(&0_u32.to_le_bytes());
        assert_eq!(request_of(&device, 0), expected);

        complete(&device, 0, &[0]);
        assert_eq!(block_on(poll_once(zeroes.as_mut())), Some(Ok(())));
        assert_eq!(device.request_counts().write_zeroes, 1);
    }

    #[test]
    fn deallocation_is_rejected_on_a_device_that_does_not_offer_it() {
        let device = device();

        assert_eq!(
            block_on(device.discard(BlockRange::new(0, 1))),
            Err(IoError::Unsupported)
        );
        assert_eq!(
            block_on(device.write_zeroes(BlockRange::new(0, 1))),
            Err(IoError::Unsupported)
        );
    }

    #[test]
    fn deallocation_beyond_the_capacity_is_rejected() {
        let device = device_with(BLK_F_DISCARD, |transport| {
            transport.set_config_u32(CFG_MAX_DISCARD_SECTORS, 8);
            transport.set_config_u32(CFG_MAX_DISCARD_SEG, 1);
        });

        assert_eq!(
            block_on(device.discard(BlockRange::new(TEST_CAPACITY_SECTORS as usize - 1, 2))),
            Err(IoError::OutOfBounds)
        );
    }

    #[test]
    fn a_transfer_is_split_at_the_segment_and_request_limits() {
        let device = device_with(BLK_F_SEG_MAX | BLK_F_SIZE_MAX, |transport| {
            transport.set_config_u32(CFG_SEG_MAX, 2);
            transport.set_config_u32(CFG_SIZE_MAX, 1024);
        });
        let payload = [0x11_u8; 4096];
        let mut write = pin!(device.write_block(0, &payload));

        assert!(block_on(poll_once(write.as_mut())).is_none());
        // Two segments of 1024 bytes each is 2048 bytes per request, so
        // the first request covers sectors 0..4 and the second 4..8.
        let mut expected = header(ReqType::Out, 0);
        expected.extend_from_slice(&payload[..2048]);
        assert_eq!(request_of(&device, 0), expected);

        complete(&device, 0, &[0]);
        let second = next_token(&device);
        assert!(block_on(poll_once(write.as_mut())).is_none());
        let mut expected = header(ReqType::Out, 4);
        expected.extend_from_slice(&payload[2048..]);
        assert_eq!(request_of(&device, second), expected);

        complete(&device, second, &[0]);
        assert_eq!(block_on(poll_once(write.as_mut())), Some(Ok(())));
        assert_eq!(device.request_counts().writes, 2);
    }

    #[test]
    fn requests_are_pipelined_and_completed_out_of_order() {
        let device = device();
        let mut first = [0_u8; 512];
        let mut second = [0_u8; 512];
        {
            let first_token = next_token(&device);
            let mut read_first = pin!(device.read_block(0, &mut first));
            assert!(block_on(poll_once(read_first.as_mut())).is_none());
            let second_token = next_token(&device);
            let mut read_second = pin!(device.read_block(1, &mut second));
            assert!(block_on(poll_once(read_second.as_mut())).is_none());
            assert_eq!(request_of(&device, first_token), header(ReqType::In, 0));
            assert_eq!(request_of(&device, second_token), header(ReqType::In, 1));

            // The device answers the second request first. The waiter that
            // wakes drains the ring for everyone, so the first request must
            // not take a completion that is not addressed to it.
            let mut response = vec![0x22_u8; 512];
            response.push(0);
            complete(&device, second_token, &response);
            assert!(
                block_on(poll_once(read_first.as_mut())).is_none(),
                "the first request is still outstanding"
            );
            assert_eq!(block_on(poll_once(read_second.as_mut())), Some(Ok(())));

            let mut response = vec![0x33_u8; 512];
            response.push(0);
            complete(&device, first_token, &response);
            assert_eq!(block_on(poll_once(read_first.as_mut())), Some(Ok(())));
        }
        assert_eq!(first, [0x33_u8; 512]);
        assert_eq!(second, [0x22_u8; 512]);
    }

    #[test]
    fn a_readonly_device_refuses_writes() {
        let device = device_with(BLK_F_RO, |_| {});
        let payload = [0_u8; 512];

        assert_eq!(
            block_on(device.write_block(0, &payload)),
            Err(IoError::ReadOnly)
        );
    }

    #[test]
    fn multiqueue_programs_one_queue_per_processor() {
        let transport = FakeTransport::new(FakeTransportConfig {
            device_type: DeviceType::Block,
            offered_features: VirtioFeatures::VERSION_1.bits() | BLK_F_MQ,
            queue_size: 8,
            supports_queue_reset: false,
        });
        transport.set_config_u32(CFG_CAPACITY_LOW, TEST_CAPACITY_SECTORS);
        transport.set_config_u16(CFG_NUM_QUEUES, 8);

        let device = VirtioBlockDevice::new(transport, FakeAffinity::<4>)
            .expect("block device should initialize");

        assert_eq!(
            device.queue_count(),
            4,
            "a device with more queues than processors is programmed per processor"
        );
        let programmed = device.transport.programmed_queues();
        assert_eq!(
            programmed
                .iter()
                .map(|queue| queue.index)
                .collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
    }

    #[test]
    fn a_device_that_offers_a_deallocation_feature_without_limits_is_rejected() {
        let transport = FakeTransport::new(FakeTransportConfig {
            device_type: DeviceType::Block,
            offered_features: VirtioFeatures::VERSION_1.bits() | BLK_F_DISCARD,
            queue_size: 8,
            supports_queue_reset: false,
        });
        transport.set_config_u32(CFG_CAPACITY_LOW, TEST_CAPACITY_SECTORS);

        let rejected = VirtioBlockDevice::new(transport, FakeAffinity::<1>).err();
        assert!(matches!(rejected, Some(IoError::InvalidDeviceConfig(_))));
    }

    #[test]
    fn rejects_missing_rights() {
        let error = validate_request(
            BlockDeviceRights::READ,
            test_geometry(),
            0,
            512,
            BlockDeviceRights::WRITE,
        )
        .expect_err("write access without WRITE right must fail");

        assert_eq!(error, IoError::PermissionDenied);
    }

    #[test]
    fn rejects_invalid_buffer_length() {
        let error = validate_request(
            BlockDeviceRights::READ | BlockDeviceRights::WRITE,
            test_geometry(),
            0,
            511,
            BlockDeviceRights::READ,
        )
        .expect_err("request must reject invalid buffer length");

        assert_eq!(
            error,
            IoError::InvalidBufferLength {
                required_multiple: 512,
                actual: 511,
            }
        );
    }

    #[test]
    fn rejects_out_of_bounds_requests() {
        let mut geometry = test_geometry();
        geometry.capacity_blocks = 1;
        let error = validate_request(
            BlockDeviceRights::READ,
            geometry,
            1,
            512,
            BlockDeviceRights::READ,
        )
        .expect_err("out-of-bounds request must fail");

        assert_eq!(error, IoError::OutOfBounds);
    }

    #[test]
    fn accepts_well_formed_request() {
        validate_request(
            BlockDeviceRights::READ | BlockDeviceRights::WRITE,
            test_geometry(),
            2,
            1024,
            BlockDeviceRights::READ,
        )
        .expect("request should be accepted");
    }

    fn test_geometry() -> BlockGeometry {
        BlockGeometry {
            logical_block_bytes: SECTOR_SIZE,
            physical_block_exp: 0,
            min_io_blocks: 0,
            opt_io_blocks: 0,
            max_segments: 1,
            max_segment_bytes: u32::MAX,
            capacity_blocks: 8,
        }
    }

    /// A block device backed by host memory, for the swap backend tests.
    struct MemoryBlockDevice {
        blocks: SpinMutex<Vec<u8>>,
        block_size: usize,
        flushes: AtomicU64,
        fail_reads: bool,
    }

    impl MemoryBlockDevice {
        fn new(block_size: usize, block_count: usize) -> Self {
            Self {
                blocks: SpinMutex::new(vec![0_u8; block_size * block_count]),
                block_size,
                flushes: AtomicU64::new(0),
                fail_reads: false,
            }
        }

        fn failing_reads(block_size: usize, block_count: usize) -> Self {
            Self {
                fail_reads: true,
                ..Self::new(block_size, block_count)
            }
        }

        fn flushes(&self) -> u64 {
            self.flushes.load(Ordering::Relaxed)
        }
    }

    impl BlockDevice for MemoryBlockDevice {
        async fn read_block(&self, block_id: usize, buf: &mut [u8]) -> IoResult<()> {
            if self.fail_reads {
                return Err(IoError::DeviceFault);
            }
            let offset = block_id * self.block_size;
            buf.copy_from_slice(&self.blocks.lock()[offset..offset + buf.len()]);
            Ok(())
        }

        async fn write_block(&self, block_id: usize, buf: &[u8]) -> IoResult<()> {
            let offset = block_id * self.block_size;
            self.blocks.lock()[offset..offset + buf.len()].copy_from_slice(buf);
            Ok(())
        }

        async fn flush(&self) -> IoResult<()> {
            self.flushes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn discard(&self, _range: BlockRange) -> IoResult<()> {
            Err(IoError::Unsupported)
        }

        async fn write_zeroes(&self, _range: BlockRange) -> IoResult<()> {
            Err(IoError::Unsupported)
        }

        async fn serial(&self) -> IoResult<BlockSerial> {
            Err(IoError::Unsupported)
        }

        fn geometry(&self) -> BlockGeometry {
            BlockGeometry {
                logical_block_bytes: self.block_size,
                physical_block_exp: 0,
                min_io_blocks: 0,
                opt_io_blocks: 0,
                max_segments: 1,
                max_segment_bytes: u32::MAX,
                capacity_blocks: self.blocks.lock().len() / self.block_size,
            }
        }

        fn capabilities(&self) -> BlockDeviceCapabilities {
            BlockDeviceCapabilities::FLUSH
        }

        fn queue_topology(&self) -> BlockQueueTopology {
            BlockQueueTopology {
                queues: 1,
                depth: 1,
            }
        }
    }

    #[test]
    fn a_swapped_out_page_comes_back_unchanged_and_is_committed() {
        let backend = VirtioBlockSwapBackend::from_entire_device(MemoryBlockDevice::new(512, 16))
            .expect("swap backend should initialize");
        // Deliberately not a whole number of blocks: the tail block is
        // the only part that goes through a copy.
        let page: Vec<u8> = (0..1300_u32).map(|byte| byte as u8).collect();

        let token = block_on(backend.swap_out(&page)).expect("swap-out should succeed");
        assert_eq!(
            backend.device.flushes(),
            1,
            "a swap-out the device has not persisted is a page the kernel cannot rebuild"
        );

        let mut restored = vec![0_u8; page.len()];
        block_on(backend.swap_in(token, &mut restored)).expect("swap-in should succeed");
        assert_eq!(restored, page);
    }

    #[test]
    fn a_failed_swap_in_keeps_the_token_valid() {
        let backend =
            VirtioBlockSwapBackend::from_entire_device(MemoryBlockDevice::failing_reads(512, 4))
                .expect("swap backend should initialize");
        let page = vec![7_u8; 1024];
        let token = block_on(backend.swap_out(&page)).expect("swap-out should succeed");

        let mut restored = vec![0_u8; page.len()];
        assert_eq!(
            block_on(backend.swap_in(token, &mut restored)),
            Err(VirtioBlockSwapError::Io(IoError::DeviceFault))
        );
        assert_eq!(
            block_on(backend.state.lock()).available_blocks(),
            2,
            "the extent of a page that is still on the device must stay allocated"
        );
    }

    #[test]
    fn swap_extents_are_reused_and_coalesced() {
        let backend = VirtioBlockSwapBackend::from_entire_device(MemoryBlockDevice::new(512, 8))
            .expect("swap backend should initialize");

        let first = block_on(backend.swap_out(&[1_u8; 1024])).expect("first swap-out");
        let second = block_on(backend.swap_out(&[2_u8; 1024])).expect("second swap-out");
        let third = block_on(backend.swap_out(&[3_u8; 2048])).expect("third swap-out");
        assert_eq!(block_on(backend.state.lock()).available_blocks(), 0);
        assert_eq!(
            block_on(backend.swap_out(&[4_u8; 512])),
            Err(VirtioBlockSwapError::OutOfSwap {
                requested_blocks: 1,
                available_blocks: 0,
            })
        );

        let mut restored = vec![0_u8; 1024];
        block_on(backend.swap_in(second, &mut restored)).expect("swap-in of the middle extent");
        block_on(backend.swap_in(first, &mut restored)).expect("swap-in of the first extent");
        {
            let state = block_on(backend.state.lock());
            assert_eq!(state.available_blocks(), 4);
            assert_eq!(
                state.free.len(),
                1,
                "two adjacent freed extents are one free extent"
            );
        }

        let mut restored = vec![0_u8; 2048];
        block_on(backend.swap_in(third, &mut restored)).expect("swap-in of the last extent");
        assert_eq!(restored, vec![3_u8; 2048]);
        let state = block_on(backend.state.lock());
        assert_eq!(state.free.len(), 1);
        assert_eq!(state.available_blocks(), 8);
    }
}
