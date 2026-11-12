use super::*;

use crate::testing::{
    Allocation, CONFINED_ARENA_BYTES, ConfinedDmaBuffer, ConfinedDmaPool, ConfinedTransport,
    FakeAffinity, FakeTransport, FakeTransportConfig, Lifetime,
};
use crate::transport::VirtioFeatures;
use alloc::sync::Arc;
use alloc::vec;
use core::pin::pin;
use core::ptr::NonNull;
use futures_lite::future::{block_on, poll_once};
use helios_hal::dma::{DmaBuffer, DmaError};
use helios_hal::iommu::DmaAccess;
use spin::Mutex as SpinMutex;

/// A device with 2 MiB of capacity in 512-byte sectors.
const TEST_CAPACITY_SECTORS: u32 = 4096;

/// One thing the driver did to a [`TestBuffer`].
#[derive(Clone, Debug, PartialEq, Eq)]
enum Heard {
    /// Asked for a write's bytes, with the state of the ring and the
    /// count of kicks at that moment.
    Filled { in_flight: usize, kicks: usize },
    /// Handed a read's bytes.
    Delivered(Vec<u8>),
    /// Told the request's outcome.
    Completed(IoResult<()>),
}

/// What a [`TestBuffer`] heard from the driver, in order.
#[derive(Default)]
struct Probe {
    heard: SpinMutex<Vec<Heard>>,
    drops: AtomicUsize,
}

impl Probe {
    fn heard(&self) -> Vec<Heard> {
        self.heard.lock().clone()
    }

    fn completions(&self) -> Vec<IoResult<()>> {
        self.heard()
            .into_iter()
            .filter_map(|heard| match heard {
                Heard::Completed(result) => Some(result),
                _ => None,
            })
            .collect()
    }

    fn drops(&self) -> usize {
        self.drops.load(Ordering::SeqCst)
    }
}

/// A request buffer that reports what the driver did with it.
///
/// Its bytes are a device's DMA memory, the memory a block request has
/// to carry, or host memory no device reaches: the one a confined device
/// refuses.
struct TestBuffer {
    bytes: NonNull<[u8]>,
    /// The device memory the bytes are, released when the buffer drops;
    /// `None` for host memory, which the buffer frees itself.
    device_memory: Option<ConfinedDmaBuffer>,
    probe: Option<Arc<Probe>>,
    answered: Option<AnswerFlag>,
    /// Runs inside [`BlockBuffer::fill`], where the test looks at the
    /// device from the point of view of the driver's copy.
    on_fill: Option<Box<dyn Fn() -> Heard + Send>>,
}

// SAFETY: the buffer owns its bytes, either as a boxed slice or as the
// confined pool's buffer it holds.
unsafe impl Send for TestBuffer {}
// SAFETY: a shared buffer only reads its bytes.
unsafe impl Sync for TestBuffer {}

impl TestBuffer {
    /// `len` zero bytes of `device`'s DMA memory.
    fn new(device: &TestDevice, len: usize) -> Self {
        Self::in_pool(device.object().transport.pool(), len)
    }

    /// `len` zero bytes of `pool`'s memory.
    fn in_pool(pool: &ConfinedDmaPool, len: usize) -> Self {
        Self::owning(pool.buffer(len))
    }

    /// `len` zero bytes of host memory.
    fn host(len: usize) -> Self {
        let bytes = Box::into_raw(vec![0_u8; len].into_boxed_slice());
        Self {
            bytes: NonNull::new(bytes).expect("a box is never null"),
            device_memory: None,
            probe: None,
            answered: None,
            on_fill: None,
        }
    }

    /// `bytes` as host memory.
    fn host_filled(bytes: &[u8]) -> Self {
        let mut buffer = Self::host(bytes.len());
        buffer.as_mut().copy_from_slice(bytes);
        buffer
    }

    /// Has the buffer record what `look` answers when the driver asks it
    /// for a write's bytes.
    fn looking_when_filled(mut self, look: impl Fn() -> Heard + Send + 'static) -> Self {
        self.on_fill = Some(Box::new(look));
        self
    }

    fn filled(device: &TestDevice, bytes: &[u8]) -> Self {
        let mut buffer = Self::new(device, bytes.len());
        buffer.as_mut().copy_from_slice(bytes);
        buffer
    }

    fn probed(device: &TestDevice, len: usize, probe: &Arc<Probe>) -> Self {
        Self::new(device, len).with_probe(probe)
    }

    fn with_probe(mut self, probe: &Arc<Probe>) -> Self {
        self.probe = Some(Arc::clone(probe));
        self
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the buffer owns its bytes, and no request holds it.
        unsafe { self.bytes.as_ref() }
    }

    /// The address the device issues for the buffer's first byte, which a
    /// confined test device makes the host address.
    fn device_address(&self) -> u64 {
        self.bytes.cast::<u8>().as_ptr() as u64
    }
}

impl Drop for TestBuffer {
    fn drop(&mut self) {
        if self.device_memory.is_none() {
            // SAFETY: a buffer without device memory came from
            // `Box::into_raw` in `host` and is freed once, here.
            drop(unsafe { Box::from_raw(self.bytes.as_ptr()) });
        }
        if let Some(probe) = &self.probe {
            probe.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl BlockBuffer for TestBuffer {
    fn len(&self) -> usize {
        self.bytes().len()
    }

    fn fill(&mut self, to: &mut [u8]) {
        if let (Some(probe), Some(look)) = (&self.probe, &self.on_fill) {
            probe.heard.lock().push(look());
        }
        to.copy_from_slice(self.bytes());
    }

    fn deliver(&mut self, from: &[u8]) {
        if let Some(probe) = &self.probe {
            probe.heard.lock().push(Heard::Delivered(from.to_vec()));
        }
        self.as_mut().copy_from_slice(from);
    }

    fn complete(&mut self, result: IoResult<()>) {
        if let Some(probe) = &self.probe {
            probe.heard.lock().push(Heard::Completed(result));
        }
        if let Some(answered) = self.answered.take() {
            answered.raise();
        }
    }
}

impl OwnedBlockBuffer for TestBuffer {
    type Memory = ConfinedDmaBuffer;

    fn raise_on_answer(&mut self, flag: AnswerFlag) {
        self.answered = Some(flag);
    }

    fn owning(mut memory: ConfinedDmaBuffer) -> Self {
        Self {
            bytes: NonNull::from(memory.as_mut_slice()),
            device_memory: Some(memory),
            probe: None,
            answered: None,
            on_fill: None,
        }
    }
}

impl AsRef<[u8]> for TestBuffer {
    fn as_ref(&self) -> &[u8] {
        self.bytes()
    }
}

impl AsMut<[u8]> for TestBuffer {
    fn as_mut(&mut self) -> &mut [u8] {
        // SAFETY: the buffer owns its bytes, and no request holds it.
        unsafe { self.bytes.as_mut() }
    }
}

/// A device whose DMA reaches only the memory its pool handed out: its
/// rings, its control slots and the buffers [`TestBuffer::new`] takes.
type TestDevice = VirtioBlockResource<ConfinedTransport, FakeAffinity<1>, TestBuffer>;

fn device_with(features: u64, setup: impl FnOnce(&ConfinedTransport)) -> TestDevice {
    let transport = FakeTransport::confined(FakeTransportConfig {
        device_type: DeviceType::Block,
        offered_features: VirtioFeatures::VERSION_1.bits() | features,
        queue_size: 8,
        supports_queue_reset: false,
        absent_queues: &[],
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

/// The first queue's ring, as the device side of a test plays it.
fn ring(device: &TestDevice) -> MutexGuard<'_, VirtQueue<ConfinedTransport>> {
    device.object().queues[0]
        .ring
        .try_lock()
        .expect("a parked driver does not hold the ring lock")
}

/// The identifier the next submission on the first queue will take.
///
/// A chain spends one descriptor per buffer, so the identifiers a
/// driver's requests get depend on the shape of the chains before them;
/// tests ask the ring rather than assuming a numbering.
fn next_token(device: &TestDevice) -> u16 {
    ring(device).next_free_descriptor()
}

/// The bytes the driver made readable in chain `token`.
fn request_of(device: &TestDevice, token: u16) -> Vec<u8> {
    ring(device).device_request(token)
}

/// Plays the device: writes `response` into the chain's writable
/// buffers and publishes the used entry, without raising the interrupt.
fn answer(device: &TestDevice, token: u16, response: &[u8]) {
    let ring = ring(device);
    let len = ring.device_respond(token, response);
    ring.device_complete(token, len);
}

/// Plays the device: answers chain `token` and raises the interrupt.
fn complete(device: &TestDevice, token: u16, response: &[u8]) {
    answer(device, token, response);
    device.handle_interrupt();
}

fn header(request: ReqType, sector: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(request as u32).to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&sector.to_le_bytes());
    bytes
}

/// A read's response: the payload, then a successful status byte.
fn read_response(byte: u8, len: usize) -> Vec<u8> {
    let mut response = vec![byte; len];
    response.push(0);
    response
}

/// The answer a finished data request gave, and the buffer it handed
/// back, which a block request does whatever the answer.
fn answered(done: BufResult<(), TestBuffer>) -> (IoResult<()>, TestBuffer) {
    match done {
        Ok(((), buffer)) => (Ok(()), buffer),
        Err(failure) => (
            Err(failure.error),
            failure
                .buffers
                .expect("a block request hands its buffer back"),
        ),
    }
}

#[test]
fn a_wrong_device_type_is_rejected() {
    let rejected = VirtioBlockDevice::<_, _, TestBuffer>::new(
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
            // The device states no limit, so its one segment would carry
            // `u32::MAX` bytes in whole sectors; the pool's run is the
            // shorter, and is what a request is bounded by.
            max_transfer_bytes: CONFINED_ARENA_BYTES,
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
            // Four segments of 16 KiB.
            max_transfer_bytes: 4 * 16384,
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

    let mut read = pin!(device.read(3, TestBuffer::new(&device, 4096)));
    assert!(block_on(poll_once(read.as_mut())).is_none());

    // Block 3 of a 4 KiB device is sector 24: virtio-blk addresses
    // every request in 512-byte sectors whatever the block size is.
    assert_eq!(request_of(&device, 0), header(ReqType::In, 24));
}

#[test]
fn a_read_carries_the_payload_and_checks_the_used_length() {
    let device = device();
    let probe = Arc::new(Probe::default());
    let mut read = pin!(device.read(2, TestBuffer::probed(&device, 512, &probe)));

    assert!(block_on(poll_once(read.as_mut())).is_none());
    assert_eq!(request_of(&device, 0), header(ReqType::In, 2));

    complete(&device, 0, &read_response(0xa5, 512));
    let (result, buffer) =
        answered(block_on(poll_once(read.as_mut())).expect("the read has completed"));
    assert_eq!(result, Ok(()));
    assert_eq!(buffer.bytes(), [0xa5_u8; 512]);
    assert_eq!(probe.completions(), [Ok(())]);
    assert_eq!(device.request_counts().reads, 1);
}

#[test]
fn a_short_read_is_a_device_fault() {
    let device = device();
    let probe = Arc::new(Probe::default());
    let mut read = pin!(device.read(0, TestBuffer::probed(&device, 1024, &probe)));

    assert!(block_on(poll_once(read.as_mut())).is_none());
    // The device claims success but only filled half the chain.
    {
        let ring = ring(&device);
        ring.device_respond(0, &vec![0_u8; 1025]);
        ring.device_complete(0, 513);
    }
    device.handle_interrupt();

    let (result, _buffer) =
        answered(block_on(poll_once(read.as_mut())).expect("the read has completed"));
    assert_eq!(result, Err(IoError::DeviceFault));
    assert_eq!(probe.completions(), [Err(IoError::DeviceFault)]);
}

#[test]
fn a_write_puts_the_payload_after_the_header() {
    let device = device();
    let payload = [0x5a_u8; 512];
    let mut write = pin!(device.write(7, TestBuffer::filled(&device, &payload)));

    assert!(block_on(poll_once(write.as_mut())).is_none());
    let mut expected = header(ReqType::Out, 7);
    expected.extend_from_slice(&payload);
    assert_eq!(request_of(&device, 0), expected);

    complete(&device, 0, &[0]);
    let (result, _buffer) =
        answered(block_on(poll_once(write.as_mut())).expect("the write has completed"));
    assert_eq!(result, Ok(()));
    assert_eq!(device.request_counts().writes, 1);
}

#[test]
fn a_device_error_status_becomes_a_device_fault() {
    let device = device();
    let mut write = pin!(device.write(0, TestBuffer::new(&device, 512)));

    assert!(block_on(poll_once(write.as_mut())).is_none());
    complete(&device, 0, &[1]);
    let (result, _buffer) =
        answered(block_on(poll_once(write.as_mut())).expect("the write has completed"));
    assert_eq!(result, Err(IoError::DeviceFault));
}

#[test]
fn an_answer_without_a_status_byte_is_a_device_fault() {
    let device = device();
    let token = next_token(&device);
    let mut write = pin!(device.write(0, TestBuffer::new(&device, 512)));

    assert!(block_on(poll_once(write.as_mut())).is_none());
    // The device publishes the chain as answered without writing the
    // status byte. The control slot was zeroed when it was carved, and a
    // zero status is success, so only a slot primed before the chain was
    // published tells the two apart.
    ring(&device).device_complete(token, 1);
    device.handle_interrupt();

    let (result, _buffer) =
        answered(block_on(poll_once(write.as_mut())).expect("the write has completed"));
    assert_eq!(result, Err(IoError::DeviceFault));
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
        let ring = ring(&device);
        let mut response = [0_u8; BLOCK_ID_BYTES + 1];
        response[BLOCK_ID_BYTES] = 2;
        ring.device_respond(token, &response);
        ring.device_complete(token, 1);
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
fn a_dropped_deallocation_is_dropped_whole_not_half_written() {
    let device = device_with(BLK_F_DISCARD, |transport| {
        transport.set_config_u32(CFG_MAX_DISCARD_SECTORS, 8);
        transport.set_config_u32(CFG_MAX_DISCARD_SEG, 4);
    });
    {
        let mut discard = pin!(device.discard(BlockRange::new(16, 12)));
        assert!(block_on(poll_once(discard.as_mut())).is_none());
    }
    // The segment list is in the control slot the device still owns, so
    // the request is still intact on the wire after its future is gone.
    let mut expected = header(ReqType::Discard, 0);
    expected.extend_from_slice(&16_u64.to_le_bytes());
    expected.extend_from_slice(&8_u32.to_le_bytes());
    expected.extend_from_slice(&0_u32.to_le_bytes());
    expected.extend_from_slice(&24_u64.to_le_bytes());
    expected.extend_from_slice(&4_u32.to_le_bytes());
    expected.extend_from_slice(&0_u32.to_le_bytes());
    assert_eq!(request_of(&device, 0), expected);
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
fn a_buffer_is_cut_at_the_segment_size_and_refused_past_one_request() {
    let device = device_with(BLK_F_SEG_MAX | BLK_F_SIZE_MAX, |transport| {
        transport.set_config_u32(CFG_SEG_MAX, 2);
        transport.set_config_u32(CFG_SIZE_MAX, 1024);
    });
    let payload = [0x11_u8; 2048];
    let mut write = pin!(device.write(0, TestBuffer::filled(&device, &payload)));

    assert!(block_on(poll_once(write.as_mut())).is_none());
    // Two segments of 1024 bytes each carry the 2048 bytes in one
    // request.
    let mut expected = header(ReqType::Out, 0);
    expected.extend_from_slice(&payload);
    assert_eq!(request_of(&device, 0), expected);
    assert_eq!(
        ring(&device)
            .device_chain(0)
            .iter()
            .filter(|(_, len, writable)| !writable && *len == 1024)
            .count(),
        2,
        "the payload travels as two device segments"
    );
    complete(&device, 0, &[0]);
    let (result, _buffer) =
        answered(block_on(poll_once(write.as_mut())).expect("the write has completed"));
    assert_eq!(result, Ok(()));

    // Twice that is longer than the 2048 bytes one request carries:
    // refused before any memory is taken, with the buffer told why, and
    // nothing reaches the device.
    assert_eq!(device.geometry().max_transfer_bytes, 2048);
    let pool = device.object().transport.pool().clone();
    let (kicks, allocations) = (
        device.object().transport.kick_count(),
        pool.allocations().len(),
    );
    let probe = Arc::new(Probe::default());
    let (result, buffer) = answered(block_on(
        device.write(0, TestBuffer::probed(&device, 4096, &probe)),
    ));
    assert_eq!(result, Err(IoError::OutOfBounds));
    assert_eq!(probe.completions(), [Err(IoError::OutOfBounds)]);
    assert_eq!(probe.drops(), 0, "the refused buffer comes back");
    drop(buffer);
    assert_eq!(device.object().transport.kick_count(), kicks);
    assert_eq!(
        pool.allocations().len(),
        allocations + 1,
        "only the test's own buffer was allocated; the refused request took no run"
    );
}

/// The published bound is the least of the device's, the chain's and the
/// longest run the driver's memory serves.
#[test]
fn a_request_is_bounded_by_the_longest_run_the_memory_serves() {
    let transport = FakeTransport::with_dma(
        FakeTransportConfig {
            device_type: DeviceType::Block,
            offered_features: VirtioFeatures::VERSION_1.bits() | BLK_F_SEG_MAX,
            queue_size: 8,
            supports_queue_reset: false,
            absent_queues: &[],
        },
        ConfinedDmaPool::new(256 * 1024),
    );
    transport.set_config_u32(CFG_CAPACITY_LOW, TEST_CAPACITY_SECTORS);
    transport.set_config_u32(CFG_SEG_MAX, 6);
    let memory_bytes = transport.pool().max_run_bytes();
    let device = VirtioBlockDevice::<_, _, TestBuffer>::new(transport, FakeAffinity::<1>)
        .expect("block device should initialize")
        .into_resource(BlockDeviceRights::READ | BlockDeviceRights::WRITE);

    // Six segments of nearly 4 GiB each are far more than 256 KiB.
    assert_eq!(memory_bytes, 256 * 1024);
    assert_eq!(device.geometry().max_transfer_bytes, memory_bytes);
}

/// A refusal of the run that waiting cannot clear ends the request at
/// once, with the buffer completed with the error and handed back, and
/// the device never sees it.
#[test]
fn a_run_the_memory_cannot_serve_refuses_the_request_with_its_buffer() {
    for (refusal, error) in [
        (
            DmaError::TooLarge {
                bytes: 512,
                limit: 256,
            },
            IoError::OutOfBounds,
        ),
        (
            DmaError::InvalidLength {
                bytes: 512,
                required_multiple: 1024,
            },
            IoError::InvalidBufferLength {
                required_multiple: 1024,
                actual: 512,
            },
        ),
    ] {
        let device = device();
        device
            .object()
            .transport
            .pool()
            .refuse_requests_with(refusal);
        let kicks = device.object().transport.kick_count();
        let probe = Arc::new(Probe::default());
        let (result, buffer) = answered(block_on(
            device.write(0, TestBuffer::host(512).with_probe(&probe)),
        ));
        assert_eq!(result, Err(error));
        assert_eq!(probe.heard(), [Heard::Completed(Err(error))]);
        assert_eq!(probe.drops(), 0, "the buffer comes back");
        drop(buffer);
        assert_eq!(device.object().transport.kick_count(), kicks);
    }
}

#[test]
fn the_header_and_the_status_travel_in_the_requests_own_slot() {
    let device = device();
    let buffer = TestBuffer::new(&device, 512);
    let payload = buffer.device_address();
    let mut read = pin!(device.read(0, buffer));
    assert!(block_on(poll_once(read.as_mut())).is_none());

    let chain = ring(&device).device_chain(0);
    assert_eq!(chain.len(), 3, "header, data, status");
    let slots = &device.object().queues[0].slots;
    for (address, _, _) in [chain[0], chain[2]] {
        assert!(
            slots.contains(address),
            "descriptor at {address:#x} is outside the control slots"
        );
    }
    assert_ne!(
        chain[1].0, payload,
        "the payload is the driver's own run, never the caller's buffer"
    );
    assert_eq!(request_of(&device, 0), header(ReqType::In, 0));
}

#[test]
fn request_memory_is_the_devices_own_in_whole_blocks() {
    let device = device();
    let block = device.block_size();
    let memory = device
        .memory(2 * block)
        .expect("two blocks of request memory");
    assert_eq!(memory.len(), 2 * block);
    assert!(
        (memory.as_ptr() as usize).is_multiple_of(PhysFrame::SIZE),
        "request memory starts on a page"
    );
    let buffer = TestBuffer::owning(memory);
    assert!(
        device.object().transport.pool().reaches(buffer.as_ref()),
        "request memory is memory the device reaches"
    );
    for bytes in [0, block + 1] {
        assert_eq!(
            device.memory(bytes).err(),
            Some(DmaError::InvalidLength {
                bytes,
                required_multiple: block,
            })
        );
    }
}

/// Request memory is carried by a request and given back, and a request
/// reads and writes it: a cache page is read into and written from.
#[test]
fn request_memory_is_per_request_and_read_write() {
    let device = device();
    let block = device.block_size();
    let pool = device.object().transport.pool().clone();
    let before = pool.allocations().len();
    let _memory = device.memory(block).expect("one block of request memory");
    let served = pool.allocations();
    assert_eq!(
        served[before..],
        [Allocation {
            lifetime: Lifetime::PerRequest,
            access: DmaAccess::READ | DmaAccess::WRITE,
            bytes: block,
        }]
    );
}

/// Rings and control slots are resident for the life of the device.
#[test]
fn rings_and_control_slots_are_resident() {
    let device = device();
    let served = device.object().transport.pool().allocations();
    assert!(!served.is_empty(), "bring-up allocated device memory");
    assert!(
        served
            .iter()
            .all(|allocation| allocation.lifetime == Lifetime::Resident),
        "bring-up allocates only resident memory: {served:?}"
    );
}

/// A buffer no device reaches, host memory or another device's, is a
/// buffer like any other: the driver copies through memory of its own,
/// and every data descriptor of the chain resolves inside the device's
/// pool.
#[test]
fn host_memory_round_trips_through_the_drivers_own_run() {
    let device = device();
    let pool = device.object().transport.pool().clone();
    let payload = [0x3c_u8; 1024];
    let write_buffer = TestBuffer::host_filled(&payload);
    let host_address = write_buffer.device_address();
    let mut write = pin!(device.write(5, write_buffer));
    assert!(block_on(poll_once(write.as_mut())).is_none());

    let mut expected = header(ReqType::Out, 5);
    expected.extend_from_slice(&payload);
    assert_eq!(
        request_of(&device, 0),
        expected,
        "the device reads the bytes"
    );
    let chain = ring(&device).device_chain(0);
    assert_eq!(chain.len(), 3, "header, data, status");
    for &(address, len, _) in &chain {
        assert!(
            pool.reaches_range(address, len as usize),
            "descriptor at {address:#x} is outside the device's memory"
        );
    }
    assert_ne!(chain[1].0, host_address, "the data is not the caller's");
    complete(&device, 0, &[0]);
    let (result, _buffer) =
        answered(block_on(poll_once(write.as_mut())).expect("the write has completed"));
    assert_eq!(result, Ok(()));

    // Another device's memory is as foreign as the host's.
    for foreign in [
        TestBuffer::host(1024),
        TestBuffer::in_pool(&ConfinedDmaPool::new(8192), 1024),
    ] {
        let token = next_token(&device);
        let mut read = pin!(device.read(5, foreign));
        assert!(block_on(poll_once(read.as_mut())).is_none());
        complete(&device, token, &read_response(0x7e, 1024));
        let (result, buffer) =
            answered(block_on(poll_once(read.as_mut())).expect("the read has completed"));
        assert_eq!(result, Ok(()));
        assert_eq!(buffer.bytes(), [0x7e_u8; 1024]);
    }
}

#[test]
fn requests_are_pipelined_and_completed_out_of_order() {
    let device = device();
    let first_token = next_token(&device);
    let mut read_first = pin!(device.read(0, TestBuffer::new(&device, 512)));
    assert!(block_on(poll_once(read_first.as_mut())).is_none());
    let second_token = next_token(&device);
    let mut read_second = pin!(device.read(1, TestBuffer::new(&device, 512)));
    assert!(block_on(poll_once(read_second.as_mut())).is_none());
    assert_eq!(request_of(&device, first_token), header(ReqType::In, 0));
    assert_eq!(request_of(&device, second_token), header(ReqType::In, 1));

    // The device answers the second request first. The drain routes
    // answers by slot, so the first request must not take a completion
    // that is not addressed to it.
    complete(&device, second_token, &read_response(0x22, 512));
    assert!(
        block_on(poll_once(read_first.as_mut())).is_none(),
        "the first request is still outstanding"
    );
    let (_, second) =
        answered(block_on(poll_once(read_second.as_mut())).expect("the second read is done"));
    assert_eq!(second.bytes(), [0x22_u8; 512]);

    complete(&device, first_token, &read_response(0x33, 512));
    let (_, first) =
        answered(block_on(poll_once(read_first.as_mut())).expect("the first read is done"));
    assert_eq!(first.bytes(), [0x33_u8; 512]);
}

#[test]
fn quiesce_waits_for_every_request_in_flight_and_leaves_the_answers_for_their_waiters() {
    let device = device();
    let first_token = next_token(&device);
    let mut read_first = pin!(device.read(0, TestBuffer::new(&device, 512)));
    assert!(block_on(poll_once(read_first.as_mut())).is_none());
    let second_token = next_token(&device);
    let mut read_second = pin!(device.read(1, TestBuffer::new(&device, 512)));
    assert!(block_on(poll_once(read_second.as_mut())).is_none());

    // The device answers while the quiesce spins, the later request
    // first.
    let mut answers = [(second_token, 0x22_u8), (first_token, 0x11_u8)].into_iter();
    let mut passes = 0;
    device
        .quiesce(|| {
            passes += 1;
            if let Some((token, byte)) = answers.next() {
                answer(&device, token, &read_response(byte, 512));
            }
            false
        })
        .expect("the device answers inside the deadline");
    assert_eq!(passes, 2);
    assert_eq!(ring(&device).in_flight_chains(), 0);

    let (_, first) =
        answered(block_on(poll_once(read_first.as_mut())).expect("the first read is done"));
    assert_eq!(first.bytes(), [0x11_u8; 512]);
    let (_, second) =
        answered(block_on(poll_once(read_second.as_mut())).expect("the second read is done"));
    assert_eq!(second.bytes(), [0x22_u8; 512]);
}

#[test]
fn quiesce_gives_up_when_the_deadline_passes() {
    let device = device();
    let mut read = pin!(device.read(0, TestBuffer::new(&device, 512)));
    assert!(block_on(poll_once(read.as_mut())).is_none());

    assert_eq!(
        device.quiesce(|| true),
        Err(QuiesceError::RequestsStillInFlight {
            device: DeviceType::Block,
            queue: 0,
            count: 1,
        })
    );
}

#[test]
fn a_reset_device_serves_requests_again_with_the_features_it_had() {
    let device = device_with(BLK_F_RO, |_| {});
    let mut read = pin!(device.read(0, TestBuffer::new(&device, 512)));
    assert!(block_on(poll_once(read.as_mut())).is_none());
    device
        .quiesce(|| {
            answer(&device, 0, &read_response(0x5a, 512));
            false
        })
        .expect("the device answers inside the deadline");

    device.object().transport.reset_behind_the_driver();
    assert_eq!(
        device.reinitialize(),
        Ok(DeviceReset {
            device: DeviceType::Block
        })
    );

    let (_, done) =
        answered(block_on(poll_once(read.as_mut())).expect("the answer from before the reset"));
    assert_eq!(done.bytes(), [0x5a_u8; 512]);
    let transport = &device.object().transport;
    assert_eq!(transport.driver_features(), device.features().bits());
    let programmed = transport.programmed_queues();
    assert_eq!(programmed.len(), 2);
    assert_eq!(programmed[0], programmed[1]);

    // The ring starts again from its first descriptor and works.
    assert_eq!(next_token(&device), 0);
    let mut again = pin!(device.read(1, TestBuffer::new(&device, 512)));
    assert!(block_on(poll_once(again.as_mut())).is_none());
    assert_eq!(request_of(&device, 0), header(ReqType::In, 1));
    complete(&device, 0, &read_response(0x6b, 512));
    let (_, done) =
        answered(block_on(poll_once(again.as_mut())).expect("the read after the reset"));
    assert_eq!(done.bytes(), [0x6b_u8; 512]);
}

#[test]
fn a_device_that_offers_fewer_features_after_the_reset_is_refused() {
    let device = device_with(BLK_F_RO, |_| {});
    device.object().transport.withdraw_features(BLK_F_RO);

    assert_eq!(
        device.reinitialize(),
        Err(ReinitError::Renegotiate(
            crate::features::RenegotiateError::FeatureWithdrawn {
                device: DeviceType::Block,
                bit: 5,
            }
        ))
    );
}

#[test]
fn a_disk_of_another_size_after_the_reset_is_refused() {
    let device = device();
    device
        .object()
        .transport
        .set_config_u32(CFG_CAPACITY_LOW, TEST_CAPACITY_SECTORS * 2);

    assert_eq!(
        device.reinitialize(),
        Err(ReinitError::DeviceChanged {
            device: DeviceType::Block,
            what: "capacity",
        })
    );
}

#[test]
fn a_readonly_device_refuses_writes() {
    let device = device_with(BLK_F_RO, |_| {});

    let (result, _buffer) = answered(block_on(device.write(0, TestBuffer::new(&device, 512))));
    assert_eq!(result, Err(IoError::ReadOnly));
}

/// A waiter that is gone changes nothing about a read: the driver still
/// delivers the answer into the buffer it parked, before it completes
/// and drops it, and the run the device wrote is its own to free then.
#[test]
fn a_dropped_waiter_still_has_its_read_delivered_before_the_buffer_goes() {
    let device = device();
    let pool = device.object().transport.pool().clone();
    let probe = Arc::new(Probe::default());
    let token = next_token(&device);
    {
        let mut read = pin!(device.read(4, TestBuffer::host(512).with_probe(&probe)));
        assert!(block_on(poll_once(read.as_mut())).is_none());
    }
    // The task is gone, but the device still has the request: the driver
    // keeps the buffer, and the run the device writes.
    let (run_address, run_len, _) = ring(&device).device_chain(token)[1];
    assert_eq!(probe.drops(), 0);
    assert!(probe.heard().is_empty());
    assert!(
        !pool.is_released(run_address, run_len as usize),
        "the run of a dropped read is still the device's"
    );

    // The device answers with nobody waiting, into memory that is still
    // the request's. The interrupt finishes the request: the buffer is
    // handed the bytes, hears the answer, and goes.
    {
        let ring = ring(&device);
        let written = pool.respond_into_live_memory(&ring, token, &read_response(0x44, 512));
        ring.device_complete(token, written);
    }
    device.handle_interrupt();
    assert_eq!(
        probe.heard(),
        [Heard::Delivered(vec![0x44; 512]), Heard::Completed(Ok(()))]
    );
    assert_eq!(probe.drops(), 1);
    assert!(
        pool.is_released(run_address, run_len as usize),
        "the device's answer is what frees the dropped read's run"
    );
    assert_eq!(pool.live_requests(), 0);
    assert_eq!(device.request_counts().reads, 1);

    // The slot is free again: the next request goes straight through.
    let mut read = pin!(device.read(5, TestBuffer::new(&device, 512)));
    assert!(block_on(poll_once(read.as_mut())).is_none());
    let token = next_token(&device);
    let reused = (0..8_u16)
        .find(|&id| id != token && request_of(&device, id) == header(ReqType::In, 5))
        .expect("the second read is in the ring");
    complete(&device, reused, &read_response(0x55, 512));
    let (_, buffer) =
        answered(block_on(poll_once(read.as_mut())).expect("the second read is done"));
    assert_eq!(buffer.bytes(), [0x55_u8; 512]);
}

#[test]
fn a_waiter_dropped_after_the_answer_releases_the_buffer_once() {
    let device = device();
    let probe = Arc::new(Probe::default());
    let token = next_token(&device);
    let waiter = block_on(device.submit_write(0, TestBuffer::probed(&device, 512, &probe)));

    complete(&device, token, &[0]);
    assert_eq!(probe.completions(), [Ok(())]);
    assert_eq!(probe.drops(), 0, "the answer waits for its waiter");

    drop(waiter);
    assert_eq!(probe.drops(), 1);
    assert_eq!(probe.completions(), [Ok(())], "a buffer is completed once");
}

#[test]
fn a_submission_dropped_before_the_device_took_it_drops_the_buffer_uncompleted() {
    let device = device();
    // Without indirect descriptors a read spends three of the ring's
    // eight descriptors, so two fill it.
    let first = block_on(device.submit_read(0, TestBuffer::new(&device, 512)));
    let second = block_on(device.submit_read(1, TestBuffer::new(&device, 512)));

    let probe = Arc::new(Probe::default());
    {
        let mut third = pin!(device.submit_read(2, TestBuffer::probed(&device, 512, &probe)));
        assert!(
            block_on(poll_once(third.as_mut())).is_none(),
            "the ring has no room for a third chain"
        );
    }
    assert_eq!(probe.drops(), 1);
    assert!(
        probe.completions().is_empty(),
        "the device never saw the request, so there is no answer to hear"
    );
    drop((first, second));
}

#[test]
fn an_answer_the_interrupt_finds_the_ring_held_for_is_finished_as_the_ring_is_let_go() {
    let device = device();
    let probe = Arc::new(Probe::default());
    let token = next_token(&device);
    let waiter = block_on(device.submit_read(3, TestBuffer::probed(&device, 512, &probe)));

    let queue = &device.object().queues[0];
    let held = ring(&device);
    let len = held.device_respond(token, &read_response(0x66, 512));
    held.device_complete(token, len);
    // The interrupt arrives while a submitter holds the ring: it leaves
    // the drain to the holder rather than spin.
    device.handle_interrupt();
    assert!(probe.completions().is_empty());

    device.object().unlock(queue, held);
    assert_eq!(probe.completions(), [Ok(())]);
    let (_, buffer) = answered(block_on(waiter));
    assert_eq!(buffer.bytes(), [0x66_u8; 512]);
}

#[test]
fn an_answer_for_a_request_never_submitted_resets_the_device_and_fails_every_request() {
    let device = device();
    let waiting = Arc::new(Probe::default());
    let detached = Arc::new(Probe::default());
    let waiter = block_on(device.submit_read(0, TestBuffer::probed(&device, 512, &waiting)));
    drop(block_on(device.submit_write(
        1,
        TestBuffer::probed(&device, 512, &detached),
    )));
    let stray = next_token(&device);

    // The device names a chain the driver never handed it.
    ring(&device).device_complete(stray, 1);
    device.handle_interrupt();

    assert!(
        device.object().transport.status().is_empty(),
        "the device is reset before any buffer is handed back"
    );
    assert_eq!(waiting.completions(), [Err(IoError::DeviceFault)]);
    assert_eq!(detached.completions(), [Err(IoError::DeviceFault)]);
    assert_eq!(detached.drops(), 1, "nobody waits for the write's buffer");
    let (result, buffer) = answered(block_on(waiter));
    assert_eq!(result, Err(IoError::DeviceFault));
    assert_eq!(waiting.drops(), 0, "the waiter has its buffer back");
    drop(buffer);

    let kicks = device.object().transport.kick_count();
    let (result, _buffer) = answered(block_on(device.read(2, TestBuffer::new(&device, 512))));
    assert_eq!(result, Err(IoError::DeviceFault));
    assert_eq!(device.object().transport.kick_count(), kicks);
}

#[test]
fn multiqueue_programs_one_queue_per_processor() {
    let transport = FakeTransport::new(FakeTransportConfig {
        device_type: DeviceType::Block,
        offered_features: VirtioFeatures::VERSION_1.bits() | BLK_F_MQ,
        queue_size: 8,
        supports_queue_reset: false,
        absent_queues: &[],
    });
    transport.set_config_u32(CFG_CAPACITY_LOW, TEST_CAPACITY_SECTORS);
    transport.set_config_u16(CFG_NUM_QUEUES, 8);

    let device = VirtioBlockDevice::<_, _, TestBuffer>::new(transport, FakeAffinity::<4>)
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
        absent_queues: &[],
    });
    transport.set_config_u32(CFG_CAPACITY_LOW, TEST_CAPACITY_SECTORS);

    let rejected = VirtioBlockDevice::<_, _, TestBuffer>::new(transport, FakeAffinity::<1>).err();
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
        max_transfer_bytes: u32::MAX as usize,
        capacity_blocks: 8,
    }
}

/// A block device backed by host memory, for the swap backend tests. It
/// carries a request out as it is submitted, answers it at once unless
/// told to hold its answers, and hands out request memory from a confined
/// pool of its own.
struct MemoryBlockDevice {
    blocks: SpinMutex<Vec<u8>>,
    block_size: usize,
    memory: ConfinedDmaPool,
    flushes: AtomicU64,
    fail_reads: bool,
    /// Each submission first parks once, as one waiting for room in a
    /// full queue does, before the device takes the request.
    queue_full: AtomicBool,
    /// Requests are kept unanswered in `held` until
    /// [`MemoryBlockDevice::answer_held`], as a device that has not got
    /// to them yet keeps them.
    hold_answers: AtomicBool,
    held: SpinMutex<Vec<HeldRequest>>,
}

/// Where a [`MemoryBlockDevice`] request's answer meets its waiter.
#[derive(Default)]
struct AnswerSlot {
    completion: Option<BufResult<(), TestBuffer>>,
    waker: Option<core::task::Waker>,
}

/// A request the device has taken and not answered. Its buffer is the
/// device's until then, whether or not the waiter is still there.
struct HeldRequest {
    buffer: TestBuffer,
    result: IoResult<()>,
    slot: Arc<SpinMutex<AnswerSlot>>,
}

/// The waiter of a [`MemoryBlockDevice`] request.
struct MemoryWaiter {
    slot: Arc<SpinMutex<AnswerSlot>>,
}

impl Future for MemoryWaiter {
    type Output = BufResult<(), TestBuffer>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut slot = self.slot.lock();
        match slot.completion.take() {
            Some(completion) => Poll::Ready(completion),
            None => {
                slot.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl MemoryBlockDevice {
    fn new(block_size: usize, block_count: usize) -> Self {
        Self {
            blocks: SpinMutex::new(vec![0_u8; block_size * block_count]),
            block_size,
            // A bump arena that never reuses memory: room for every
            // page-aligned staging buffer one swap test allocates.
            memory: ConfinedDmaPool::new(64 * PhysFrame::SIZE),
            flushes: AtomicU64::new(0),
            fail_reads: false,
            queue_full: AtomicBool::new(false),
            hold_answers: AtomicBool::new(false),
            held: SpinMutex::new(Vec::new()),
        }
    }

    fn stall_submissions(&self, stall: bool) {
        self.queue_full.store(stall, Ordering::Relaxed);
    }

    fn hold_answers(&self, hold: bool) {
        self.hold_answers.store(hold, Ordering::Relaxed);
    }

    /// Answers every held request: its buffer is completed, and the
    /// answer goes to its waiter, or is dropped with the buffer when the
    /// waiter is gone.
    fn answer_held(&self) {
        let held = core::mem::take(&mut *self.held.lock());
        for HeldRequest {
            mut buffer,
            result,
            slot,
        } in held
        {
            buffer.complete(result);
            let mut slot = slot.lock();
            slot.completion = Some(Self::answered(buffer, result));
            if let Some(waker) = slot.waker.take() {
                waker.wake();
            }
        }
    }

    async fn wait_for_room(&self) {
        if self.queue_full.load(Ordering::Relaxed) {
            futures_lite::future::yield_now().await;
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

    /// The answer a waiter resolves to: the buffer comes back whatever
    /// the result was.
    fn answered(buffer: TestBuffer, result: IoResult<()>) -> BufResult<(), TestBuffer> {
        match result {
            Ok(()) => Ok(((), buffer)),
            Err(error) => Err(BufError::returned(error, buffer)),
        }
    }

    fn answer(&self, mut buffer: TestBuffer, result: IoResult<()>) -> MemoryWaiter {
        let slot = Arc::new(SpinMutex::new(AnswerSlot::default()));
        if self.hold_answers.load(Ordering::Relaxed) {
            self.held.lock().push(HeldRequest {
                buffer,
                result,
                slot: Arc::clone(&slot),
            });
        } else {
            buffer.complete(result);
            slot.lock().completion = Some(Self::answered(buffer, result));
        }
        MemoryWaiter { slot }
    }
}

impl BlockDevice for MemoryBlockDevice {
    type Buffer = TestBuffer;
    type Memory = ConfinedDmaBuffer;
    type Waiter<'a> = MemoryWaiter;

    fn memory(&self, bytes: usize) -> Result<ConfinedDmaBuffer, DmaError> {
        let layout =
            Layout::from_size_align(bytes, PhysFrame::SIZE).map_err(|_| DmaError::TooLarge {
                bytes,
                limit: isize::MAX as usize & !(PhysFrame::SIZE - 1),
            })?;
        self.memory
            .allocate(layout, DmaAccess::READ | DmaAccess::WRITE)
    }

    async fn submit_read(&self, block_id: usize, mut buffer: TestBuffer) -> Self::Waiter<'_> {
        self.wait_for_room().await;
        if self.fail_reads {
            return self.answer(buffer, Err(IoError::DeviceFault));
        }
        let offset = block_id * self.block_size;
        let len = buffer.as_ref().len();
        buffer
            .as_mut()
            .copy_from_slice(&self.blocks.lock()[offset..offset + len]);
        self.answer(buffer, Ok(()))
    }

    async fn submit_write(&self, block_id: usize, buffer: TestBuffer) -> Self::Waiter<'_> {
        self.wait_for_room().await;
        let offset = block_id * self.block_size;
        let len = buffer.as_ref().len();
        self.blocks.lock()[offset..offset + len].copy_from_slice(buffer.as_ref());
        self.answer(buffer, Ok(()))
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
            max_transfer_bytes: u32::MAX as usize,
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

    fn is_read_only(&self) -> bool {
        false
    }
}

#[test]
fn a_swapped_out_page_comes_back_unchanged_and_is_not_committed() {
    let backend = VirtioBlockSwapBackend::from_entire_device(MemoryBlockDevice::new(512, 16))
        .expect("swap backend should initialize");
    // Deliberately not a whole number of blocks: the staging buffer is
    // rounded up to the next one, and the page rides in its head.
    let mut page = backend.buffer(1300).expect("staging buffer");
    assert_eq!(page.as_ref().len(), 1536);
    assert!(
        backend.device.memory.reaches(page.as_ref()),
        "the staging page is the device's own memory"
    );
    assert_eq!(
        backend.buffer(0).err(),
        Some(VirtioBlockSwapError::EmptyPayload)
    );
    for (index, byte) in page.as_mut().iter_mut().enumerate() {
        *byte = index as u8;
    }
    let expected = page.as_ref().to_vec();

    let (token, _page) = block_on(backend.swap_out(page)).expect("swap-out should succeed");
    assert_eq!(
        backend.device.flushes(),
        0,
        "swap is scratch storage; a flush per evicted page buys durability nobody reads"
    );

    let restored = backend.buffer(1300).expect("staging buffer");
    let ((), restored) =
        block_on(backend.swap_in(token, restored)).expect("swap-in should succeed");
    assert_eq!(restored.as_ref(), expected);
}

/// Swaps `bytes` bytes of `fill` out and returns the token.
fn swapped(
    backend: &VirtioBlockSwapBackend<MemoryBlockDevice>,
    bytes: usize,
    fill: u8,
) -> Result<VirtioBlockSwapToken, VirtioBlockSwapError> {
    block_on(backend.swap_out(staged(backend, bytes, fill)))
        .map(|(token, _page)| token)
        .map_err(BufError::into_error)
}

/// Swaps `token` back into a fresh buffer of `bytes` bytes and returns
/// the bytes.
fn restored(
    backend: &VirtioBlockSwapBackend<MemoryBlockDevice>,
    token: VirtioBlockSwapToken,
    bytes: usize,
) -> Result<Vec<u8>, VirtioBlockSwapError> {
    block_on(backend.swap_in(token, staged(backend, bytes, 0)))
        .map(|((), page)| page.as_ref().to_vec())
        .map_err(BufError::into_error)
}

/// A staging buffer of `bytes` bytes of `fill`.
fn staged(
    backend: &VirtioBlockSwapBackend<MemoryBlockDevice>,
    bytes: usize,
    fill: u8,
) -> TestBuffer {
    let mut buffer = backend.buffer(bytes).expect("staging buffer");
    buffer.as_mut().fill(fill);
    buffer
}

#[test]
fn a_failed_swap_in_keeps_the_token_valid() {
    let backend =
        VirtioBlockSwapBackend::from_entire_device(MemoryBlockDevice::failing_reads(512, 4))
            .expect("swap backend should initialize");
    let token = swapped(&backend, 1024, 7).expect("swap-out should succeed");

    let refused = block_on(backend.swap_in(token, staged(&backend, 1024, 0)))
        .err()
        .expect("the read fails");
    assert_eq!(
        refused.error,
        VirtioBlockSwapError::Io(IoError::DeviceFault)
    );
    assert!(refused.buffers.is_some(), "the staging page comes back");
    assert_eq!(
        backend.state.lock().available_blocks(),
        2,
        "the extent of a page that is still on the device must stay allocated"
    );
}

#[test]
fn swap_extents_are_reused_and_coalesced() {
    let backend = VirtioBlockSwapBackend::from_entire_device(MemoryBlockDevice::new(512, 8))
        .expect("swap backend should initialize");

    let first = swapped(&backend, 1024, 1).expect("first swap-out");
    let second = swapped(&backend, 1024, 2).expect("second swap-out");
    let third = swapped(&backend, 2048, 3).expect("third swap-out");
    assert_eq!(backend.state.lock().available_blocks(), 0);
    assert_eq!(
        swapped(&backend, 512, 4),
        Err(VirtioBlockSwapError::OutOfSwap {
            requested_blocks: 1,
            available_blocks: 0,
        })
    );

    restored(&backend, second, 1024).expect("swap-in of the middle extent");
    restored(&backend, first, 1024).expect("swap-in of the first extent");
    {
        let mut state = backend.state.lock();
        assert_eq!(state.available_blocks(), 4);
        assert_eq!(
            state.free.len(),
            1,
            "two adjacent freed extents are one free extent"
        );
    }

    assert_eq!(
        restored(&backend, third, 2048).expect("swap-in of the last extent"),
        vec![3_u8; 2048]
    );
    let mut state = backend.state.lock();
    assert_eq!(state.free.len(), 1);
    assert_eq!(state.available_blocks(), 8);
}

/// Every field of the configuration space the driver sized its
/// requests or reported its geometry from is compared after a reset,
/// not only the capacity.
#[test]
fn a_device_whose_limits_changed_after_the_reset_is_refused() {
    let features = BLK_F_SIZE_MAX
        | BLK_F_SEG_MAX
        | BLK_F_BLK_SIZE
        | BLK_F_TOPOLOGY
        | BLK_F_CONFIG_WCE
        | BLK_F_DISCARD
        | BLK_F_WRITE_ZEROES;
    type Change = fn(&ConfinedTransport);
    let changes: [(&str, Change); 8] = [
        ("seg_max", |t| t.set_config_u32(CFG_SEG_MAX, 8)),
        ("size_max", |t| t.set_config_u32(CFG_SIZE_MAX, 8192)),
        ("blk_size", |t| t.set_config_u32(CFG_BLK_SIZE, 512)),
        ("topology", |t| t.set_config_u16(CFG_MIN_IO_SIZE, 4)),
        ("writeback", |t| t.set_config_u8(CFG_WRITEBACK, 0)),
        ("discard", |t| t.set_config_u32(CFG_MAX_DISCARD_SECTORS, 1)),
        ("write zeroes", |t| {
            t.set_config_u32(CFG_MAX_WRITE_ZEROES_SECTORS, 1);
        }),
        ("write zeroes segments", |t| {
            t.set_config_u32(CFG_MAX_WRITE_ZEROES_SEG, 1);
        }),
    ];
    for (name, change) in changes {
        let device = device_with(features, |transport| {
            transport.set_config_u32(CFG_SEG_MAX, 4);
            transport.set_config_u32(CFG_SIZE_MAX, 16384);
            transport.set_config_u32(CFG_BLK_SIZE, 4096);
            transport.set_config_u16(CFG_MIN_IO_SIZE, 2);
            transport.set_config_u8(CFG_WRITEBACK, 1);
            transport.set_config_u32(CFG_MAX_DISCARD_SECTORS, 64);
            transport.set_config_u32(CFG_MAX_DISCARD_SEG, 2);
            transport.set_config_u32(CFG_MAX_WRITE_ZEROES_SECTORS, 64);
            transport.set_config_u32(CFG_MAX_WRITE_ZEROES_SEG, 2);
        });
        change(&device.object().transport);

        assert!(
            matches!(
                device.reinitialize(),
                Err(ReinitError::DeviceChanged { .. })
            ),
            "a changed {name} must be refused"
        );
    }
}

/// A multi-queue device has every queue programmed again, not only the
/// first.
#[test]
fn every_queue_of_a_multi_queue_device_is_programmed_again() {
    let transport = FakeTransport::new(FakeTransportConfig {
        device_type: DeviceType::Block,
        offered_features: VirtioFeatures::VERSION_1.bits() | BLK_F_MQ,
        queue_size: 8,
        supports_queue_reset: false,
        absent_queues: &[],
    });
    transport.set_config_u32(CFG_CAPACITY_LOW, TEST_CAPACITY_SECTORS);
    transport.set_config_u16(CFG_NUM_QUEUES, 2);
    let device = VirtioBlockDevice::<_, _, TestBuffer>::new(transport, FakeAffinity::<2>)
        .expect("a two-queue device comes up")
        .into_resource(BlockDeviceRights::READ | BlockDeviceRights::WRITE);
    assert_eq!(device.queue_count(), 2);
    device.quiesce(|| false).expect("both queues are idle");

    device.object().transport.reset_behind_the_driver();
    assert_eq!(
        device.reinitialize(),
        Ok(DeviceReset {
            device: DeviceType::Block
        })
    );

    let programmed = device.object().transport.programmed_queues();
    for index in 0..2 {
        assert_eq!(
            programmed
                .iter()
                .filter(|queue| queue.index == index)
                .count(),
            2,
            "queue {index} is programmed at bring-up and again after the reset"
        );
    }
}

#[test]
fn a_swap_out_dropped_before_the_device_has_its_write_gives_the_extent_back() {
    let backend = VirtioBlockSwapBackend::from_entire_device(MemoryBlockDevice::new(512, 4))
        .expect("swap backend should initialize");
    backend.device.stall_submissions(true);
    let page = staged(&backend, 1024, 9);

    {
        let mut swap_out = pin!(backend.swap_out(page));
        assert!(
            block_on(poll_once(swap_out.as_mut())).is_none(),
            "the submission parks for room in the queue"
        );
        assert_eq!(
            backend.state.lock().available_blocks(),
            2,
            "the parked swap-out holds its extent"
        );
    }

    let mut state = backend.state.lock();
    assert_eq!(
        state.available_blocks(),
        4,
        "a dropped swap-out whose write never reached the device leaks no extent"
    );
    assert_eq!(state.free.len(), 1);
}

#[test]
fn a_swap_out_dropped_while_the_device_holds_its_write_gives_the_extent_back_once_answered() {
    let backend = VirtioBlockSwapBackend::from_entire_device(MemoryBlockDevice::new(512, 4))
        .expect("swap backend should initialize");
    backend.device.hold_answers(true);
    let page = staged(&backend, 1024, 9);

    {
        let mut swap_out = pin!(backend.swap_out(page));
        assert!(
            block_on(poll_once(swap_out.as_mut())).is_none(),
            "the write is with the device, unanswered"
        );
    }

    assert_eq!(
        backend.state.lock().available_blocks(),
        2,
        "blocks an abandoned write may still reach are not handed to another page"
    );
    backend.device.hold_answers(false);
    let other = swapped(&backend, 1024, 1).expect("the rest still serves");
    assert_eq!(
        other.start_block, 2,
        "the extent of the unanswered write is not reused"
    );
    restored(&backend, other, 1024).expect("swap-in of the other page");

    backend.device.answer_held();
    let mut state = backend.state.lock();
    assert_eq!(
        state.available_blocks(),
        4,
        "once the device answered the abandoned write, its extent is free again"
    );
    assert_eq!(state.free.len(), 1);
    assert!(state.abandoned.is_empty());
    drop(state);
    let reused = swapped(&backend, 1024, 2).expect("swap-out after the answer");
    assert_eq!(
        reused.start_block, 0,
        "the answered write's extent is handed out again"
    );
}

#[test]
fn a_swap_in_dropped_mid_read_leaves_the_token_valid() {
    let backend = VirtioBlockSwapBackend::from_entire_device(MemoryBlockDevice::new(512, 4))
        .expect("swap backend should initialize");
    let mut page = backend.buffer(1024).expect("staging buffer");
    for (index, byte) in page.as_mut().iter_mut().enumerate() {
        *byte = index as u8;
    }
    let expected = page.as_ref().to_vec();
    let (token, _page) = block_on(backend.swap_out(page)).expect("swap-out should succeed");
    backend.device.hold_answers(true);

    {
        let mut swap_in = pin!(backend.swap_in(token, staged(&backend, 1024, 0)));
        assert!(
            block_on(poll_once(swap_in.as_mut())).is_none(),
            "the read is with the device, unanswered"
        );
    }
    assert_eq!(
        backend.state.lock().available_blocks(),
        2,
        "the extent of a page a dropped swap-in never read back stays allocated"
    );

    backend.device.answer_held();
    backend.device.hold_answers(false);
    assert_eq!(
        restored(&backend, token, 1024).expect("the token is still valid"),
        expected
    );
    assert_eq!(backend.state.lock().available_blocks(), 4);
}

/// Counts the times a task was woken.
#[derive(Default)]
struct Wakes(AtomicUsize);

impl Wakes {
    fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

impl alloc::task::Wake for Wakes {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Polls `future` once, as a task that `wakes` counts the wake-ups of.
fn poll_counting<F: Future>(future: Pin<&mut F>, wakes: &Arc<Wakes>) -> Poll<F::Output> {
    let waker = core::task::Waker::from(Arc::clone(wakes));
    future.poll(&mut Context::from_waker(&waker))
}

/// A read's response whose status byte is `status`.
fn read_response_with_status(byte: u8, len: usize, status: u8) -> Vec<u8> {
    let mut response = vec![byte; len];
    response.push(status);
    response
}

fn live_runs(device: &TestDevice) -> usize {
    device.object().transport.pool().live_requests()
}

#[test]
fn a_read_is_delivered_whole_before_it_is_completed() {
    let device = device();
    let probe = Arc::new(Probe::default());
    let mut read = pin!(device.read(0, TestBuffer::host(1024).with_probe(&probe)));
    assert!(block_on(poll_once(read.as_mut())).is_none());
    assert!(
        probe.heard().is_empty(),
        "nothing is delivered before the answer"
    );

    complete(&device, 0, &read_response(0x9d, 1024));

    // The bytes reach the buffer before the buffer is told the request is
    // over, because completing it is what publishes it to its consumers.
    assert_eq!(
        probe.heard(),
        [Heard::Delivered(vec![0x9d; 1024]), Heard::Completed(Ok(())),]
    );
    let (result, buffer) =
        answered(block_on(poll_once(read.as_mut())).expect("the read has completed"));
    assert_eq!(result, Ok(()));
    assert_eq!(buffer.bytes(), [0x9d_u8; 1024]);
}

#[test]
fn a_failed_or_short_read_delivers_nothing() {
    // The device answers with an error status after writing the payload.
    let device = device();
    let probe = Arc::new(Probe::default());
    let mut read = pin!(device.read(0, TestBuffer::host(512).with_probe(&probe)));
    assert!(block_on(poll_once(read.as_mut())).is_none());
    complete(&device, 0, &read_response_with_status(0x41, 512, 1));
    assert_eq!(probe.heard(), [Heard::Completed(Err(IoError::DeviceFault))]);
    let (result, buffer) =
        answered(block_on(poll_once(read.as_mut())).expect("the read has completed"));
    assert_eq!(result, Err(IoError::DeviceFault));
    assert_eq!(
        buffer.bytes(),
        [0_u8; 512],
        "the caller's bytes are untouched"
    );

    // The device claims success but only filled half of the chain.
    let device = device_with(0, |_| {});
    let probe = Arc::new(Probe::default());
    let mut read = pin!(device.read(0, TestBuffer::host(1024).with_probe(&probe)));
    assert!(block_on(poll_once(read.as_mut())).is_none());
    {
        let ring = ring(&device);
        ring.device_respond(0, &read_response(0x42, 1024));
        ring.device_complete(0, 513);
    }
    device.handle_interrupt();
    assert_eq!(probe.heard(), [Heard::Completed(Err(IoError::DeviceFault))]);
    let (result, buffer) =
        answered(block_on(poll_once(read.as_mut())).expect("the read has completed"));
    assert_eq!(result, Err(IoError::DeviceFault));
    assert_eq!(
        buffer.bytes(),
        [0_u8; 1024],
        "the caller's bytes are untouched"
    );
}

#[test]
fn the_bytes_of_a_write_are_copied_before_the_chain_is_published() {
    let device = device();
    let observer = device.clone();
    let probe = Arc::new(Probe::default());
    let kicks = device.object().transport.kick_count();
    let buffer = TestBuffer::host_filled(&[0x61; 512])
        .with_probe(&probe)
        .looking_when_filled(move || Heard::Filled {
            // Looked at from inside the copy: the ring is free for this
            // to take, so the copy holds no lock of the driver.
            in_flight: ring(&observer).in_flight_chains(),
            kicks: observer.object().transport.kick_count(),
        });
    let mut write = pin!(device.write(1, buffer));

    assert!(block_on(poll_once(write.as_mut())).is_none());

    assert_eq!(
        probe.heard(),
        [Heard::Filled {
            in_flight: 0,
            kicks
        }]
    );
    assert_eq!(ring(&device).in_flight_chains(), 1);
    assert_eq!(device.object().transport.kick_count(), kicks + 1);
}

#[test]
fn a_request_parks_until_a_run_comes_back_and_then_takes_it() {
    let device = device();
    let pool = device.object().transport.pool().clone();
    pool.admit_only(1);
    let first_token = next_token(&device);
    let mut first = pin!(device.read(0, TestBuffer::host(512)));
    assert!(block_on(poll_once(first.as_mut())).is_none());
    assert_eq!(live_runs(&device), 1);
    let second_token = next_token(&device);
    let kicks = device.object().transport.kick_count();

    let wakes = Arc::new(Wakes::default());
    let mut second = pin!(device.read(1, TestBuffer::host(512)));
    assert!(poll_counting(second.as_mut(), &wakes).is_pending());
    assert_eq!(live_runs(&device), 1, "the second request holds no run");
    assert_eq!(
        ring(&device).in_flight_chains(),
        1,
        "and no slot of the ring"
    );
    assert_eq!(device.object().transport.kick_count(), kicks);
    assert_eq!(wakes.count(), 0, "it is parked, not spinning");

    // The first answer frees the run, which wakes the parked request.
    complete(&device, first_token, &read_response(0x01, 512));
    assert_eq!(wakes.count(), 1);
    let (result, _) =
        answered(block_on(poll_once(first.as_mut())).expect("the first read has completed"));
    assert_eq!(result, Ok(()));

    assert!(poll_counting(second.as_mut(), &wakes).is_pending());
    assert_eq!(
        ring(&device).in_flight_chains(),
        1,
        "the second chain is issued"
    );
    assert_eq!(device.object().transport.kick_count(), kicks + 1);
    complete(&device, second_token, &read_response(0x02, 512));
    let (result, buffer) = answered(block_on(poll_once(second.as_mut())).expect("done"));
    assert_eq!(result, Ok(()));
    assert_eq!(buffer.bytes(), [0x02_u8; 512]);
    assert_eq!(live_runs(&device), 0);
}

/// The run is freed after the allocation was refused and before the
/// caller declared that it would wait: the declaration's mark is already
/// past the free, so parking on it alone would sleep forever. Only asking
/// again after the declaration finds the run.
#[test]
fn a_run_freed_between_the_refusal_and_the_wait_is_not_slept_through() {
    let device = device();
    let pool = device.object().transport.pool().clone();
    pool.admit_only(1);
    let first_token = next_token(&device);
    // Nobody waits for the first read, so its slot is free again the
    // moment it is answered.
    drop(block_on(device.submit_read(0, TestBuffer::host(512))));
    assert_eq!(live_runs(&device), 1);
    let observer = device.clone();
    pool.on_refusal(move || complete(&observer, first_token, &read_response(0x21, 512)));
    let kicks = device.object().transport.kick_count();

    let mut second = pin!(device.read(1, TestBuffer::host(512)));
    assert!(
        block_on(poll_once(second.as_mut())).is_none(),
        "the second read is with the device, unanswered"
    );

    assert_eq!(
        ring(&device).in_flight_chains(),
        1,
        "the second chain was issued in the same poll, not parked"
    );
    assert_eq!(device.object().transport.kick_count(), kicks + 1);
    assert_eq!(live_runs(&device), 1, "the first run went, the second came");
}

/// Every way a request ends gives its run back to the memory it came
/// from.
#[test]
fn every_way_a_request_ends_gives_its_run_back() {
    // Answered and collected, a read and a write.
    let device = device();
    let mut read = pin!(device.read(0, TestBuffer::host(512)));
    assert!(block_on(poll_once(read.as_mut())).is_none());
    assert_eq!(live_runs(&device), 1, "a request in flight holds its run");
    complete(&device, 0, &read_response(1, 512));
    assert_eq!(
        live_runs(&device),
        0,
        "the answer frees it, before collection"
    );
    drop(answered(block_on(poll_once(read.as_mut())).expect("done")));
    let token = next_token(&device);
    let mut write = pin!(device.write(0, TestBuffer::host(512)));
    assert!(block_on(poll_once(write.as_mut())).is_none());
    assert_eq!(live_runs(&device), 1);
    complete(&device, token, &[0]);
    assert_eq!(live_runs(&device), 0);
    drop(answered(block_on(poll_once(write.as_mut())).expect("done")));

    // Answered with an error status, and answered short.
    let device = device_with(0, |_| {});
    let mut read = pin!(device.read(0, TestBuffer::host(512)));
    assert!(block_on(poll_once(read.as_mut())).is_none());
    complete(&device, 0, &read_response_with_status(1, 512, 1));
    assert_eq!(live_runs(&device), 0);
    drop(answered(block_on(poll_once(read.as_mut())).expect("done")));
    let token = next_token(&device);
    let mut read = pin!(device.read(0, TestBuffer::host(1024)));
    assert!(block_on(poll_once(read.as_mut())).is_none());
    {
        let ring = ring(&device);
        ring.device_respond(token, &read_response(2, 1024));
        ring.device_complete(token, 513);
    }
    device.handle_interrupt();
    assert_eq!(live_runs(&device), 0);
    drop(answered(block_on(poll_once(read.as_mut())).expect("done")));

    // The waiter went before the answer.
    let device = device_with(0, |_| {});
    let token = next_token(&device);
    drop(block_on(device.submit_read(0, TestBuffer::host(512))));
    assert_eq!(
        live_runs(&device),
        1,
        "a detached request still holds its run"
    );
    complete(&device, token, &read_response(3, 512));
    assert_eq!(live_runs(&device), 0);

    // The submission went while it waited for room in the ring, holding
    // its run.
    let device = device_with(0, |_| {});
    let first = block_on(device.submit_read(0, TestBuffer::host(512)));
    let second = block_on(device.submit_read(1, TestBuffer::host(512)));
    {
        let mut third = pin!(device.submit_read(2, TestBuffer::host(512)));
        assert!(block_on(poll_once(third.as_mut())).is_none());
        assert_eq!(
            live_runs(&device),
            3,
            "the third holds a run while it waits"
        );
    }
    assert_eq!(
        live_runs(&device),
        2,
        "dropping the submission gives it back"
    );
    drop((first, second));

    // The device broke the used-ring contract: every request in flight,
    // awaited or not, is failed with its run.
    let device = device_with(0, |_| {});
    let waiter = block_on(device.submit_read(0, TestBuffer::host(512)));
    drop(block_on(device.submit_write(1, TestBuffer::host(512))));
    assert_eq!(live_runs(&device), 2);
    let stray = next_token(&device);
    ring(&device).device_complete(stray, 1);
    device.handle_interrupt();
    assert_eq!(live_runs(&device), 0);
    drop(answered(block_on(waiter)));
    // A stopped device is refused before any run is drawn.
    drop(answered(block_on(device.read(2, TestBuffer::host(512)))));
    assert_eq!(live_runs(&device), 0);
}
