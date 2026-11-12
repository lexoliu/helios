//! The kernel's block devices: every disk the platform names, each in
//! the role its serial gives it.
//!
//! A machine hands the kernel several disks on one bus — the image it
//! was booted from, the scratch disk it may write, images to mount — and
//! nothing about their position on the bus says which is which. The
//! kernel therefore asks each one for its serial and gives it a
//! [`DiskRole`] by that name alone:
//!
//! - the disk named [`SCRATCH_DISK_SERIAL`] is the kernel's own: it is
//!   proved by a self-check and handed to the backend, typed as the
//!   backend's device, to back swap;
//! - every other disk that reports a serial is a *mount source*: it is
//!   put behind the kernel's block cache and published in
//!   [`BlockService`] under its serial, for the mount layer to name;
//! - a disk that reports no serial — the boot image — is left alone.
//!
//! Requests reach a device by static dispatch only. The scratch disk is
//! a [`ScratchDisk`] and the mount sources a [`BlockCache`], both over
//! the backend's device type, and both go back to the backend, which
//! names that type; a task that uses either talks to the device itself.
//! [`BlockService`], which the runtime state publishes, carries what
//! needs no device: the scratch disk's counters and the compressed
//! pool's settings.
//!
//! Concurrency contract: identification and the self-check run as one
//! task on the processor the devices' interrupts are routed to, before
//! anything is published. The scratch disk and the cache then follow
//! their own contracts (`scratch.rs`, `cache.rs`); [`BlockService`] is
//! an immutable, cloneable handle.

extern crate alloc;

mod cache;
mod pages;
mod scratch;

use alloc::vec::Vec;
use core::time::Duration;

use arrayvec::ArrayVec;
use helios_hal::cpu::Cpu;
use helios_hal::fs::{BlockDevice, BlockDeviceCapabilities, BlockSerial};
use helios_hal::watchdog::Watchdog;

pub use cache::{
    BlockCache, BlockCacheError, BlockCacheLimits, BlockMut, BlockRef, CACHE_BLOCK_BYTES,
    CacheLayoutError, CachedDisk, CachedDiskStats,
};
pub use pages::BlockPages;
pub use scratch::{BlockSelfCheckError, BlockStats, ScratchDisk, ScratchDiskStats};

use crate::memory::{CompressedPool, RootEntropyHandle};
use crate::{Kernel, MAX_BLOCK_DEVICES, PressureLevel, Timer};
use cache::{CacheDisk, DiskLayout};

/// The serial the platform gives the disk the kernel owns.
///
/// Identification is by name because bus order is not a contract: the
/// same VM hands the kernel its boot image, its scratch disk and the
/// images it mounts on the same bus, and only the name says which one
/// the kernel may write to for itself.
pub const SCRATCH_DISK_SERIAL: &str = "helios-data";

/// How often the cache re-reads the machine's memory pressure: the
/// cadence swap reads it at, so both answer the same reading.
const RECLAIM_TICK: Duration = crate::SWAP_TICK;

/// What the kernel does with a disk, decided by its serial.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskRole {
    /// The kernel's own disk, which backs swap.
    Scratch,
    /// A disk the mount layer may mount, looked up by its serial.
    MountSource,
    /// A disk with no name — the boot image — which the kernel leaves
    /// alone.
    Unused,
}

impl DiskRole {
    pub fn of(serial: &BlockSerial) -> Self {
        if serial.is_empty() {
            Self::Unused
        } else if serial.as_bytes() == SCRATCH_DISK_SERIAL.as_bytes() {
            Self::Scratch
        } else {
            Self::MountSource
        }
    }
}

/// Why the kernel could not take ownership of its disks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BlockInstallError {
    #[error(
        "no block device reported the serial {SCRATCH_DISK_SERIAL:?}; the kernel will not write to a disk it was not given"
    )]
    NoScratchDisk,
    #[error(
        "more than one block device reported the serial {SCRATCH_DISK_SERIAL:?}; the kernel cannot tell which one it owns"
    )]
    DuplicateScratchDisk,
    #[error("scratch disk self check failed: {0}")]
    SelfCheck(#[from] BlockSelfCheckError),
}

/// Why a mount-source disk was left unused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DiskRefusal {
    #[error("another disk already reported the serial {serial:?}")]
    DuplicateSerial { serial: BlockSerial },
    #[error("disk {serial:?} cannot be cached: {error}")]
    Layout {
        serial: BlockSerial,
        error: CacheLayoutError,
    },
}

/// Why a backend refused a block device before bringing it up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BlockRouteError {
    #[error(
        "block device {position} was refused: this backend routes interrupts for at most {limit} block devices"
    )]
    RoutesExhausted { position: usize, limit: usize },
}

/// Keeps the block devices a backend can route interrupts for, in bus
/// order, and refuses the rest with a [`BlockRouteError`] in the log.
///
/// Backends call this on their discovered candidates before bringing any
/// up, so a device the interrupt table could not reach is never
/// programmed at all.
pub fn admit_block_devices<Candidate>(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut admitted = candidates;
    for position in MAX_BLOCK_DEVICES..admitted.len() {
        let error = BlockRouteError::RoutesExhausted {
            position,
            limit: MAX_BLOCK_DEVICES,
        };
        tracing::error!(%error, "block device left unused");
    }
    admitted.truncate(MAX_BLOCK_DEVICES);
    admitted
}

/// What the kernel publishes about its disks that names no device: the
/// scratch disk's counters and the pool the block cache compresses
/// into, whose limit and level are operator settings.
#[derive(Clone)]
pub struct BlockService {
    scratch: ScratchDiskStats,
    pool: CompressedPool,
}

impl BlockService {
    /// What the scratch disk is and what the kernel has asked of it, for
    /// `helios:system/stats`.
    pub fn stats(&self) -> BlockStats {
        self.scratch.snapshot()
    }

    /// The pool the block cache compresses cold blocks into.
    pub fn compressed_pool(&self) -> &CompressedPool {
        &self.pool
    }
}

/// The disks [`identify`] sorted by role.
struct Identified<Device> {
    scratch: Device,
    mount_sources: ArrayVec<(BlockSerial, Device), MAX_BLOCK_DEVICES>,
}

/// Asks every disk its serial and sorts the disks by the role it gives.
async fn identify<Device>(devices: Vec<Device>) -> Result<Identified<Device>, BlockInstallError>
where
    Device: BlockDevice,
{
    let mut scratch = None;
    let mut mount_sources = ArrayVec::new();
    for device in devices {
        let serial = match device.serial().await {
            Ok(serial) => serial,
            Err(error) => {
                tracing::warn!(
                    ?error,
                    "block device did not answer its identification; it stays unused"
                );
                continue;
            }
        };
        match DiskRole::of(&serial) {
            DiskRole::Unused => {
                // The boot image, most of the time. Asking its name is
                // the only thing the kernel does to it.
                tracing::info!("block device reports no serial; it stays unused");
            }
            DiskRole::Scratch => {
                if scratch.replace(device).is_some() {
                    return Err(BlockInstallError::DuplicateScratchDisk);
                }
                tracing::info!(
                    ?serial,
                    "block device identified as the kernel scratch disk"
                );
            }
            DiskRole::MountSource => {
                if mount_sources.iter().any(|(taken, _)| *taken == serial) {
                    let refusal = DiskRefusal::DuplicateSerial { serial };
                    tracing::error!(%refusal, "block device left unused");
                    continue;
                }
                if mount_sources.try_push((serial, device)).is_err() {
                    let error = BlockRouteError::RoutesExhausted {
                        position: mount_sources.len(),
                        limit: MAX_BLOCK_DEVICES,
                    };
                    tracing::error!(%error, ?serial, "block device left unused");
                    continue;
                }
                tracing::info!(?serial, "block device identified as a mount source");
            }
        }
    }
    Ok(Identified {
        scratch: scratch.ok_or(BlockInstallError::NoScratchDisk)?,
        mount_sources,
    })
}

/// Puts every mount source the cache can serve behind one cache, in bus
/// order.
fn open_mount_sources<Device>(
    mount_sources: ArrayVec<(BlockSerial, Device), MAX_BLOCK_DEVICES>,
    limits: BlockCacheLimits,
    processors: usize,
    pool: CompressedPool,
) -> Option<BlockCache<Device>>
where
    Device: BlockDevice<Buffer = BlockPages>,
{
    let mut disks = ArrayVec::<CacheDisk<Device>, MAX_BLOCK_DEVICES>::new();
    for (serial, device) in mount_sources {
        let layout = match DiskLayout::new(device.geometry()) {
            Ok(layout) => layout,
            Err(error) => {
                let refusal = DiskRefusal::Layout { serial, error };
                tracing::error!(%refusal, "block device left unused");
                continue;
            }
        };
        disks.push(CacheDisk {
            serial,
            layout,
            read_only: device.is_read_only(),
            device,
        });
    }
    if disks.is_empty() {
        return None;
    }
    Some(BlockCache::new(limits, processors, pool, disks))
}

/// The machine's memory pressure, as the block cache answers it: free
/// memory across the kernel heap and the user pool, against everything
/// usable.
fn machine_pressure() -> PressureLevel {
    let machine = crate::machine_memory();
    if machine.usable_bytes == 0 {
        return PressureLevel::Green;
    }
    PressureLevel::from_free_fraction(machine.free_bytes as f32 / machine.usable_bytes as f32)
}

/// Re-reads memory pressure every [`RECLAIM_TICK`] and shrinks the cache
/// while it is above green, writing back what it evicts dirty itself.
async fn reclaim_under_pressure<CpuImpl, Device>(
    cache: BlockCache<Device>,
    timer: Timer<CpuImpl>,
) -> !
where
    CpuImpl: Cpu + Clone,
    Device: BlockDevice<Buffer = BlockPages>,
{
    loop {
        timer.sleep_for(RECLAIM_TICK).await;
        let level = machine_pressure();
        cache.set_pressure(level);
        let released = cache.shrink(level).await;
        if released != 0 {
            tracing::debug!(
                released,
                ?level,
                resident = cache.resident_blocks(),
                "block cache shrank under memory pressure"
            );
        }
    }
}

/// Identifies the devices a backend brought up, proves the scratch disk,
/// puts the mount sources behind the block cache and publishes both.
///
/// `install` receives the service, the scratch disk for swap, and the
/// block cache over the mount sources when there are any, both typed as
/// the backend's device. The identification task and the cache's
/// reclaim task run on the calling processor; every other request a
/// disk sees is issued by the task that needs it, on its own processor.
pub fn install_block_devices<CpuImpl, WatchdogImpl, MeasuredBootImpl, Device, Install>(
    kernel: &Kernel<CpuImpl, WatchdogImpl, MeasuredBootImpl>,
    root: RootEntropyHandle,
    devices: Vec<Device>,
    install: Install,
) where
    CpuImpl: Cpu + Clone + Send + Sync + 'static,
    WatchdogImpl: Watchdog + Clone,
    MeasuredBootImpl: helios_hal::measured_boot::MeasuredBoot,
    Device: BlockDevice<Buffer = BlockPages> + 'static,
    Install: FnOnce(BlockService, ScratchDisk<Device>, Option<BlockCache<Device>>) + 'static,
{
    if devices.is_empty() {
        tracing::warn!("no block device was discovered on the platform bus");
        return;
    }
    let spawner = kernel.spawner();
    let timer = kernel.timer();
    let processors = kernel.topology().configured_processors;
    kernel.spawn_local_detached(async move {
        let identified = match identify(devices).await {
            Ok(identified) => identified,
            Err(error) => panic!("{error}"),
        };
        let scratch = ScratchDisk::new(identified.scratch);
        if let Err(error) = scratch.self_check(&root).await {
            panic!("{}", BlockInstallError::SelfCheck(error));
        }
        log_scratch_disk(&scratch);

        let usable_bytes = crate::machine_memory().usable_bytes;
        let limits = BlockCacheLimits::for_machine(usable_bytes);
        let pool = CompressedPool::for_machine(usable_bytes, processors);
        let cache = open_mount_sources(identified.mount_sources, limits, processors, pool.clone());
        if let Some(cache) = &cache {
            for disk in cache.disks() {
                tracing::info!(
                    serial = ?disk.serial(),
                    capacity_blocks = disk.capacity_blocks(),
                    block_bytes = CACHE_BLOCK_BYTES,
                    read_only = disk.is_read_only(),
                    "mount source online"
                );
            }
            spawner.spawn_local_detached(reclaim_under_pressure(cache.clone(), timer));
        }
        let service = BlockService {
            scratch: scratch.stats_handle(),
            pool,
        };
        install(service, scratch, cache);
    });
}

fn log_scratch_disk<Device: BlockDevice>(scratch: &ScratchDisk<Device>) {
    let stats = scratch.stats();
    let capabilities = BlockDeviceCapabilities::from_bits_truncate(stats.capabilities);
    tracing::info!(
        serial = SCRATCH_DISK_SERIAL,
        capacity_bytes = stats.capacity_bytes,
        block_bytes = stats.block_bytes,
        physical_block_bytes = stats.physical_block_bytes,
        queues = stats.queues,
        queue_depth = stats.queue_depth,
        flush = capabilities.contains(BlockDeviceCapabilities::FLUSH),
        discard = capabilities.contains(BlockDeviceCapabilities::DISCARD),
        write_zeroes = capabilities.contains(BlockDeviceCapabilities::WRITE_ZEROES),
        "block device online, self check passed"
    );
}

#[cfg(test)]
mod tests;
