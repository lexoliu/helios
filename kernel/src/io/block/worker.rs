//! The I/O tasks that serve one cached disk from its device.
//!
//! The cache is shared by every processor and names no device type; the
//! device is a backend type the kernel only meets as a generic. The two
//! meet here: each mount-source disk gets a fixed set of worker tasks,
//! monomorphized over the backend's device, that drain the disk's
//! request ring and complete the cache entries the requests name. This
//! is how the rest of the kernel's device services draw the same line
//! (the display and audio owner tasks): the shared handle is concrete,
//! the device is generic, and nothing between them is dynamically
//! dispatched.
//!
//! Concurrency contract: any processor pushes requests into the ring,
//! a lock-free bounded queue allocated once with the disk. The workers
//! run on the processor that brought the device up, where its
//! completions are delivered, and pop from it; there are as many of
//! them as the device keeps requests in flight on one queue, so the
//! device's pipeline is what bounds the disk's concurrency. A request is
//! a `Copy` value naming an entry, so neither submitting nor serving one
//! allocates. A full ring parks the submitter on the disk's space gate
//! until a worker pops.

use core::sync::atomic::{AtomicU64, Ordering, fence};

use concurrent_queue::{ConcurrentQueue, PushError};
use helios_hal::fs::{BlockDevice, BlockSerial};
use helios_hal::io::IoError;
use spin::Mutex;

use super::cache::{BlockCache, CACHE_BLOCK_BYTES, CacheDisk, CachedDiskStats, DiskLayout, Gate};
use super::cache::{key_block, key_disk};
use crate::Notify;

/// One request a worker serves. Entries named here are pinned for the
/// request by its submitter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Request {
    /// Fill the locked entry from the disk.
    Read(u32),
    /// Write the entry, held in write-back, to the disk.
    Write(u32),
    /// Commit the device's write cache.
    Flush,
}

/// The cache's state for one disk: its ring, its flush sequence and the
/// errors its device reported.
pub(super) struct DiskQueue {
    pub(super) serial: BlockSerial,
    pub(super) layout: DiskLayout,
    pub(super) read_only: bool,
    ring: ConcurrentQueue<Request>,
    /// One permit per pushed request, so exactly one worker wakes for it.
    submitted: Notify,
    space: Gate,
    /// Flushes asked for, numbered from one.
    flush_requested: AtomicU64,
    /// The highest flush number a successful device flush covered.
    flush_completed: AtomicU64,
    /// The highest flush number a failed device flush covered.
    flush_failed: AtomicU64,
    flushed: Gate,
    /// Bumped by every failed write; a sync compares it across itself.
    write_errors: AtomicU64,
    /// What the device said when it last failed a request.
    last_error: Mutex<Option<IoError>>,
    misses: AtomicU64,
    device_reads: AtomicU64,
    readahead: AtomicU64,
    device_writes: AtomicU64,
    flushes: AtomicU64,
    read_errors: AtomicU64,
}

impl DiskQueue {
    pub(super) fn new(disk: CacheDisk) -> Self {
        Self {
            serial: disk.serial,
            layout: disk.layout,
            read_only: disk.read_only,
            ring: ConcurrentQueue::bounded(disk.queue_capacity.max(1)),
            submitted: Notify::new(),
            space: Gate::new(),
            flush_requested: AtomicU64::new(0),
            flush_completed: AtomicU64::new(0),
            flush_failed: AtomicU64::new(0),
            flushed: Gate::new(),
            write_errors: AtomicU64::new(0),
            last_error: Mutex::new(None),
            misses: AtomicU64::new(0),
            device_reads: AtomicU64::new(0),
            readahead: AtomicU64::new(0),
            device_writes: AtomicU64::new(0),
            flushes: AtomicU64::new(0),
            read_errors: AtomicU64::new(0),
        }
    }

    /// Queues `request`, parking while the ring is full.
    pub(super) async fn submit(&self, request: Request) {
        let mut request = request;
        loop {
            match self.ring.push(request) {
                Ok(()) => {
                    self.submitted.notify_one();
                    return;
                }
                Err(PushError::Full(returned)) => request = returned,
                Err(PushError::Closed(_)) => panic!("a block request ring was closed"),
            }
            self.space.wait_until(|| !self.ring.is_full()).await;
        }
    }

    /// Queues `request` if the ring has room.
    pub(super) fn try_submit(&self, request: Request) -> bool {
        match self.ring.push(request) {
            Ok(()) => {
                self.submitted.notify_one();
                true
            }
            Err(PushError::Full(_)) => false,
            Err(PushError::Closed(_)) => panic!("a block request ring was closed"),
        }
    }

    /// Numbers a new flush. Every write completed before this call is
    /// covered by the device flush that answers it.
    pub(super) fn request_flush(&self) -> u64 {
        self.flush_requested.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Parks until a device flush covered `ticket`, and says whether it
    /// succeeded.
    pub(super) async fn wait_flush(&self, ticket: u64) -> bool {
        self.flushed
            .wait_until(|| {
                self.flush_completed.load(Ordering::SeqCst) >= ticket
                    || self.flush_failed.load(Ordering::SeqCst) >= ticket
            })
            .await;
        self.flush_failed.load(Ordering::SeqCst) < ticket
    }

    pub(super) fn write_errors(&self) -> u64 {
        self.write_errors.load(Ordering::Acquire)
    }

    pub(super) fn last_error(&self) -> IoError {
        self.last_error.lock().unwrap_or(IoError::DeviceFault)
    }

    pub(super) fn count_miss(&self) {
        self.misses.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn count_readahead(&self) {
        self.readahead.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn stats(&self) -> CachedDiskStats {
        CachedDiskStats {
            misses: self.misses.load(Ordering::Relaxed),
            device_reads: self.device_reads.load(Ordering::Relaxed),
            readahead: self.readahead.load(Ordering::Relaxed),
            device_writes: self.device_writes.load(Ordering::Relaxed),
            flushes: self.flushes.load(Ordering::Relaxed),
            read_errors: self.read_errors.load(Ordering::Relaxed),
            write_errors: self.write_errors.load(Ordering::Relaxed),
        }
    }

    fn record_error(&self, error: IoError) {
        *self.last_error.lock() = Some(error);
    }
}

/// Serves disk `disk` of `cache` from `device`, forever.
///
/// The backend spawns as many of these per disk as the device keeps
/// requests in flight on a queue.
pub(super) async fn serve<Device>(cache: BlockCache, disk: usize, device: Device) -> !
where
    Device: BlockDevice,
{
    let shared = cache.shared();
    let queue = &shared.disks[disk];
    loop {
        let wait = queue.submitted.notified();
        let request = match queue.ring.pop() {
            Ok(request) => request,
            Err(_) => {
                wait.await;
                continue;
            }
        };
        drop(wait);
        // The pop is the change a submitter parked on a full ring waits
        // for; publish it before looking for one.
        fence(Ordering::SeqCst);
        queue.space.open();
        match request {
            Request::Read(id) => {
                let first = device_block(queue, disk, shared.entry_key(id));
                // SAFETY: the entry is locked for this read, so nothing
                // else reads or writes the buffer until it completes.
                let buffer = unsafe {
                    core::slice::from_raw_parts_mut(shared.buffer(id), CACHE_BLOCK_BYTES)
                };
                let result = device.read_block(first, buffer).await;
                queue.device_reads.fetch_add(1, Ordering::Relaxed);
                if let Err(error) = result {
                    queue.read_errors.fetch_add(1, Ordering::Relaxed);
                    queue.record_error(error);
                    tracing::warn!(?error, serial = ?queue.serial, block = first, "block cache read failed");
                }
                shared.complete_read(id, result.is_ok());
            }
            Request::Write(id) => {
                let first = device_block(queue, disk, shared.entry_key(id));
                // SAFETY: the entry is in write-back with a share held for
                // this write, so no writer changes the buffer meanwhile.
                let buffer =
                    unsafe { core::slice::from_raw_parts(shared.buffer(id), CACHE_BLOCK_BYTES) };
                let result = device.write_block(first, buffer).await;
                queue.device_writes.fetch_add(1, Ordering::Relaxed);
                if let Err(error) = result {
                    queue.record_error(error);
                    queue.write_errors.fetch_add(1, Ordering::AcqRel);
                    tracing::warn!(?error, serial = ?queue.serial, block = first, "block cache write-back failed");
                }
                shared.complete_write(id, result.is_ok());
            }
            Request::Flush => {
                // Every flush numbered up to here was asked for after the
                // writes it covers completed, and the device flush below
                // is issued after this read.
                let covered = queue.flush_requested.load(Ordering::Acquire);
                let result = device.flush().await;
                queue.flushes.fetch_add(1, Ordering::Relaxed);
                match result {
                    Ok(()) => {
                        queue.flush_completed.fetch_max(covered, Ordering::SeqCst);
                    }
                    Err(error) => {
                        queue.record_error(error);
                        queue.flush_failed.fetch_max(covered, Ordering::SeqCst);
                        tracing::warn!(?error, serial = ?queue.serial, "block cache flush failed");
                    }
                }
                queue.flushed.open();
            }
        }
    }
}

/// The first logical block of the device the entry's key names.
fn device_block(queue: &DiskQueue, disk: usize, key: u64) -> usize {
    assert_eq!(
        key_disk(key),
        disk,
        "a request on disk {disk}'s ring names an entry of disk {}",
        key_disk(key)
    );
    let block = key_block(key) as usize;
    block * queue.layout.device_blocks_per_block
}
