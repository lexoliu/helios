extern crate std;

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::{Pin, pin};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use core::task::{Context, Poll, Waker};

use futures_lite::future::block_on;
use helios_hal::cpu::ProcessorId;
use helios_hal::fs::{
    BlockBuffer, BlockCompletion, BlockDevice, BlockDeviceCapabilities, BlockGeometry,
    BlockQueueTopology, BlockRange, BlockSerial, IoError, IoResult,
};
use spin::Mutex;

use super::cache::{CacheDisk, DiskLayout, dirty_blocks, idle_blocks};
use super::*;
use crate::memory::{CompressedPool, RootEntropy};
use crate::test_support::TestCpu;

/// What a [`MemoryDisk`] was asked to do: the first logical block and
/// the logical blocks one request covered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Read(usize, usize),
    Write(usize, usize),
    Flush,
}

/// Something a test runs inside a device write, to look at the cache
/// while the request is in flight.
type WriteProbe = Box<dyn Fn() + Send + Sync>;

/// Which way a [`MemoryDisk`] data request moves bytes.
#[derive(Clone, Copy)]
enum Direction {
    Read,
    Write,
}

/// Where a request's answer meets its waiter.
#[derive(Default)]
struct Answer {
    completion: Option<BlockCompletion<BlockPages>>,
    waker: Option<Waker>,
    /// The waiter was dropped: the answer goes nowhere.
    detached: bool,
}

/// A data request the disk accepted and has not answered: it owns the
/// buffer until it does, whatever happens to its waiter.
struct Accepted {
    direction: Direction,
    block_id: usize,
    buffer: BlockPages,
    answer: Arc<Mutex<Answer>>,
}

/// The data requests a paused disk holds.
#[derive(Default)]
struct Queue {
    paused: bool,
    accepted: Vec<Accepted>,
}

/// The waiter of one [`MemoryDisk`] data request. Dropping it detaches
/// it; the request is still answered and its buffer completed.
struct MemoryWaiter {
    answer: Arc<Mutex<Answer>>,
}

impl Future for MemoryWaiter {
    type Output = BlockCompletion<BlockPages>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let mut answer = self.answer.lock();
        match answer.completion.take() {
            Some(completion) => Poll::Ready(completion),
            None => {
                answer.waker = Some(context.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl Drop for MemoryWaiter {
    fn drop(&mut self) {
        let completion = {
            let mut answer = self.answer.lock();
            answer.detached = true;
            answer.completion.take()
        };
        drop(completion);
    }
}

struct DiskState {
    bytes: Mutex<Vec<u8>>,
    log: Mutex<Vec<Op>>,
    block_bytes: usize,
    opt_io_blocks: u32,
    max_segments: u32,
    serial: &'static str,
    read_only: bool,
    fail_reads: bool,
    /// A logical block whose writes the device refuses.
    fail_writes_at: Option<usize>,
    /// Mirrors a device whose write cache is never committed: reads
    /// answer something other than the last write.
    corrupt_readback: bool,
    /// While paused, the disk holds the data requests it accepts until
    /// something answers them, as a device that has not answered yet.
    queue: Mutex<Queue>,
    on_write: Mutex<Option<WriteProbe>>,
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
            max_segments: 32,
            serial,
            read_only: false,
            fail_reads: false,
            fail_writes_at: None,
            corrupt_readback: false,
            queue: Mutex::new(Queue::default()),
            on_write: Mutex::new(None),
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
            .filter(|op| matches!(op, Op::Read(..)))
            .count()
    }

    fn writes(&self) -> Vec<Op> {
        self.log()
            .into_iter()
            .filter(|op| matches!(op, Op::Write(..)))
            .collect()
    }

    fn bytes_at(&self, offset: usize, len: usize) -> Vec<u8> {
        self.state.bytes.lock()[offset..offset + len].to_vec()
    }

    /// Holds every data request accepted from now on until it is
    /// answered.
    fn pause(&self) {
        self.state.queue.lock().paused = true;
    }

    /// Answers every held request, and answers the next ones as they
    /// arrive.
    fn resume(&self) {
        let accepted = {
            let mut queue = self.state.queue.lock();
            queue.paused = false;
            core::mem::take(&mut queue.accepted)
        };
        for request in accepted {
            self.carry_out(request);
        }
    }

    /// Answers every request held so far and stays paused: the device's
    /// completion path, running beside the tasks that submit.
    fn answer_accepted(&self) {
        let accepted = core::mem::take(&mut self.state.queue.lock().accepted);
        for request in accepted {
            self.carry_out(request);
        }
    }

    /// Requests accepted and not answered yet.
    fn accepted(&self) -> usize {
        self.state.queue.lock().accepted.len()
    }

    fn submit(&self, direction: Direction, block_id: usize, buffer: BlockPages) -> MemoryWaiter {
        let answer = Arc::new(Mutex::new(Answer::default()));
        let request = Accepted {
            direction,
            block_id,
            buffer,
            answer: Arc::clone(&answer),
        };
        let mut queue = self.state.queue.lock();
        if queue.paused {
            queue.accepted.push(request);
        } else {
            drop(queue);
            self.carry_out(request);
        }
        MemoryWaiter { answer }
    }

    /// Answers one request: moves its bytes, completes its buffer, and
    /// hands both to its waiter, or drops the buffer when nobody waits.
    fn carry_out(&self, request: Accepted) {
        let Accepted {
            direction,
            block_id,
            mut buffer,
            answer,
        } = request;
        let result = match direction {
            Direction::Read => {
                let mut runs: Vec<&mut [u8]> = buffer
                    .segments()
                    // SAFETY: the disk holds the request, and with it the
                    // only right to touch the buffer's runs until it
                    // completes the buffer.
                    .map(|run| unsafe { &mut *run.as_ptr() })
                    .collect();
                self.read_into(block_id, &mut runs)
            }
            Direction::Write => {
                let runs: Vec<&[u8]> = buffer
                    .segments()
                    // SAFETY: as above; a write only reads the runs.
                    .map(|run| unsafe { &*run.as_ptr() })
                    .collect();
                self.write_from(block_id, &runs)
            }
        };
        buffer.complete(result);
        let mut slot = answer.lock();
        if slot.detached {
            drop(slot);
            drop(buffer);
            return;
        }
        slot.completion = Some(BlockCompletion { result, buffer });
        let waker = slot.waker.take();
        drop(slot);
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn read_into(&self, block_id: usize, bufs: &mut [&mut [u8]]) -> IoResult<()> {
        let blocks: usize =
            bufs.iter().map(|buf| buf.len()).sum::<usize>() / self.state.block_bytes;
        self.state.log.lock().push(Op::Read(block_id, blocks));
        if self.state.fail_reads {
            return Err(IoError::DeviceFault);
        }
        let bytes = self.state.bytes.lock();
        let mut offset = block_id * self.state.block_bytes;
        for buf in bufs.iter_mut() {
            buf.copy_from_slice(&bytes[offset..offset + buf.len()]);
            offset += buf.len();
        }
        if self.state.corrupt_readback {
            bufs[0][0] ^= 0xff;
        }
        Ok(())
    }

    fn write_from(&self, block_id: usize, bufs: &[&[u8]]) -> IoResult<()> {
        if self.state.read_only {
            return Err(IoError::ReadOnly);
        }
        if let Some(probe) = self.state.on_write.lock().as_ref() {
            probe();
        }
        let len: usize = bufs.iter().map(|buf| buf.len()).sum();
        let blocks = len / self.state.block_bytes;
        self.state.log.lock().push(Op::Write(block_id, blocks));
        if self
            .state
            .fail_writes_at
            .is_some_and(|failing| (block_id..block_id + blocks).contains(&failing))
        {
            return Err(IoError::DeviceFault);
        }
        let mut bytes = self.state.bytes.lock();
        let mut offset = block_id * self.state.block_bytes;
        for buf in bufs {
            bytes[offset..offset + buf.len()].copy_from_slice(buf);
            offset += buf.len();
        }
        Ok(())
    }
}

impl BlockDevice for MemoryDisk {
    type Buffer = BlockPages;
    type Waiter<'a>
        = MemoryWaiter
    where
        Self: 'a;

    async fn submit_read(&self, block_id: usize, buffer: BlockPages) -> MemoryWaiter {
        self.submit(Direction::Read, block_id, buffer)
    }

    async fn submit_write(&self, block_id: usize, buffer: BlockPages) -> MemoryWaiter {
        self.submit(Direction::Write, block_id, buffer)
    }

    /// Answered at once, paused or not: a flush carries no memory, so
    /// holding it shows nothing about who owns a request's buffer.
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
            max_segments: self.state.max_segments,
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
        RootEntropy::from_platform(
            &TestCpu::with_entropy(0x5a),
            None,
            None,
            crate::MachineGeneration::new(None),
        )
        .expect("the test CPU has an entropy source"),
    )
}

/// A cache over one disk, sized and shaped by the test.
fn cache_over(disk: &MemoryDisk, max_blocks: usize, processors: usize) -> BlockCache<MemoryDisk> {
    let layout = DiskLayout::new(disk.geometry()).expect("the test disk fits the cache");
    BlockCache::new(
        BlockCacheLimits { max_blocks },
        processors,
        CompressedPool::new(1 << 20, processors),
        [CacheDisk {
            serial: BlockSerial::new(disk.state.serial.as_bytes()).expect("serial fits"),
            layout,
            read_only: disk.is_read_only(),
            device: disk.clone(),
        }],
    )
}

fn on_processor(id: u16) {
    crate::test_processor_identity::set(ProcessorId::new(id));
}

fn shrink(cache: &BlockCache<MemoryDisk>, level: PressureLevel) -> usize {
    cache.set_pressure(level);
    block_on(cache.shrink(level))
}

/// Yellow-pressure passes until every cold block has had its second
/// chance and been compressed: a pass moves an eighth of the cache.
fn compress_cold_blocks(cache: &BlockCache<MemoryDisk>) {
    for _ in 0..64 {
        shrink(cache, PressureLevel::Yellow);
    }
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

    block_on(async {
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

    assert_eq!(disk.log(), [Op::Read(3, 1)]);
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

    // Both processors miss before the device answers either: the first
    // issues the read from its own task, the second parks on its entry.
    disk.pause();
    on_processor(0);
    assert!(first.as_mut().poll(&mut context).is_pending());
    on_processor(1);
    assert!(second.as_mut().poll(&mut context).is_pending());
    disk.resume();

    on_processor(0);
    let Poll::Ready(Ok(first)) = first.as_mut().poll(&mut context) else {
        panic!("the first miss should be answered by its own read");
    };
    on_processor(1);
    let Poll::Ready(Ok(second)) = second.as_mut().poll(&mut context) else {
        panic!("the second miss should be answered by the first one's read");
    };
    on_processor(0);
    assert!(first.iter().all(|byte| *byte == 7));
    assert!(second.iter().all(|byte| *byte == 7));
    assert_eq!(disk.log(), [Op::Read(7, 1)]);
    assert_eq!(view.stats().misses, 1);
}

#[test]
fn a_commit_writes_dirty_blocks_then_flushes_then_writes_the_commit_block_then_flushes() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    block_on(async {
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
        .filter(|op| !matches!(op, Op::Read(..)))
        .collect();
    assert_eq!(writes_and_flushes.len(), 5, "log: {writes_and_flushes:?}");
    let mut data = [writes_and_flushes[0], writes_and_flushes[1]];
    data.sort_by_key(|op| match op {
        Op::Write(block, _) => *block,
        _ => usize::MAX,
    });
    assert_eq!(data, [Op::Write(5, 1), Op::Write(9, 1)]);
    assert_eq!(
        writes_and_flushes[2..],
        [Op::Flush, Op::Write(0, 1), Op::Flush]
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

    block_on(async {
        view.write(2).await.expect("block 2")[0] = 0xee;
        assert_eq!(dirty_blocks(&cache), [2]);
        view.sync().await.expect("the sync succeeds");
    });

    assert_eq!(disk.log(), [Op::Read(2, 1), Op::Write(2, 1), Op::Flush]);
    let block = disk.bytes_at(2 * CACHE_BLOCK_BYTES, CACHE_BLOCK_BYTES);
    assert_eq!(block[0], 0xee);
    assert!(block[1..].iter().all(|byte| *byte == 2));
}

#[test]
fn contiguous_dirty_blocks_are_written_back_in_one_request() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    block_on(async {
        for block in [6, 4, 5, 7, 11] {
            view.overwrite(block)
                .await
                .expect("the block")
                .fill(block as u8 | 0x80);
        }
        view.sync().await.expect("the sync succeeds");
        // Nothing is dirty any more: a second sync only flushes.
        view.sync().await.expect("the second sync succeeds");
    });

    let mut writes = disk.writes();
    writes.sort_by_key(|op| match op {
        Op::Write(block, _) => *block,
        _ => usize::MAX,
    });
    assert_eq!(writes, [Op::Write(4, 4), Op::Write(11, 1)]);
    assert_eq!(disk.log().iter().filter(|op| **op == Op::Flush).count(), 2);
    for block in [4, 5, 6, 7, 11] {
        assert_eq!(
            disk.bytes_at(block * CACHE_BLOCK_BYTES, 4),
            [block as u8 | 0x80; 4]
        );
    }
}

#[test]
fn a_miss_reads_the_devices_optimal_transfer_ahead_in_one_request() {
    // 512-byte sectors and a 16 KiB optimal transfer: four cache blocks.
    let mut state = MemoryDisk::build("images", 512, 128);
    state.opt_io_blocks = 32;
    let disk = MemoryDisk::from(state);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    block_on(async {
        for block in [5_u64, 6, 4] {
            let read = view.read(block).await.expect("the block reads");
            assert!(read.iter().all(|byte| *byte == block as u8));
        }
    });

    assert_eq!(disk.log(), [Op::Read(32, 32)]);
    assert_eq!(view.stats().readahead, 3);
    assert_eq!(view.stats().device_reads, 1);
}

#[test]
fn readahead_is_bounded_whatever_transfer_the_device_claims() {
    let geometry = |opt_io_blocks, max_segments| BlockGeometry {
        logical_block_bytes: 512,
        physical_block_exp: 0,
        min_io_blocks: 0,
        opt_io_blocks,
        max_segments,
        max_segment_bytes: u32::MAX,
        capacity_blocks: 1 << 30,
    };

    // A device claiming a 2 TiB optimal transfer gets Linux's 128 KiB
    // window.
    let layout = DiskLayout::new(geometry(u32::MAX, u32::MAX)).expect("layout");
    assert_eq!((layout.readahead_blocks, layout.run_blocks), (32, 32));
    // One request carries no more blocks than the device has segments.
    let layout = DiskLayout::new(geometry(u32::MAX, 4)).expect("layout");
    assert_eq!((layout.readahead_blocks, layout.run_blocks), (4, 4));
    // A segment smaller than a cache block takes several per block.
    let layout = DiskLayout::new(BlockGeometry {
        max_segment_bytes: 1024,
        ..geometry(u32::MAX, 8)
    })
    .expect("layout");
    assert_eq!(layout.run_blocks, 2);
}

#[test]
fn a_full_cache_recycles_its_coldest_clean_block() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    let cache = cache_over(&disk, 2, 1);
    let view = cache.disk(0);

    block_on(async {
        for block in [0, 1, 2, 0] {
            let read = view.read(block).await.expect("the block reads");
            assert!(read.iter().all(|byte| *byte == block as u8));
        }
    });

    assert_eq!(cache.resident_blocks(), 2);
    assert_eq!(disk.reads(), 4);
}

#[test]
fn red_pressure_drops_clean_blocks_and_writes_dirty_ones_back_first() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    block_on(async {
        for block in 0..4 {
            drop(view.read(block).await.expect("the block reads"));
        }
        view.overwrite(8).await.expect("block 8").fill(0x88);
    });
    assert_eq!(cache.resident_blocks(), 5);

    assert_eq!(shrink(&cache, PressureLevel::Red), 5);
    assert_eq!(cache.resident_blocks(), 0);
    assert_eq!(cache.page_buffers(), 0);
    assert!(dirty_blocks(&cache).is_empty());
    assert_eq!(disk.writes(), [Op::Write(8, 1)]);
    assert_eq!(disk.bytes_at(8 * CACHE_BLOCK_BYTES, 4), [0x88; 4]);

    cache.set_pressure(PressureLevel::Green);
    block_on(async {
        let block = view.read(8).await.expect("block 8");
        assert!(block.iter().all(|byte| *byte == 0x88));
    });
}

#[test]
fn a_cold_block_is_compressed_and_a_hit_on_it_reads_nothing() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    block_on(async {
        drop(view.read(3).await.expect("block 3"));
    });
    compress_cold_blocks(&cache);
    assert_eq!(cache.compressed_blocks(), 1);
    assert_eq!(cache.resident_blocks(), 1);
    assert_eq!(cache.page_buffers(), 0);
    assert_eq!(cache.compressed_pool().stats().stored_pages, 1);

    let reads = disk.reads();
    block_on(async {
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

    block_on(async {
        drop(view.read(1).await.expect("block 1"));
        view.overwrite(8).await.expect("block 8").fill(0x88);
    });
    compress_cold_blocks(&cache);
    assert_eq!(cache.compressed_blocks(), 2);
    assert_eq!(dirty_blocks(&cache), [8]);
    assert!(disk.writes().is_empty());

    shrink(&cache, PressureLevel::Red);

    // Both left the cache; the dirty one reached the disk uncompressed
    // first, from a page staged for the write and freed after it.
    assert_eq!(cache.resident_blocks(), 0);
    assert_eq!(cache.page_buffers(), 0);
    assert_eq!(cache.compressed_pool().stats().stored_pages, 0);
    assert!(dirty_blocks(&cache).is_empty());
    assert_eq!(disk.writes(), [Op::Write(8, 1)]);
    assert_eq!(
        disk.bytes_at(8 * CACHE_BLOCK_BYTES, CACHE_BLOCK_BYTES),
        [0x88; CACHE_BLOCK_BYTES]
    );
}

#[test]
fn red_pressure_on_a_full_pool_holds_no_more_pages_than_one_request_carries() {
    // Four segments a request: write-back runs of four blocks.
    let mut state = MemoryDisk::build("images", CACHE_BLOCK_BYTES, 32);
    state.max_segments = 4;
    let disk = MemoryDisk::from(state);
    let cache = cache_over(&disk, 32, 1);
    let view = cache.disk(0);

    block_on(async {
        for block in 0..12_u64 {
            view.overwrite(block)
                .await
                .expect("the block")
                .fill(0x40 | block as u8);
        }
    });
    compress_cold_blocks(&cache);
    assert_eq!(cache.compressed_blocks(), 12);
    assert_eq!(cache.page_buffers(), 0);
    // The pool is full: nothing more fits until it gives pages back.
    let pool = cache.compressed_pool();
    pool.set_limit_bytes(pool.stats().stored_bytes);

    let peak = Arc::new(AtomicUsize::new(0));
    {
        let cache = cache.clone();
        let peak = Arc::clone(&peak);
        *disk.state.on_write.lock() = Some(Box::new(move || {
            peak.fetch_max(cache.page_buffers(), Ordering::SeqCst);
        }));
    }
    shrink(&cache, PressureLevel::Red);
    // The probe holds a clone of the cache; let it go.
    disk.state.on_write.lock().take();

    assert!(
        peak.load(Ordering::SeqCst) <= 4,
        "red pressure held {} page buffers at once",
        peak.load(Ordering::SeqCst)
    );
    assert_eq!(cache.page_buffers(), 0);
    assert_eq!(cache.resident_blocks(), 0);
    let written: usize = disk
        .writes()
        .iter()
        .map(|op| match op {
            Op::Write(_, blocks) => *blocks,
            _ => 0,
        })
        .sum();
    assert_eq!(written, 12);
    for block in 0..12 {
        assert_eq!(
            disk.bytes_at(block * CACHE_BLOCK_BYTES, 4),
            [0x40 | block as u8; 4]
        );
    }
}

/// Compresses the one block the cache holds, then makes the pool full.
fn fill_pool_with(cache: &BlockCache<MemoryDisk>) {
    compress_cold_blocks(cache);
    assert_eq!(cache.compressed_blocks(), 1);
    let pool = cache.compressed_pool();
    pool.set_limit_bytes(pool.stats().stored_bytes);
}

/// Makes `block` cold on its own while the pool is full, and offers it
/// to the pool.
fn offer_to_full_pool(cache: &BlockCache<MemoryDisk>, block: u64) {
    cache.set_pressure(PressureLevel::Green);
    block_on(async {
        drop(cache.disk(0).read(block).await.expect("the block reads"));
    });
    compress_cold_blocks(cache);
}

#[test]
fn a_full_pool_writes_its_coldest_dirty_page_back_to_take_a_new_one() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);
    block_on(async {
        view.overwrite(9).await.expect("block 9").fill(0x99);
    });
    fill_pool_with(&cache);

    offer_to_full_pool(&cache, 4);

    // The dirty page left the pool through the disk, uncompressed, and
    // the new block took its place.
    let pool = cache.compressed_pool();
    assert!(pool.stats().limit_hits >= 1);
    assert_eq!(cache.compressed_blocks(), 1);
    assert_eq!(disk.writes(), [Op::Write(9, 1)]);
    assert_eq!(
        disk.bytes_at(9 * CACHE_BLOCK_BYTES, CACHE_BLOCK_BYTES),
        [0x99; CACHE_BLOCK_BYTES]
    );
    assert!(dirty_blocks(&cache).is_empty());
    let reads = disk.reads();
    block_on(async {
        let block = view.read(4).await.expect("block 4 from the pool");
        assert!(block.iter().all(|byte| *byte == 4));
    });
    assert_eq!(disk.reads(), reads);
}

#[test]
fn a_full_pool_drops_its_coldest_clean_page_to_take_a_new_one() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);
    block_on(async {
        drop(view.read(1).await.expect("block 1"));
    });
    fill_pool_with(&cache);

    offer_to_full_pool(&cache, 4);

    assert_eq!(cache.compressed_blocks(), 1);
    assert!(disk.writes().is_empty());
    // Block 1 left the cache; block 4 is the pool's now.
    let reads = disk.reads();
    block_on(async {
        drop(view.read(4).await.expect("block 4 from the pool"));
    });
    assert_eq!(disk.reads(), reads);
    block_on(async {
        drop(view.read(1).await.expect("block 1 from the disk"));
    });
    assert_eq!(disk.reads(), reads + 1);
}

#[test]
fn sync_writes_a_dirty_block_held_compressed_uncompressed() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    block_on(async {
        view.overwrite(5).await.expect("block 5").fill(0x55);
    });
    compress_cold_blocks(&cache);
    assert_eq!(cache.compressed_blocks(), 1);
    assert_eq!(disk.log(), []);

    block_on(async {
        view.sync().await.expect("sync");
    });

    assert_eq!(disk.log(), [Op::Write(5, 1), Op::Flush]);
    assert_eq!(
        disk.bytes_at(5 * CACHE_BLOCK_BYTES, CACHE_BLOCK_BYTES),
        [0x55; CACHE_BLOCK_BYTES]
    );
    // The block stays compressed, now clean, and the page the write was
    // staged in is gone.
    assert!(dirty_blocks(&cache).is_empty());
    assert_eq!(cache.compressed_blocks(), 1);
    assert_eq!(cache.page_buffers(), 0);
}

#[test]
fn an_incompressible_block_skips_the_compressed_tier() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    disk.state.bytes.lock()[4 * CACHE_BLOCK_BYTES..5 * CACHE_BLOCK_BYTES]
        .copy_from_slice(&noise(4));
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    block_on(async {
        drop(view.read(4).await.expect("block 4"));
        view.overwrite(2)
            .await
            .expect("block 2")
            .copy_from_slice(&noise(2));
    });
    compress_cold_blocks(&cache);

    // The clean block was dropped and the dirty one written to the disk
    // and dropped.
    assert_eq!(cache.compressed_blocks(), 0);
    assert_eq!(cache.compressed_pool().stats().incompressible, 2);
    assert_eq!(cache.resident_blocks(), 0);
    assert!(dirty_blocks(&cache).is_empty());
    assert_eq!(disk.writes(), [Op::Write(2, 1)]);
    assert_eq!(
        disk.bytes_at(2 * CACHE_BLOCK_BYTES, CACHE_BLOCK_BYTES),
        noise(2)
    );
}

#[test]
fn an_overwrite_starts_zeroed_whatever_the_cache_held() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    block_on(async {
        // A cached, valid block.
        drop(view.read(3).await.expect("block 3"));
        let block = view.overwrite(3).await.expect("block 3");
        assert!(block.iter().all(|byte| *byte == 0));
        drop(block);
        // A block held compressed, which the overwrite does not
        // decompress.
        drop(view.read(6).await.expect("block 6"));
    });
    cache.set_pressure(PressureLevel::Yellow);
    compress_cold_blocks(&cache);
    assert_eq!(cache.compressed_blocks(), 2);
    let loads = cache.compressed_pool().stats().loads;
    block_on(async {
        let block = view.overwrite(6).await.expect("block 6");
        assert!(block.iter().all(|byte| *byte == 0));
    });
    assert_eq!(cache.compressed_pool().stats().loads, loads);
    assert_eq!(cache.compressed_blocks(), 1);
}

#[test]
fn a_failed_commit_aborts_the_disk_and_its_commit_block_is_never_written_again() {
    let mut state = MemoryDisk::build("images", CACHE_BLOCK_BYTES, 16);
    state.fail_writes_at = Some(0);
    let disk = MemoryDisk::from(state);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    block_on(async {
        view.overwrite(5).await.expect("block 5").fill(0x55);
        assert_eq!(
            view.commit(0, |superblock| superblock[..4].copy_from_slice(b"HLIO"))
                .await,
            Err(BlockCacheError::Device(IoError::DeviceFault))
        );
        assert!(view.is_aborted());
        assert_eq!(view.write(1).await.err(), Some(BlockCacheError::Aborted));
        assert_eq!(
            view.overwrite(1).await.err(),
            Some(BlockCacheError::Aborted)
        );
        assert_eq!(view.sync().await, Err(BlockCacheError::Aborted));
        assert_eq!(view.commit(0, |_| {}).await, Err(BlockCacheError::Aborted));
    });
    shrink(&cache, PressureLevel::Red);

    // The data block went out before the commit block; the commit block
    // was tried once and never again.
    let writes = disk.writes();
    assert_eq!(writes, [Op::Write(5, 1), Op::Write(0, 1)]);
    // Reading the commit block answers what the disk holds, not the
    // commit that failed.
    cache.set_pressure(PressureLevel::Green);
    block_on(async {
        let block = view.read(0).await.expect("block 0");
        assert!(block.iter().all(|byte| *byte == 0));
    });
}

#[test]
fn a_commit_dropped_in_flight_aborts_the_disk() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);
    let mut context = Context::from_waker(Waker::noop());

    block_on(async {
        view.overwrite(5).await.expect("block 5").fill(0x55);
    });
    disk.pause();
    {
        let mut commit = pin!(view.commit(0, |superblock| superblock[0] = 1));
        assert!(commit.as_mut().poll(&mut context).is_pending());
    }
    // The commit's data write is still the device's.
    assert_eq!(disk.accepted(), 1);
    assert!(view.is_aborted());
    disk.resume();

    block_on(async {
        assert_eq!(view.sync().await, Err(BlockCacheError::Aborted));
    });
    // The data write finished on its own and left its block clean; the
    // commit block was never written.
    assert!(dirty_blocks(&cache).is_empty());
    assert_eq!(disk.writes(), [Op::Write(5, 1)]);
    assert_eq!(disk.bytes_at(5 * CACHE_BLOCK_BYTES, 4), [0x55; 4]);
    assert_eq!(idle_blocks(&cache), cache.resident_blocks());
}

/// Polls `future` up to `polls` times without waiting, and answers its
/// output if it finished; otherwise drops it where it stands.
fn poll_up_to<F: Future>(future: F, polls: u64) -> Option<F::Output> {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    for _ in 0..polls {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return Some(output);
        }
        std::thread::yield_now();
    }
    None
}

#[test]
fn a_read_dropped_in_flight_still_fills_its_block_for_the_next_reader() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);
    let mut context = Context::from_waker(Waker::noop());

    disk.pause();
    assert!(poll_up_to(view.read(7), 1).is_none());
    // The dropped miss's request is still the device's.
    assert_eq!(disk.accepted(), 1);
    // A second reader finds the entry the dropped miss inserted and parks
    // on it rather than asking the device again.
    let mut second = pin!(view.read(7));
    assert!(second.as_mut().poll(&mut context).is_pending());
    assert_eq!(disk.accepted(), 1);
    disk.resume();

    let Poll::Ready(Ok(block)) = second.as_mut().poll(&mut context) else {
        panic!("the parked reader should be answered by the dropped miss's read");
    };
    assert!(block.iter().all(|byte| *byte == 7));
    drop(block);
    block_on(async {
        let block = view.read(7).await.expect("block 7 is cached");
        assert!(block.iter().all(|byte| *byte == 7));
    });

    assert_eq!(disk.log(), [Op::Read(7, 1)]);
    assert_eq!(view.stats().device_reads, 1);
    assert_eq!(idle_blocks(&cache), cache.resident_blocks());
}

#[test]
fn a_write_back_dropped_in_flight_leaves_its_block_clean_and_the_disk_open() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    block_on(async {
        view.overwrite(5).await.expect("block 5").fill(0x55);
    });
    disk.pause();
    assert!(poll_up_to(view.sync(), 1).is_none());
    assert_eq!(disk.accepted(), 1);
    assert!(disk.writes().is_empty());
    disk.resume();

    assert!(!view.is_aborted());
    assert!(dirty_blocks(&cache).is_empty());
    assert_eq!(disk.writes(), [Op::Write(5, 1)]);
    assert_eq!(
        disk.bytes_at(5 * CACHE_BLOCK_BYTES, CACHE_BLOCK_BYTES),
        [0x55; CACHE_BLOCK_BYTES]
    );
    assert_eq!(view.stats().write_errors, 0);
    // Nothing is left to write: the next sync only flushes.
    block_on(view.sync()).expect("a later sync succeeds");
    assert_eq!(disk.writes(), [Op::Write(5, 1)]);
    assert_eq!(idle_blocks(&cache), cache.resident_blocks());
}

#[test]
fn a_failed_write_back_dropped_in_flight_stays_dirty_with_its_error_recorded() {
    let mut state = MemoryDisk::build("images", CACHE_BLOCK_BYTES, 16);
    state.fail_writes_at = Some(5);
    let disk = MemoryDisk::from(state);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    block_on(async {
        view.overwrite(5).await.expect("block 5").fill(0x55);
    });
    disk.pause();
    assert!(poll_up_to(view.sync(), 1).is_none());
    assert_eq!(disk.accepted(), 1);
    disk.resume();

    assert!(!view.is_aborted());
    assert_eq!(dirty_blocks(&cache), [5]);
    assert_eq!(view.stats().write_errors, 1);
    // The next sync writes the block again and reports the device's
    // answer; the disk stays open.
    assert_eq!(
        block_on(view.sync()),
        Err(BlockCacheError::Device(IoError::DeviceFault))
    );
    assert!(!view.is_aborted());
    assert_eq!(dirty_blocks(&cache), [5]);
    assert_eq!(idle_blocks(&cache), cache.resident_blocks());
}

#[test]
fn a_sync_dropped_part_way_leaves_every_block_it_did_not_write_for_the_next_sync() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 32);
    let cache = cache_over(&disk, 32, 1);
    let view = cache.disk(0);
    // Ten blocks with a gap between each: ten runs, more than one sync
    // keeps in flight, so the sync parks with blocks it has not started.
    let blocks: Vec<u64> = (0..10).map(|index| index * 2).collect();
    block_on(async {
        for &block in &blocks {
            view.overwrite(block)
                .await
                .expect("the block")
                .fill(0x80 | block as u8);
        }
    });

    disk.pause();
    assert!(poll_up_to(view.sync(), 1).is_none());
    assert!(disk.accepted() < blocks.len());
    disk.resume();
    block_on(view.sync()).expect("the next sync succeeds");

    assert!(dirty_blocks(&cache).is_empty());
    for &block in &blocks {
        assert_eq!(
            disk.bytes_at(block as usize * CACHE_BLOCK_BYTES, 4),
            [0x80 | block as u8; 4],
            "block {block} reached the disk"
        );
    }
}

#[test]
fn dropped_requests_leave_no_entry_or_page_behind() {
    const BLOCKS: u64 = 64;
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, BLOCKS as usize);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    for round in 0..300_u64 {
        let block = round * 7 % BLOCKS;
        // Dropped after no poll, after the one that hands the request to
        // the device, or after a second one while the device holds it. A
        // miss in a full cache may itself be writing a dirty block back
        // when it is dropped.
        disk.pause();
        let _ = poll_up_to(view.read(block), round % 3);
        disk.resume();
        block_on(async {
            view.overwrite((block + 1) % BLOCKS)
                .await
                .expect("the block")
                .fill(round as u8);
        });
        disk.pause();
        let _ = poll_up_to(view.sync(), (round / 3) % 3);
        disk.resume();
    }

    assert_eq!(disk.accepted(), 0);
    assert!(!view.is_aborted());
    assert_eq!(
        idle_blocks(&cache),
        cache.resident_blocks(),
        "every resident entry is free of requests, pins and locks"
    );
    assert_eq!(
        cache.page_buffers(),
        cache.resident_blocks() - cache.compressed_blocks()
    );
    block_on(view.sync()).expect("sync");
    assert!(dirty_blocks(&cache).is_empty());
    shrink(&cache, PressureLevel::Red);
    assert_eq!(cache.resident_blocks(), 0);
    assert_eq!(cache.page_buffers(), 0);
}

#[test]
fn a_dirty_block_that_goes_idle_is_written_back_for_a_waiting_miss() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 16);
    let cache = cache_over(&disk, 2, 1);
    let view = cache.disk(0);
    let mut context = Context::from_waker(Waker::noop());

    block_on(async {
        let mut first = view.overwrite(0).await.expect("block 0");
        let mut second = view.overwrite(1).await.expect("block 1");
        first.fill(0xa0);
        second.fill(0xa1);
        // Both entries are held, so a miss has nothing to recycle.
        let mut miss = pin!(view.read(5));
        assert!(miss.as_mut().poll(&mut context).is_pending());
        // The writer lets block 1 go dirty; the waiting miss writes it
        // back itself and takes its entry.
        drop(second);
        let Poll::Ready(Ok(block)) = miss.as_mut().poll(&mut context) else {
            panic!("the miss should have recycled the written-back entry");
        };
        assert!(block.iter().all(|byte| *byte == 5));
        drop(first);
    });

    assert_eq!(disk.writes(), [Op::Write(1, 1)]);
    assert_eq!(disk.bytes_at(CACHE_BLOCK_BYTES, 4), [0xa1; 4]);
}

#[test]
fn a_read_only_disk_refuses_writes_before_reaching_the_device() {
    let mut state = MemoryDisk::build("images", CACHE_BLOCK_BYTES, 16);
    state.read_only = true;
    let disk = MemoryDisk::from(state);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    block_on(async {
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
    assert!(!view.is_aborted());
}

#[test]
fn a_failed_read_is_reported_and_a_later_read_retries() {
    let mut state = MemoryDisk::build("images", CACHE_BLOCK_BYTES, 16);
    state.fail_reads = true;
    let disk = MemoryDisk::from(state);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);

    block_on(async {
        assert!(matches!(
            view.read(3).await,
            Err(BlockCacheError::Device(IoError::DeviceFault))
        ));
        assert!(view.read(3).await.is_err());
    });

    assert_eq!(disk.log(), [Op::Read(3, 1), Op::Read(3, 1)]);
    assert_eq!(view.stats().read_errors, 2);
}

#[test]
fn reads_past_the_end_are_refused() {
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, 4);
    let cache = cache_over(&disk, 16, 1);
    let view = cache.disk(0);
    let mut bytes = [0_u8; 8];

    block_on(async {
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

/// What a stress block holds after `version` writes: its number, then
/// the version, everywhere.
fn stress_pattern(block: u64, version: u8) -> [u8; CACHE_BLOCK_BYTES] {
    let mut bytes = [version; CACHE_BLOCK_BYTES];
    bytes[..8].copy_from_slice(&block.to_le_bytes());
    bytes
}

/// Whether `bytes` are some version of `block`, written whole.
fn is_stress_pattern(block: u64, bytes: &[u8; CACHE_BLOCK_BYTES]) -> bool {
    bytes[..8] == block.to_le_bytes() && bytes[8..].iter().all(|byte| *byte == bytes[8])
}

/// Whether a stress task drops its next request part-way, and after how
/// many polls: about one request in four, after one to four polls.
fn abandon_after(seed: u64) -> Option<u64> {
    (seed & 3 == 0).then_some(1 + (seed >> 40) % 4)
}

/// Runs `future` to its end, or drops it after `abandon` polls.
fn run_or_abandon<F: Future>(future: F, abandon: Option<u64>) -> Option<F::Output> {
    match abandon {
        Some(polls) => poll_up_to(future, polls),
        None => Some(block_on(future)),
    }
}

/// Real threads, one processor each, against a cache far smaller than the
/// disk: readers check every block they get is whole and the right one,
/// writers own disjoint blocks and rewrite them whole, and a reclaimer
/// compresses, decompresses, evicts and syncs under them. Misses
/// coalesce, the clock evicts against hits, and blocks move between
/// tiers while readers and writers want them. A device thread answers
/// every request on its own processor, and every task drops some of its
/// requests part-way, so answers keep landing for submitters that are
/// gone. The disk ends up holding each writer's last version, and
/// nothing is left held.
#[test]
fn the_cache_stays_coherent_under_real_threads() {
    const BLOCKS: u64 = 48;
    const WRITERS: u64 = 3;
    const READERS: u16 = 3;
    const ROUNDS: usize = 300;
    let disk = MemoryDisk::with_blocks("images", CACHE_BLOCK_BYTES, BLOCKS as usize);
    {
        let mut bytes = disk.state.bytes.lock();
        for block in 0..BLOCKS {
            let start = block as usize * CACHE_BLOCK_BYTES;
            bytes[start..start + CACHE_BLOCK_BYTES].copy_from_slice(&stress_pattern(block, 0));
        }
    }
    let reclaimer = WRITERS as u16 + READERS;
    let device = reclaimer + 1;
    let processors = usize::from(device) + 1;
    let cache = cache_over(&disk, 12, processors);
    let done = Arc::new(AtomicBool::new(false));
    let unplugged = Arc::new(AtomicBool::new(false));
    disk.pause();

    std::thread::scope(|scope| {
        {
            let disk = disk.clone();
            let unplugged = Arc::clone(&unplugged);
            scope.spawn(move || {
                on_processor(device);
                while !unplugged.load(Ordering::SeqCst) {
                    disk.answer_accepted();
                    std::thread::yield_now();
                }
            });
        }
        let mut writers = Vec::new();
        for writer in 0..WRITERS {
            let view = cache.disk(0);
            writers.push(scope.spawn(move || {
                on_processor(writer as u16);
                let mut versions = [0_u8; BLOCKS as usize];
                let mut seed = 0x9e37_79b9_u64 ^ writer;
                for round in 0..ROUNDS {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let block = (seed >> 33) % (BLOCKS / WRITERS) * WRITERS + writer;
                    let version = versions[block as usize].wrapping_add(1).max(1);
                    let abandon = abandon_after(seed);
                    let guard = if round % 2 == 0 {
                        run_or_abandon(view.overwrite(block), abandon)
                            .map(|guard| guard.expect("overwrite"))
                    } else {
                        run_or_abandon(view.write(block), abandon).map(|guard| {
                            let guard = guard.expect("write");
                            assert!(
                                is_stress_pattern(block, &guard),
                                "write of {block} saw torn bytes"
                            );
                            guard
                        })
                    };
                    // A write dropped before it had the block changed
                    // nothing.
                    if let Some(mut guard) = guard {
                        guard.copy_from_slice(&stress_pattern(block, version));
                        versions[block as usize] = version;
                    }
                }
                versions
            }));
        }
        let mut others = Vec::new();
        for reader in 0..READERS {
            let view = cache.disk(0);
            let done = Arc::clone(&done);
            others.push(scope.spawn(move || {
                on_processor(WRITERS as u16 + reader);
                let mut seed = 0x51ed_270b_u64 + u64::from(reader);
                while !done.load(Ordering::SeqCst) {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let block = (seed >> 33) % BLOCKS;
                    if let Some(bytes) = run_or_abandon(view.read(block), abandon_after(seed)) {
                        let bytes = bytes.expect("read");
                        assert!(
                            is_stress_pattern(block, &bytes),
                            "read of {block} saw torn or foreign bytes"
                        );
                    }
                }
            }));
        }
        {
            let cache = cache.clone();
            let done = Arc::clone(&done);
            others.push(scope.spawn(move || {
                on_processor(reclaimer);
                let view = cache.disk(0);
                let mut pass = 0_u64;
                let mut seed = 0x2545_f491_u64;
                while !done.load(Ordering::SeqCst) {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let level = match pass % 4 {
                        0 | 1 => PressureLevel::Yellow,
                        2 => PressureLevel::Red,
                        _ => PressureLevel::Green,
                    };
                    cache.set_pressure(level);
                    run_or_abandon(cache.shrink(level), abandon_after(seed));
                    if pass.is_multiple_of(3)
                        && let Some(synced) = run_or_abandon(view.sync(), abandon_after(seed >> 2))
                    {
                        synced.expect("sync");
                    }
                    pass += 1;
                }
                cache.set_pressure(PressureLevel::Green);
            }));
        }
        let versions: Vec<[u8; BLOCKS as usize]> = writers
            .into_iter()
            .map(|writer| writer.join().expect("writer"))
            .collect();
        done.store(true, Ordering::SeqCst);
        for other in others {
            other.join().expect("reader or reclaimer");
        }
        unplugged.store(true, Ordering::SeqCst);
        // Whatever the device thread left is answered here, and from now
        // on every request is answered as it arrives.
        disk.resume();

        on_processor(0);
        assert!(!cache.disk(0).is_aborted());
        block_on(cache.disk(0).sync()).expect("final sync");
        for (writer, versions) in versions.iter().enumerate() {
            for block in (writer as u64..BLOCKS).step_by(WRITERS as usize) {
                let version = versions[block as usize];
                let expected = stress_pattern(block, version);
                assert_eq!(
                    disk.bytes_at(block as usize * CACHE_BLOCK_BYTES, CACHE_BLOCK_BYTES),
                    expected,
                    "block {block} should hold version {version}"
                );
            }
        }
    });
    assert!(dirty_blocks(&cache).is_empty());
    assert_eq!(disk.accepted(), 0);
    assert_eq!(cache.disk(0).stats().write_errors, 0);
    assert_eq!(
        idle_blocks(&cache),
        cache.resident_blocks(),
        "every resident entry is free of requests, pins and locks"
    );
    assert_eq!(
        cache.page_buffers(),
        cache.resident_blocks() - cache.compressed_blocks()
    );
}
