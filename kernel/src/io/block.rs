//! The kernel's block device: the scratch disk the platform gives it.
//!
//! A machine hands the kernel more than one disk — the image it was
//! booted from and the scratch disk it may write — and nothing about
//! their position on the bus says which is which. The kernel therefore
//! asks each candidate for its serial and keeps only the one the
//! platform named [`SCRATCH_DISK_SERIAL`]; every other disk is dropped
//! untouched, boot images included.
//!
//! The chosen disk is proved end-to-end before it is published: a random
//! pattern goes to the last blocks of the device, is committed, read
//! back and compared, and the blocks are released again. A boot that
//! reaches components with a block device that does not round-trip is
//! worse than a boot that stops here, so a mismatch is fatal.
//!
//! Concurrency contract: identification and the self-check run as one
//! task on the processor the device's interrupts are routed to, before
//! the service is published. Afterwards [`BlockService`] is a shared
//! handle whose operations are the driver's own async requests; the
//! service itself holds no lock and only counts what it forwarded.

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU64, Ordering};

use helios_hal::cpu::Cpu;
use helios_hal::fs::{
    BlockDevice, BlockDeviceCapabilities, BlockGeometry, BlockQueueTopology, BlockRange,
    BlockSerial, IoError, IoResult,
};
use helios_hal::watchdog::Watchdog;

use crate::Kernel;
use crate::memory::RootEntropyHandle;

/// The serial the platform gives the disk the kernel owns.
///
/// Identification is by name because bus order is not a contract: the
/// same VM hands the kernel its boot image and its scratch disk on the
/// same bus, and only the name says which one the kernel may write to.
pub const SCRATCH_DISK_SERIAL: &str = "helios-data";

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

/// Why the kernel could not take ownership of a scratch disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BlockInstallError {
    #[error(
        "no block device reported the serial {SCRATCH_DISK_SERIAL:?}; the kernel will not write to a disk it was not given"
    )]
    NoScratchDisk,
    #[error("scratch disk self check failed: {0}")]
    SelfCheck(#[from] BlockSelfCheckError),
}

/// Counters and fixed properties the kernel reports for its disk.
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

/// The block device the kernel owns.
///
/// The concrete device is a backend type — a virtio-blk resource behind
/// whichever transport the platform exposes it on — so the service
/// stores it behind a trait object rather than making every consumer of
/// [`crate::RuntimeState`] generic over a device it never names. This is
/// the same boundary the network service draws for the same reason.
#[derive(Clone)]
pub struct BlockService {
    device: Arc<dyn DynBlockDevice>,
    geometry: BlockGeometry,
    capabilities: BlockDeviceCapabilities,
    topology: BlockQueueTopology,
    counters: Arc<RequestCounters>,
}

#[derive(Debug, Default)]
struct RequestCounters {
    reads: AtomicU64,
    writes: AtomicU64,
    flushes: AtomicU64,
    discards: AtomicU64,
    write_zeroes: AtomicU64,
}

impl BlockService {
    /// Wraps a platform block device as the kernel's disk.
    pub fn new<Device>(device: Device) -> Self
    where
        Device: BlockDevice + 'static,
    {
        Self {
            geometry: device.geometry(),
            capabilities: device.capabilities(),
            topology: device.queue_topology(),
            device: Arc::new(device),
            counters: Arc::new(RequestCounters::default()),
        }
    }

    pub fn geometry(&self) -> BlockGeometry {
        self.geometry
    }

    pub fn capabilities(&self) -> BlockDeviceCapabilities {
        self.capabilities
    }

    pub fn queue_topology(&self) -> BlockQueueTopology {
        self.topology
    }

    pub async fn read_block(&self, block_id: usize, buf: &mut [u8]) -> IoResult<()> {
        self.counters.reads.fetch_add(1, Ordering::Relaxed);
        self.device.read_block(block_id, buf).await
    }

    pub async fn write_block(&self, block_id: usize, buf: &[u8]) -> IoResult<()> {
        self.counters.writes.fetch_add(1, Ordering::Relaxed);
        self.device.write_block(block_id, buf).await
    }

    pub async fn flush(&self) -> IoResult<()> {
        self.counters.flushes.fetch_add(1, Ordering::Relaxed);
        self.device.flush().await
    }

    pub async fn discard(&self, range: BlockRange) -> IoResult<()> {
        self.counters.discards.fetch_add(1, Ordering::Relaxed);
        self.device.discard(range).await
    }

    pub async fn write_zeroes(&self, range: BlockRange) -> IoResult<()> {
        self.counters.write_zeroes.fetch_add(1, Ordering::Relaxed);
        self.device.write_zeroes(range).await
    }

    pub async fn serial(&self) -> IoResult<BlockSerial> {
        self.device.serial().await
    }

    /// A snapshot of what the disk is and what the kernel has asked of
    /// it, for `helios:system/stats`.
    pub fn stats(&self) -> BlockStats {
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

    /// Proves the disk round-trips before anything depends on it.
    ///
    /// The pattern is random so a device that answers with a stale or
    /// mirrored buffer cannot pass, it lands on the last blocks of the
    /// device where nothing else is placed, and it is committed before
    /// the read so a volatile cache cannot answer from memory it never
    /// wrote through.
    pub async fn self_check(&self, root: &RootEntropyHandle) -> Result<(), BlockSelfCheckError> {
        let block_bytes = self.geometry.logical_block_bytes;
        let blocks = SELF_CHECK_BYTES.div_ceil(block_bytes);
        if self.geometry.capacity_blocks < blocks {
            return Err(BlockSelfCheckError::TooSmall {
                capacity_blocks: self.geometry.capacity_blocks,
                block_bytes,
            });
        }
        let start_block = self.geometry.capacity_blocks - blocks;

        let mut pattern = [0_u8; SELF_CHECK_BYTES];
        let bytes = blocks * block_bytes;
        let pattern = &mut pattern[..bytes];
        root.fill(pattern);

        self.write_block(start_block, pattern)
            .await
            .map_err(BlockSelfCheckError::Write)?;
        self.flush().await.map_err(BlockSelfCheckError::Flush)?;

        let mut readback = [0_u8; SELF_CHECK_BYTES];
        let readback = &mut readback[..bytes];
        self.read_block(start_block, readback)
            .await
            .map_err(BlockSelfCheckError::Read)?;
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

/// Chooses the kernel's disk out of the devices a backend brought up and
/// publishes it once it has proved itself.
///
/// `install` receives the service; it is the backend's runtime state
/// slot. The task runs on the calling processor because that is where
/// the devices' completions are delivered.
pub fn install_block_devices<CpuImpl, WatchdogImpl, Device, Install>(
    kernel: &Kernel<CpuImpl, WatchdogImpl>,
    root: RootEntropyHandle,
    devices: Vec<Device>,
    install: Install,
) where
    CpuImpl: Cpu + Clone + Send + Sync + 'static,
    WatchdogImpl: Watchdog + Clone,
    Device: BlockDevice + 'static,
    Install: FnOnce(BlockService) + 'static,
{
    if devices.is_empty() {
        tracing::warn!("no block device was discovered on the platform bus");
        return;
    }
    kernel.spawn_local_detached(async move {
        let service = match open_scratch_disk(devices, &root).await {
            Ok(service) => service,
            Err(error) => panic!("{error}"),
        };
        let stats = service.stats();
        tracing::info!(
            serial = SCRATCH_DISK_SERIAL,
            capacity_bytes = stats.capacity_bytes,
            block_bytes = stats.block_bytes,
            physical_block_bytes = stats.physical_block_bytes,
            queues = stats.queues,
            queue_depth = stats.queue_depth,
            flush = service
                .capabilities()
                .contains(BlockDeviceCapabilities::FLUSH),
            discard = service
                .capabilities()
                .contains(BlockDeviceCapabilities::DISCARD),
            write_zeroes = service
                .capabilities()
                .contains(BlockDeviceCapabilities::WRITE_ZEROES),
            "block device online, self check passed"
        );
        install(service);
    });
}

async fn open_scratch_disk<Device>(
    devices: Vec<Device>,
    root: &RootEntropyHandle,
) -> Result<BlockService, BlockInstallError>
where
    Device: BlockDevice + 'static,
{
    let mut scratch = None;
    for device in devices {
        let service = BlockService::new(device);
        match service.serial().await {
            Ok(serial) if serial.as_str() == Some(SCRATCH_DISK_SERIAL) => {
                tracing::info!(
                    ?serial,
                    "block device identified as the kernel scratch disk"
                );
                scratch = Some(service);
            }
            Ok(serial) => {
                // Every other disk on the bus belongs to someone else —
                // the boot image, most of the time. Reading its name is
                // the only thing the kernel does to it.
                tracing::info!(?serial, "block device is not the kernel scratch disk");
            }
            Err(error) => {
                tracing::warn!(?error, "block device did not answer its identification");
            }
        }
    }

    let scratch = scratch.ok_or(BlockInstallError::NoScratchDisk)?;
    scratch.self_check(root).await?;
    Ok(scratch)
}

/// The object-safe view of [`BlockDevice`] the service stores.
trait DynBlockDevice: Send + Sync + 'static {
    fn read_block<'a>(
        &'a self,
        block_id: usize,
        buf: &'a mut [u8],
    ) -> Pin<Box<dyn Future<Output = IoResult<()>> + Send + 'a>>;

    fn write_block<'a>(
        &'a self,
        block_id: usize,
        buf: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = IoResult<()>> + Send + 'a>>;

    fn flush(&self) -> Pin<Box<dyn Future<Output = IoResult<()>> + Send + '_>>;

    fn discard(&self, range: BlockRange)
    -> Pin<Box<dyn Future<Output = IoResult<()>> + Send + '_>>;

    fn write_zeroes(
        &self,
        range: BlockRange,
    ) -> Pin<Box<dyn Future<Output = IoResult<()>> + Send + '_>>;

    fn serial(&self) -> Pin<Box<dyn Future<Output = IoResult<BlockSerial>> + Send + '_>>;
}

impl<Device> DynBlockDevice for Device
where
    Device: BlockDevice + 'static,
{
    fn read_block<'a>(
        &'a self,
        block_id: usize,
        buf: &'a mut [u8],
    ) -> Pin<Box<dyn Future<Output = IoResult<()>> + Send + 'a>> {
        Box::pin(BlockDevice::read_block(self, block_id, buf))
    }

    fn write_block<'a>(
        &'a self,
        block_id: usize,
        buf: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = IoResult<()>> + Send + 'a>> {
        Box::pin(BlockDevice::write_block(self, block_id, buf))
    }

    fn flush(&self) -> Pin<Box<dyn Future<Output = IoResult<()>> + Send + '_>> {
        Box::pin(BlockDevice::flush(self))
    }

    fn discard(
        &self,
        range: BlockRange,
    ) -> Pin<Box<dyn Future<Output = IoResult<()>> + Send + '_>> {
        Box::pin(BlockDevice::discard(self, range))
    }

    fn write_zeroes(
        &self,
        range: BlockRange,
    ) -> Pin<Box<dyn Future<Output = IoResult<()>> + Send + '_>> {
        Box::pin(BlockDevice::write_zeroes(self, range))
    }

    fn serial(&self) -> Pin<Box<dyn Future<Output = IoResult<BlockSerial>> + Send + '_>> {
        Box::pin(BlockDevice::serial(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::memory::RootEntropy;
    use crate::test_support::TestCpu;
    use futures_lite::future::block_on;
    use spin::Mutex;

    /// A disk in host memory that answers a fixed serial.
    struct MemoryDisk {
        blocks: Mutex<Vec<u8>>,
        block_bytes: usize,
        serial: &'static str,
        /// Mirrors a device whose write cache is never committed: reads
        /// answer from the last write only once a flush happened.
        corrupt_readback: bool,
    }

    impl MemoryDisk {
        fn new(serial: &'static str) -> Self {
            Self {
                blocks: Mutex::new(alloc::vec![0_u8; 64 * 512]),
                block_bytes: 512,
                serial,
                corrupt_readback: false,
            }
        }

        fn corrupting(serial: &'static str) -> Self {
            Self {
                corrupt_readback: true,
                ..Self::new(serial)
            }
        }
    }

    impl BlockDevice for MemoryDisk {
        async fn read_block(&self, block_id: usize, buf: &mut [u8]) -> IoResult<()> {
            let offset = block_id * self.block_bytes;
            buf.copy_from_slice(&self.blocks.lock()[offset..offset + buf.len()]);
            if self.corrupt_readback {
                buf[0] ^= 0xff;
            }
            Ok(())
        }

        async fn write_block(&self, block_id: usize, buf: &[u8]) -> IoResult<()> {
            let offset = block_id * self.block_bytes;
            self.blocks.lock()[offset..offset + buf.len()].copy_from_slice(buf);
            Ok(())
        }

        async fn flush(&self) -> IoResult<()> {
            Ok(())
        }

        async fn discard(&self, _range: BlockRange) -> IoResult<()> {
            Err(IoError::Unsupported)
        }

        async fn write_zeroes(&self, range: BlockRange) -> IoResult<()> {
            let offset = range.start_block * self.block_bytes;
            let len = range.block_count * self.block_bytes;
            self.blocks.lock()[offset..offset + len].fill(0);
            Ok(())
        }

        async fn serial(&self) -> IoResult<BlockSerial> {
            BlockSerial::new(self.serial.as_bytes()).ok_or(IoError::DeviceFault)
        }

        fn geometry(&self) -> BlockGeometry {
            BlockGeometry {
                logical_block_bytes: self.block_bytes,
                physical_block_exp: 0,
                min_io_blocks: 0,
                opt_io_blocks: 0,
                max_segments: 1,
                max_segment_bytes: u32::MAX,
                capacity_blocks: self.blocks.lock().len() / self.block_bytes,
            }
        }

        fn capabilities(&self) -> BlockDeviceCapabilities {
            BlockDeviceCapabilities::FLUSH | BlockDeviceCapabilities::WRITE_ZEROES
        }

        fn queue_topology(&self) -> BlockQueueTopology {
            BlockQueueTopology {
                queues: 1,
                depth: 128,
            }
        }
    }

    fn root() -> RootEntropyHandle {
        RootEntropyHandle::new(
            RootEntropy::from_platform(&TestCpu::with_entropy(0x5a), None)
                .expect("the test CPU has an entropy source"),
        )
    }

    #[test]
    fn the_scratch_disk_is_chosen_by_serial_and_the_others_are_left_alone() {
        let disks = alloc::vec![
            MemoryDisk::new("boot-image"),
            MemoryDisk::new(SCRATCH_DISK_SERIAL),
        ];

        let service = block_on(open_scratch_disk(disks, &root())).expect("scratch disk is present");

        assert_eq!(
            block_on(service.serial()).expect("serial").as_str(),
            Some(SCRATCH_DISK_SERIAL)
        );
    }

    #[test]
    fn a_bus_without_the_scratch_disk_is_a_boot_failure() {
        let disks = alloc::vec![MemoryDisk::new("boot-image")];

        assert_eq!(
            block_on(open_scratch_disk(disks, &root())).err(),
            Some(BlockInstallError::NoScratchDisk)
        );
    }

    #[test]
    fn a_disk_that_does_not_round_trip_is_a_boot_failure() {
        let disks = alloc::vec![MemoryDisk::corrupting(SCRATCH_DISK_SERIAL)];

        assert_eq!(
            block_on(open_scratch_disk(disks, &root())).err(),
            Some(BlockInstallError::SelfCheck(
                BlockSelfCheckError::Mismatch { block: 0 }
            ))
        );
    }

    #[test]
    fn the_self_check_leaves_the_pattern_behind_zeroed_and_counts_its_requests() {
        let disk = MemoryDisk::new(SCRATCH_DISK_SERIAL);
        let service = BlockService::new(disk);

        block_on(service.self_check(&root())).expect("self check should pass");

        let stats = service.stats();
        assert_eq!(stats.writes, 1);
        assert_eq!(stats.flushes, 1);
        assert_eq!(stats.reads, 1);
        assert_eq!(stats.write_zeroes, 1);
        assert_eq!(stats.capacity_bytes, 64 * 512);
        assert_eq!(stats.block_bytes, 512);
        assert_eq!(stats.queue_depth, 128);

        let mut tail = [0xff_u8; 4096];
        block_on(service.read_block(56, &mut tail)).expect("read the self-check range");
        assert_eq!(tail, [0_u8; 4096]);
    }
}
