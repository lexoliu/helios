//! The scratch disk: the one disk the kernel writes for itself.
//!
//! It is proved end to end before it is published: a random pattern
//! goes to the last blocks of the device, is committed, read back and
//! compared, and the blocks are released again. A boot that reaches
//! components with a scratch disk that does not round-trip is worse than
//! a boot that stops here, so a mismatch is fatal.
//!
//! Concurrency contract: the self-check runs as one task before the disk
//! is handed out. Afterwards [`ScratchDisk`] is a cloneable handle whose
//! operations are the driver's own async requests; it holds no lock and
//! counts what it forwarded in relaxed atomics another processor may
//! read through [`ScratchDiskStats`].

extern crate alloc;

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use helios_hal::cpu::Cpu;
use helios_hal::fs::{
    BlockDevice, BlockDeviceCapabilities, BlockGeometry, BlockQueueTopology, BlockRange,
    BlockSerial, IoError, IoResult, OwnedBlockBuffer,
};
use helios_hal::vmm::SwapBackend;

use crate::Timer;
use crate::exec::Spawner;
use crate::instance::InstanceRegistry;
use crate::memory::{RootEntropyHandle, SwapDisabled, SwapHandle, disable_swap, install_swap};

/// Bytes the boot self-check round-trips through the device.
const SELF_CHECK_BYTES: usize = 4096;

/// What the boot self-check found wrong with the scratch disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BlockSelfCheckError {
    #[error(
        "scratch disk holds {capacity_blocks} blocks of {block_bytes} bytes, too small for a {SELF_CHECK_BYTES}-byte self check"
    )]
    TooSmall {
        capacity_blocks: usize,
        block_bytes: usize,
    },
    #[error("scratch disk write failed: {0}")]
    Write(IoError),
    #[error("scratch disk cache flush failed: {0}")]
    Flush(IoError),
    #[error("scratch disk read failed: {0}")]
    Read(IoError),
    #[error("scratch disk release failed: {0}")]
    Release(IoError),
    #[error(
        "scratch disk returned different bytes than were written: block {block} of the pattern differs"
    )]
    Mismatch { block: usize },
}

/// Counters and fixed properties the kernel reports for its scratch disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockStats {
    pub capacity_bytes: u64,
    pub block_bytes: u32,
    pub physical_block_bytes: u32,
    /// [`BlockDeviceCapabilities`] bits.
    pub capabilities: u8,
    pub queues: u32,
    pub queue_depth: u32,
    pub reads: u64,
    pub writes: u64,
    pub flushes: u64,
    pub discards: u64,
    pub write_zeroes: u64,
}

#[derive(Debug, Default)]
struct RequestCounters {
    reads: AtomicU64,
    writes: AtomicU64,
    flushes: AtomicU64,
    discards: AtomicU64,
    write_zeroes: AtomicU64,
}

/// What the scratch disk is and what the kernel has asked of it, readable
/// without naming the device's type.
#[derive(Clone)]
pub struct ScratchDiskStats {
    geometry: BlockGeometry,
    capabilities: BlockDeviceCapabilities,
    topology: BlockQueueTopology,
    counters: Arc<RequestCounters>,
}

impl ScratchDiskStats {
    /// A snapshot for `helios:system/stats`.
    pub fn snapshot(&self) -> BlockStats {
        BlockStats {
            capacity_bytes: self.geometry.capacity_bytes(),
            block_bytes: u32::try_from(self.geometry.logical_block_bytes).unwrap_or(u32::MAX),
            physical_block_bytes: u32::try_from(self.geometry.physical_block_bytes())
                .unwrap_or(u32::MAX),
            capabilities: self.capabilities.bits(),
            queues: u32::try_from(self.topology.queues).unwrap_or(u32::MAX),
            queue_depth: u32::try_from(self.topology.depth).unwrap_or(u32::MAX),
            reads: self.counters.reads.load(Ordering::Relaxed),
            writes: self.counters.writes.load(Ordering::Relaxed),
            flushes: self.counters.flushes.load(Ordering::Relaxed),
            discards: self.counters.discards.load(Ordering::Relaxed),
            write_zeroes: self.counters.write_zeroes.load(Ordering::Relaxed),
        }
    }
}

/// Puts swap on the scratch disk, behind the self-check's blocks, and
/// returns its handle for the backend to publish in its runtime state.
///
/// The self-check owns the last blocks of the device and has already run
/// by the time this is called; everything before them is swap's. Swap is
/// not persistence, a token is meaningless across a boot, so the whole
/// extent is handed out fresh every time.
///
/// `make` builds the [`SwapBackend`] over the disk from its first block
/// and block count, which keeps this crate from depending on the driver
/// crate that supplies the backend. A disk with no room for swap, or one
/// the backend refuses, leaves the machine without swap and says why in
/// the boot log; the machine still boots, because the disk is a
/// convenience and not a requirement.
///
/// The task is local to the calling processor, because the disk's
/// completions are (see [`install_swap`]).
pub fn install_scratch_swap<CpuImpl, Device, Backend, Error, Make>(
    registry: InstanceRegistry,
    scratch: ScratchDisk<Device>,
    spawner: Spawner<CpuImpl>,
    timer: Timer<CpuImpl>,
    cpu: CpuImpl,
    backend_name: &'static str,
    make: Make,
) -> Option<SwapHandle>
where
    CpuImpl: Cpu + Clone + Send + Sync + 'static,
    Device: BlockDevice,
    Backend: SwapBackend,
    Error: core::fmt::Display,
    Make: FnOnce(ScratchDisk<Device>, usize, usize) -> Result<Backend, Error>,
{
    let block_bytes = scratch.geometry().logical_block_bytes;
    let swap_blocks = scratch.swap_blocks();
    if swap_blocks == 0 {
        disable_swap(SwapDisabled::NoSwapDevice);
        return None;
    }
    let backend = match make(scratch, 0, swap_blocks) {
        Ok(backend) => backend,
        Err(error) => {
            tracing::warn!(
                target: "helios_kernel::swap",
                %error,
                "scratch disk cannot back swap"
            );
            disable_swap(SwapDisabled::NoSwapDevice);
            return None;
        }
    };
    match install_swap(
        spawner,
        timer,
        cpu,
        registry,
        backend,
        backend_name,
        (swap_blocks * block_bytes) as u64,
    ) {
        Ok(handle) => Some(handle),
        Err(error) => {
            tracing::warn!(
                target: "helios_kernel::swap",
                %error,
                "scratch disk could not supply swap's staging page"
            );
            disable_swap(SwapDisabled::NoSwapDevice);
            None
        }
    }
}

/// The disk the platform named [`super::SCRATCH_DISK_SERIAL`], typed as
/// the backend's own device so every request is a direct call.
#[derive(Clone)]
pub struct ScratchDisk<Device> {
    device: Device,
    stats: ScratchDiskStats,
}

impl<Device> ScratchDisk<Device>
where
    Device: BlockDevice,
{
    pub(super) fn new(device: Device) -> Self {
        Self {
            stats: ScratchDiskStats {
                geometry: device.geometry(),
                capabilities: device.capabilities(),
                topology: device.queue_topology(),
                counters: Arc::new(RequestCounters::default()),
            },
            device,
        }
    }

    /// A handle on this disk's counters that names no device type.
    pub fn stats_handle(&self) -> ScratchDiskStats {
        self.stats.clone()
    }

    pub fn stats(&self) -> BlockStats {
        self.stats.snapshot()
    }

    /// Blocks the boot self-check writes to, at the very end of the
    /// disk. Nothing else may be placed there.
    pub fn self_check_blocks(&self) -> usize {
        SELF_CHECK_BYTES.div_ceil(self.stats.geometry.logical_block_bytes)
    }

    /// Blocks available to swap: the whole disk except the ones the
    /// self-check owns.
    pub fn swap_blocks(&self) -> usize {
        self.stats
            .geometry
            .capacity_blocks
            .saturating_sub(self.self_check_blocks())
    }

    /// Proves the disk round-trips before anything depends on it.
    ///
    /// The pattern is random so a device that answers with a stale or
    /// mirrored buffer cannot pass, it lands on the last blocks of the
    /// device where nothing else is placed, and it is committed before
    /// the read so a volatile cache cannot answer from memory it never
    /// wrote through.
    pub async fn self_check(&self, root: &RootEntropyHandle) -> Result<(), BlockSelfCheckError>
    where
        Device::Buffer: OwnedBlockBuffer,
    {
        let geometry = self.stats.geometry;
        let block_bytes = geometry.logical_block_bytes;
        let blocks = SELF_CHECK_BYTES.div_ceil(block_bytes);
        if geometry.capacity_blocks < blocks {
            return Err(BlockSelfCheckError::TooSmall {
                capacity_blocks: geometry.capacity_blocks,
                block_bytes,
            });
        }
        let start_block = geometry.capacity_blocks - blocks;

        let mut pattern = [0_u8; SELF_CHECK_BYTES];
        let bytes = blocks * block_bytes;
        let pattern = &mut pattern[..bytes];
        root.fill(pattern);

        let mut written = Device::Buffer::zeroed(bytes);
        written.as_mut().copy_from_slice(pattern);
        self.write(start_block, written)
            .await
            .map_err(|failure| BlockSelfCheckError::Write(failure.into_error()))?;
        self.flush().await.map_err(BlockSelfCheckError::Flush)?;

        let ((), read) = self
            .read(start_block, Device::Buffer::zeroed(bytes))
            .await
            .map_err(|failure| BlockSelfCheckError::Read(failure.into_error()))?;
        let readback = read.as_ref();
        if readback != pattern {
            let block = readback
                .chunks(block_bytes)
                .zip(pattern.chunks(block_bytes))
                .position(|(read, written)| read != written)
                .unwrap_or(0);
            return Err(BlockSelfCheckError::Mismatch { block });
        }

        // The pattern is the kernel's, not the disk's content: give the
        // blocks back rather than leaving random bytes behind. A device
        // without the operation simply keeps them.
        if self
            .stats
            .capabilities
            .contains(BlockDeviceCapabilities::WRITE_ZEROES)
        {
            self.write_zeroes(BlockRange::new(start_block, blocks))
                .await
                .map_err(BlockSelfCheckError::Release)?;
        }
        Ok(())
    }
}

/// The scratch disk is itself a block device, so anything generic over
/// one — the swap backend, in particular — is built on it rather than on
/// the driver behind it, and its requests are counted like every other.
impl<Device> BlockDevice for ScratchDisk<Device>
where
    Device: BlockDevice,
{
    type Buffer = Device::Buffer;
    type Waiter<'a>
        = Device::Waiter<'a>
    where
        Self: 'a;

    async fn submit_read(&self, block_id: usize, buffer: Device::Buffer) -> Device::Waiter<'_> {
        self.stats.counters.reads.fetch_add(1, Ordering::Relaxed);
        self.device.submit_read(block_id, buffer).await
    }

    async fn submit_write(&self, block_id: usize, buffer: Device::Buffer) -> Device::Waiter<'_> {
        self.stats.counters.writes.fetch_add(1, Ordering::Relaxed);
        self.device.submit_write(block_id, buffer).await
    }

    async fn flush(&self) -> IoResult<()> {
        self.stats.counters.flushes.fetch_add(1, Ordering::Relaxed);
        self.device.flush().await
    }

    async fn discard(&self, range: BlockRange) -> IoResult<()> {
        self.stats.counters.discards.fetch_add(1, Ordering::Relaxed);
        self.device.discard(range).await
    }

    async fn write_zeroes(&self, range: BlockRange) -> IoResult<()> {
        self.stats
            .counters
            .write_zeroes
            .fetch_add(1, Ordering::Relaxed);
        self.device.write_zeroes(range).await
    }

    fn geometry(&self) -> BlockGeometry {
        self.stats.geometry
    }

    async fn serial(&self) -> IoResult<BlockSerial> {
        self.device.serial().await
    }

    fn capabilities(&self) -> BlockDeviceCapabilities {
        self.stats.capabilities
    }

    fn queue_topology(&self) -> BlockQueueTopology {
        self.stats.topology
    }

    fn is_read_only(&self) -> bool {
        self.device.is_read_only()
    }
}
