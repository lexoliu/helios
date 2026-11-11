extern crate std;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

use futures_lite::future::{block_on, or};
use helios_hal::cpu::ProcessorId;
use helios_hal::fs::{
    BlockDevice, BlockDeviceCapabilities, BlockGeometry, BlockQueueTopology, BlockRange,
    BlockSerial, IoError, IoResult,
};
use spin::Mutex;

use super::cache::{CacheDisk, DiskLayout, dirty_blocks};
use super::*;
use crate::memory::{CompressedPool, RootEntropy};
use crate::test_support::TestCpu;

/// What a [`MemoryDisk`] was asked to do, in logical blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Read(usize),
    Write(usize),
    Flush,
}

struct DiskState {
    bytes: Mutex<Vec<u8>>,
    log: Mutex<Vec<Op>>,
    block_bytes: usize,
    opt_io_blocks: u32,
    serial: &'static str,
    read_only: bool,
    fail_reads: bool,
    /// Mirrors a device whose write cache is never committed: reads
    /// answer something other than the last write.
    corrupt_readback: bool,
}

/// A disk in host memory that answers a fixed serial and logs every
/// request it serves.
#[derive(Clone)]
struct MemoryDisk {
    state: Arc<DiskState>,
}

impl MemoryDisk {
    fn build(serial: &'static str, block_bytes: usize, blocks: usize) -> DiskState {
        let mut bytes = alloc::vec![0_u8; block_bytes * blocks];
        // Every cache-sized block starts filled with its own number, so
        // a read that lands on the wrong block cannot pass.
        for (index, chunk) in bytes.chunks_mut(CACHE_BLOCK_BYTES).enumerate() {
            chunk.fill(index as u8);
        }
        DiskState {
            bytes: Mutex::new(bytes),
            log: Mutex::new(Vec::new()),
            block_bytes,
            opt_io_blocks: 0,
            serial,
            read_only: false,
            fail_reads: false,
            corrupt_readback: false,
        }
    }

    fn new(serial: &'static str) -> Self {
        Self::from(Self::build(serial, 512, 64))
    }

    fn with_blocks(serial: &'static str, block_bytes: usize, blocks: usize) -> Self {
        Self::from(Self::build(serial, block_bytes, blocks))
    }

    fn from(state: DiskState) -> Self {
        Self {
            state: Arc::new(state),
        }
    }

    fn log(&self) -> Vec<Op> {
        self.state.log.lock().clone()
    }

    fn reads(&self) -> usize {
        self.log()
            .iter()
            .filter(|op| matches!(op, Op::Read(_)))
            .count()
    }

    fn bytes_at(&self, offset: usize, len: usize) -> Vec<u8> {
        self.state.bytes.lock()[offset..offset + len].to_vec()
    }
}

impl BlockDevice for MemoryDisk {
    async fn read_block(&self, block_id: usize, buf: &mut [u8]) -> IoResult<()> {
        self.state.log.lock().push(Op::Read(block_id));
        if self.state.fail_reads {
            return Err(IoError::DeviceFault);
        }
        let offset = block_id * self.state.block_bytes;
        buf.copy_from_slice(&self.state.bytes.lock()[offset..offset + buf.len()]);
        if self.state.corrupt_readback {
            buf[0] ^= 0xff;
        }
        Ok(())
    }

    async fn write_block(&self, block_id: usize, buf: &[u8]) -> IoResult<()> {
        if self.state.read_only {
            return Err(IoError::ReadOnly);
        }
        self.state.log.lock().push(Op::Write(block_id));
        let offset = block_id * self.state.block_bytes;
        self.state.bytes.lock()[offset..offset + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    async fn flush(&self) -> IoResult<()> {
        self.state.log.lock().push(Op::Flush);
        Ok(())
    }

    async fn discard(&self, _range: BlockRange) -> IoResult<()> {
        Err(IoError::Unsupported)
    }

    async fn write_zeroes(&self, range: BlockRange) -> IoResult<()> {
        let offset = range.start_block * self.state.block_bytes;
        let len = range.block_count * self.state.block_bytes;
        self.state.bytes.lock()[offset..offset + len].fill(0);
        Ok(())
    }

    async fn serial(&self) -> IoResult<BlockSerial> {
        BlockSerial::new(self.state.serial.as_bytes()).ok_or(IoError::DeviceFault)
    }

    fn geometry(&self) -> BlockGeometry {
        BlockGeometry {
            logical_block_bytes: self.state.block_bytes,
            physical_block_exp: 0,
            min_io_blocks: 0,
            opt_io_blocks: self.state.opt_io_blocks,
            max_segments: 1,
            max_segment_bytes: u32::MAX,
            capacity_blocks: self.state.bytes.lock().len() / self.state.block_bytes,
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

    fn is_read_only(&self) -> bool {
        self.state.read_only
    }
}

fn root() -> RootEntropyHandle {
    RootEntropyHandle::new(
        RootEntropy::from_platform(&TestCpu::with_entropy(0x5a), None, None)
            .expect("the test CPU has an entropy source"),
    )
}

/// A cache over one disk, sized and shaped by the test.
fn cache_over(disk: &MemoryDisk, max_blocks: usize, processors: usize) -> BlockCache {
    let layout = DiskLayout::new(disk.geometry()).expect("the test disk fits the cache");
    BlockCache::new(
        BlockCacheLimits { max_blocks },
        processors,
        CompressedPool::new(1 << 20, processors),
        [CacheDisk {
            serial: BlockSerial::new(disk.state.serial.as_bytes()).expect("serial fits"),
            layout,
            read_only: disk.is_read_only(),
            queue_capacity: 16,
        }],
    )
}

/// One worker serving the disk, as a future of any output so it can be
/// raced against the test.
async fn serving<T>(cache: BlockCache, disk: MemoryDisk) -> T {
    match worker::serve(cache, 0, disk).await {}
}

/// Runs `test` with one worker serving the cache's only disk.
fn run<T>(cache: &BlockCache, disk: &MemoryDisk, test: impl Future<Output = T>) -> T {
    block_on(or(test, serving(cache.clone(), disk.clone())))
}

fn on_processor(id: u16) {
    crate::test_processor_identity::set(ProcessorId::new(id));
}

#[test]
fn disks_take_their_role_from_their_serial() {
    let disks = alloc::vec![
        MemoryDisk::new(""),
        MemoryDisk::new(SCRATCH_DISK_SERIAL),
        MemoryDisk::new("images"),
    ];

    let identified = block_on(identify(disks)).expect("the scratch disk is present");

    assert_eq!(
        block_on(identified.scratch.serial())
            .expect("serial")
            .as_str(),
        Some(SCRATCH_DISK_SERIAL)
    );
    let mount_sources: Vec<_> = identified
        .mount_sources
        .iter()
        .map(|(serial, _)| serial.as_str())
        .collect();
    assert_eq!(mount_sources, [Some("images")]);
}

#[test]
fn a_second_disk_with_a_taken_serial_stays_unused() {
    let disks = alloc::vec![
        MemoryDisk::new(SCRATCH_DISK_SERIAL),
        MemoryDisk::new("images"),
        MemoryDisk::new("images"),
    ];

    let identified = block_on(identify(disks)).expect("the scratch disk is present");

    assert_eq!(identified.mount_sources.len(), 1);
}

#[test]
fn a_bus_without_the_scratch_disk_is_a_boot_failure() {
    let disks = alloc::vec![MemoryDisk::new(""), MemoryDisk::new("images")];

    assert_eq!(
        block_on(identify(disks)).err(),
        Some(BlockInstallError::NoScratchDisk)
    );
}

#[test]
fn two_scratch_disks_are_a_boot_failure() {
    let disks = alloc::vec![
        MemoryDisk::new(SCRATCH_DISK_SERIAL),
        MemoryDisk::new(SCRATCH_DISK_SERIAL),
    ];

    assert!(matches!(
        block_on(identify(disks)),
        Err(BlockInstallError::DuplicateScratchDisk)
    ));
}

#[test]
fn block_devices_past_the_interrupt_routes_are_refused() {
    let candidates: Vec<usize> = (0..MAX_BLOCK_DEVICES + 2).collect();

    let admitted = admit_block_devices(candidates);

    assert_eq!(admitted, (0..MAX_BLOCK_DEVICES).collect::<Vec<_>>());
    assert_eq!(
        BlockRouteError::RoutesExhausted {
            position: MAX_BLOCK_DEVICES,
            limit: MAX_BLOCK_DEVICES,
        }
        .to_string(),
        alloc::format!(
            "block device {MAX_BLOCK_DEVICES} was refused: this backend routes interrupts for at most {MAX_BLOCK_DEVICES} block devices"
        )
    );
}

#[test]
fn a_disk_that_does_not_round_trip_fails_its_self_check() {
    let mut state = MemoryDisk::build(SCRATCH_DISK_SERIAL, 512, 64);
    state.corrupt_readback = true;
    let scratch = ScratchDisk::new(MemoryDisk::from(state));

    assert_eq!(
        block_on(scratch.self_check(&root())),
        Err(BlockSelfCheckError::Mismatch { block: 0 })
    );
}

#[test]
fn the_self_check_leaves_the_pattern_behind_zeroed_and_counts_its_requests() {
    let disk = MemoryDisk::new(SCRATCH_DISK_SERIAL);
    let scratch = ScratchDisk::new(disk.clone());

    block_on(scratch.self_check(&root())).expect("self check should pass");

    let stats = scratch.stats();
    assert_eq!(stats.writes, 1);
    assert_eq!(stats.flushes, 1);
    assert_eq!(stats.reads, 1);
    assert_eq!(stats.write_zeroes, 1);
    assert_eq!(stats.capacity_bytes, 64 * 512);
    assert_eq!(stats.block_bytes, 512);
    assert_eq!(stats.queue_depth, 128);
    assert_eq!(disk.bytes_at(56 * 512, 4096), [0_u8; 4096]);
}

#[test]
fn a_miss_reads_the_device_once_and_a_hit_reads_nothing() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    let cache = cache_over(&disk, 16, 2);
    let view = cache.disk(0);

    run(&cache, &disk, async {
        on_processor(0);
        let missed = view.read(3).await.expect("block 3 reads");
        assert!(missed.iter().all(|byte| *byte == 3));
        drop(missed);
        // The same processor finds it in its lookaside, another one in
        // the shared index.
        let hit = view.read(3).await.expect("block 3 is cached");
        assert!(hit.iter().all(|byte| *byte == 3));
        drop(hit);
        on_processor(1);
        let hit = view.read(3).await.expect("block 3 is cached");
        assert!(hit.iter().all(|byte| *byte == 3));
        on_processor(0);
    });

    assert_eq!(disk.log(), [Op::Read(3)]);
    assert_eq!(view.stats().misses, 1);
    assert_eq!(view.stats().device_reads, 1);
}

#[test]
fn two_concurrent_misses_on_one_block_issue_one_device_read() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    let cache = cache_over(&disk, 16, 2);
    let view = cache.disk(0);
    let mut context = Context::from_waker(Waker::noop());
    let mut first = pin!(view.read(7));
    let mut second = pin!(view.read(7));
    let mut worker = pin!(worker::serve(cache.clone(), 0, disk.clone()));

    // Both processors miss before the device answers either.
    on_processor(0);
    assert!(first.as_mut().poll(&mut context).is_pending());
    on_processor(1);
    assert!(second.as_mut().poll(&mut context).is_pending());
    assert!(worker.as_mut().poll(&mut context).is_pending());

    on_processor(1);
    let Poll::Ready(Ok(second)) = second.as_mut().poll(&mut context) else {
        panic!("the second miss should be answered by the first one's read");
    };
    on_processor(0);
    let Poll::Ready(Ok(first)) = first.as_mut().poll(&mut context) else {
        panic!("the first miss should be answered by its own read");
    };
    assert!(first.iter().all(|byte| *byte == 7));
    assert!(second.iter().all(|byte| *byte == 7));
    assert_eq!(disk.log(), [Op::Read(7)]);
    assert_eq!(view.stats().misses, 1);
}

#[test]
fn a_commit_writes_dirty_blocks_then_flushes_then_writes_the_commit_block_then_flushes() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    run(&cache, &disk, async {
        view.overwrite(5).await.expect("block 5").fill(0x55);
        view.overwrite(9).await.expect("block 9").fill(0x99);
        assert_eq!(
            disk.log(),
            [],
            "dirty blocks wait for write-back rather than going straight to the disk"
        );
        view.commit(0, |superblock| superblock[..4].copy_from_slice(b"HLIO"))
            .await
            .expect("the commit succeeds");
    });

    let writes_and_flushes: Vec<Op> = disk
        .log()
        .into_iter()
        .filter(|op| !matches!(op, Op::Read(_)))
        .collect();
    assert_eq!(writes_and_flushes.len(), 5, "log: {writes_and_flushes:?}");
    let mut data = [writes_and_flushes[0], writes_and_flushes[1]];
    data.sort_by_key(|op| match op {
        Op::Write(block) => *block,
        _ => usize::MAX,
    });
    assert_eq!(data, [Op::Write(5), Op::Write(9)]);
    assert_eq!(
        writes_and_flushes[2..],
        [Op::Flush, Op::Write(0), Op::Flush]
    );
    assert_eq!(disk.bytes_at(5 * CACHE_BLOCK_BYTES, 4), [0x55; 4]);
    assert_eq!(disk.bytes_at(0, 8), *b"HLIO\0\0\0\0");
    assert!(dirty_blocks(&cache).is_empty());
}

#[test]
fn a_write_keeps_the_rest_of_the_block_and_sync_makes_it_durable() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    run(&cache, &disk, async {
        view.write(2).await.expect("block 2")[0] = 0xee;
        assert_eq!(dirty_blocks(&cache), [2]);
        view.sync().await.expect("the sync succeeds");
    });

    assert_eq!(disk.log(), [Op::Read(2), Op::Write(2), Op::Flush]);
    let block = disk.bytes_at(2 * CACHE_BLOCK_BYTES, CACHE_BLOCK_BYTES);
    assert_eq!(block[0], 0xee);
    assert!(block[1..].iter().all(|byte| *byte == 2));
}

#[test]
fn a_miss_reads_the_devices_optimal_transfer_ahead() {
    // 512-byte sectors and a 16 KiB optimal transfer: four cache blocks.
    let mut state = MemoryDisk::build("images", 512, 128);
    state.opt_io_blocks = 32;
    let disk = MemoryDisk::from(state);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    run(&cache, &disk, async {
        assert!(
            view.read(5)
                .await
                .expect("block 5")
                .iter()
                .all(|byte| *byte == 5)
        );
        assert!(
            view.read(6)
                .await
                .expect("block 6")
                .iter()
                .all(|byte| *byte == 6)
        );
        assert!(
            view.read(4)
                .await
                .expect("block 4")
                .iter()
                .all(|byte| *byte == 4)
        );
    });

    let mut reads = disk.log();
    reads.sort_by_key(|op| match op {
        Op::Read(block) => *block,
        _ => usize::MAX,
    });
    assert_eq!(
        reads,
        [Op::Read(32), Op::Read(40), Op::Read(48), Op::Read(56)]
    );
    assert_eq!(view.stats().readahead, 3);
}

#[test]
fn a_full_cache_recycles_its_coldest_clean_block() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    let cache = cache_over(&disk, 2, 1);
    let view = cache.disk(0);

    run(&cache, &disk, async {
        for block in [0, 1, 2, 0] {
            let read = view.read(block).await.expect("the block reads");
            assert!(read.iter().all(|byte| *byte == block as u8));
        }
    });

    assert_eq!(cache.resident_blocks(), 2);
    assert_eq!(disk.reads(), 4);
}

#[test]
fn memory_pressure_returns_clean_blocks_but_keeps_dirty_ones() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    run(&cache, &disk, async {
        for block in 0..4 {
            drop(view.read(block).await.expect("the block reads"));
        }
        view.overwrite(8).await.expect("block 8").fill(0x88);
    });
    assert_eq!(cache.resident_blocks(), 5);

    cache.set_pressure(PressureLevel::Red);
    assert_eq!(cache.shrink(PressureLevel::Red), 4);
    assert_eq!(cache.resident_blocks(), 1);
    // Red pressure also started the dirty block's write-back, so the
    // next pass can take it.
    assert!(dirty_blocks(&cache).is_empty());

    run(&cache, &disk, async {
        assert!(
            view.read(1)
                .await
                .expect("block 1 again")
                .iter()
                .all(|byte| *byte == 1)
        );
        assert!(
            view.read(8)
                .await
                .expect("block 8")
                .iter()
                .all(|byte| *byte == 0x88)
        );
    });
    assert_eq!(disk.bytes_at(8 * CACHE_BLOCK_BYTES, 4), [0x88; 4]);
}

/// Bytes no compressor shrinks below the pool's threshold.
fn noise(seed: u64) -> [u8; CACHE_BLOCK_BYTES] {
    let mut state = seed | 1;
    core::array::from_fn(|_| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as u8
    })
}

/// Yellow-pressure passes until every cold block has had its second
/// chance and been compressed.
fn compress_cold_blocks(cache: &BlockCache) {
    cache.set_pressure(PressureLevel::Yellow);
    for _ in 0..4 {
        cache.shrink(PressureLevel::Yellow);
    }
}

#[test]
fn a_cold_block_is_compressed_and_a_hit_on_it_reads_nothing() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    run(&cache, &disk, async {
        drop(view.read(3).await.expect("block 3"));
    });
    compress_cold_blocks(&cache);
    assert_eq!(cache.compressed_blocks(), 1);
    assert_eq!(cache.resident_blocks(), 1);
    assert_eq!(cache.compressed_pool().stats().stored_pages, 1);

    let reads = disk.reads();
    run(&cache, &disk, async {
        let block = view.read(3).await.expect("block 3 again");
        assert!(block.iter().all(|byte| *byte == 3));
    });

    assert_eq!(disk.reads(), reads);
    assert_eq!(cache.compressed_blocks(), 0);
    let stats = cache.compressed_pool().stats();
    assert_eq!((stats.loads, stats.stored_pages), (1, 0));
}

#[test]
fn red_pressure_drops_clean_compressed_blocks_and_writes_back_dirty_ones() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    run(&cache, &disk, async {
        drop(view.read(1).await.expect("block 1"));
        view.overwrite(8).await.expect("block 8").fill(0x88);
    });
    compress_cold_blocks(&cache);
    assert_eq!(cache.compressed_blocks(), 2);
    assert_eq!(dirty_blocks(&cache), [8]);
    assert!(!disk.log().contains(&Op::Write(8)));

    cache.set_pressure(PressureLevel::Red);
    cache.shrink(PressureLevel::Red);

    // The clean block left the cache; the dirty one came back into a
    // buffer with its write-back queued.
    assert_eq!(cache.compressed_blocks(), 0);
    assert_eq!(cache.resident_blocks(), 1);
    assert_eq!(cache.compressed_pool().stats().stored_pages, 0);
    assert!(dirty_blocks(&cache).is_empty());

    run(&cache, &disk, async {
        view.sync().await.expect("the write-back lands");
    });
    let writes: Vec<_> = disk
        .log()
        .into_iter()
        .filter(|op| matches!(op, Op::Write(_)))
        .collect();
    assert_eq!(writes, [Op::Write(8)]);
    assert_eq!(disk.bytes_at(8 * CACHE_BLOCK_BYTES, 4), [0x88; 4]);
}

#[test]
fn sync_writes_a_dirty_block_held_compressed_uncompressed() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    run(&cache, &disk, async {
        view.overwrite(5).await.expect("block 5").fill(0x55);
    });
    compress_cold_blocks(&cache);
    assert_eq!(cache.compressed_blocks(), 1);
    assert_eq!(disk.log(), []);

    run(&cache, &disk, async {
        view.sync().await.expect("sync");
    });

    assert_eq!(disk.log(), [Op::Write(5), Op::Flush]);
    assert_eq!(
        disk.bytes_at(5 * CACHE_BLOCK_BYTES, CACHE_BLOCK_BYTES),
        [0x55; CACHE_BLOCK_BYTES]
    );
    assert!(dirty_blocks(&cache).is_empty());
    assert_eq!(cache.compressed_blocks(), 0);
}

#[test]
fn an_incompressible_block_skips_the_compressed_tier() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    disk.state.bytes.lock()[4 * CACHE_BLOCK_BYTES..5 * CACHE_BLOCK_BYTES]
        .copy_from_slice(&noise(4));
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    run(&cache, &disk, async {
        drop(view.read(4).await.expect("block 4"));
        view.overwrite(2)
            .await
            .expect("block 2")
            .copy_from_slice(&noise(2));
    });
    compress_cold_blocks(&cache);

    // The clean block was dropped and the dirty one sent to the disk.
    assert_eq!(cache.compressed_blocks(), 0);
    assert_eq!(cache.compressed_pool().stats().incompressible, 2);
    assert_eq!(cache.resident_blocks(), 1);
    assert!(dirty_blocks(&cache).is_empty());

    run(&cache, &disk, async {
        view.sync().await.expect("the write-back lands");
    });
    assert!(disk.log().contains(&Op::Write(2)));
    assert_eq!(
        disk.bytes_at(2 * CACHE_BLOCK_BYTES, CACHE_BLOCK_BYTES),
        noise(2)
    );
}

#[test]
fn a_read_only_disk_refuses_writes_before_reaching_the_device() {
    let mut state = MemoryDisk::build("images", CACHE_BLOCK_BYTES, 16);
    state.read_only = true;
    let disk = MemoryDisk::from(state);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    run(&cache, &disk, async {
        assert!(matches!(
            view.overwrite(1).await,
            Err(BlockCacheError::ReadOnly)
        ));
        assert!(matches!(
            view.commit(0, |_| {}).await,
            Err(BlockCacheError::ReadOnly)
        ));
    });

    assert_eq!(disk.log(), []);
}

#[test]
fn a_failed_read_is_reported_and_a_later_read_retries() {
    let mut state = MemoryDisk::build("images", CACHE_BLOCK_BYTES, 16);
    state.fail_reads = true;
    let disk = MemoryDisk::from(state);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    run(&cache, &disk, async {
        assert!(matches!(
            view.read(3).await,
            Err(BlockCacheError::Device(IoError::DeviceFault))
        ));
        assert!(view.read(3).await.is_err());
    });

    assert_eq!(disk.log(), [Op::Read(3), Op::Read(3)]);
    assert_eq!(view.stats().read_errors, 2);
}

#[test]
fn reads_past_the_end_are_refused() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 4);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);
    let mut bytes = [0_u8; 8];

    run(&cache, &disk, async {
        assert!(matches!(
            view.read(4).await,
            Err(BlockCacheError::OutOfRange {
                block: 4,
                capacity_blocks: 4
            })
        ));
        view.read_at(CACHE_BLOCK_BYTES as u64 - 4, &mut bytes)
            .await
            .expect("a read across two blocks");
        assert!(
            view.read_at(4 * CACHE_BLOCK_BYTES as u64 - 4, &mut bytes)
                .await
                .is_err()
        );
    });

    assert_eq!(bytes, [0, 0, 0, 0, 1, 1, 1, 1]);
}

#[test]
fn a_mount_source_whose_blocks_do_not_divide_a_page_is_refused() {
    let disk = MemoryDisk::with_blocks("images", 3 * 512, 64);
    let mut mount_sources = ArrayVec::<_, MAX_BLOCK_DEVICES>::new();
    mount_sources.push((BlockSerial::new(b"images").expect("fits"), disk));

    assert!(
        open_mount_sources(
            mount_sources,
            BlockCacheLimits { max_blocks: 16 },
            1,
            CompressedPool::new(1 << 20, 1),
        )
        .is_none()
    );
}
