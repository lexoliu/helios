use super::*;

use crate::testing::{
    ConfinedDmaBuffer, ConfinedDmaPool, ConfinedTransport, FakeAffinity, FakeTransport,
    FakeTransportConfig,
};
use crate::transport::VirtioFeatures;
use alloc::sync::Arc;
use alloc::vec;
use core::pin::pin;
use core::ptr::NonNull;
use futures_lite::future::{block_on, poll_once};
use helios_hal::dma::DmaBuffer;
use spin::Mutex as SpinMutex;

/// A device with 2 MiB of capacity in 512-byte sectors.
const TEST_CAPACITY_SECTORS: u32 = 4096;

/// What a [`TestBuffer`] heard from the driver.
#[derive(Default)]
struct Probe {
    completions: SpinMutex<Vec<IoResult<()>>>,
    drops: AtomicUsize,
}

impl Probe {
    fn completions(&self) -> Vec<IoResult<()>> {
        self.completions.lock().clone()
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
        }
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
    fn segments(&self) -> impl Iterator<Item = NonNull<[u8]>> + '_ {
        core::iter::once(self.bytes)
    }

    fn complete(&mut self, result: IoResult<()>) {
        if let Some(probe) = &self.probe {
            probe.completions.lock().push(result);
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

    // Twice that needs four segments, more than one request carries:
    // refused, with the buffer told why, and nothing reaches the device.
    let kicks = device.object().transport.kick_count();
    let probe = Arc::new(Probe::default());
    let (result, _buffer) = answered(block_on(
        device.write(0, TestBuffer::probed(&device, 4096, &probe)),
    ));
    assert_eq!(result, Err(IoError::OutOfBounds));
    assert_eq!(probe.completions(), [Err(IoError::OutOfBounds)]);
    assert_eq!(device.object().transport.kick_count(), kicks);
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
    assert_eq!(chain[1].0, payload, "the payload is the caller's buffer");
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
            Some(IoError::InvalidBufferLength {
                required_multiple: block,
                actual: bytes,
            })
        );
    }
}

#[test]
fn a_buffer_the_device_cannot_reach_is_refused_and_handed_back() {
    // A segment outside the device's memory is refused before any
    // descriptor is written. A buffer from another device's memory has
    // the right type but not the right provenance, which is what host
    // memory, a stack array or a future's frame would have too.
    let device = device();
    let kicks = device.object().transport.kick_count();
    for foreign in [
        TestBuffer::in_pool(&ConfinedDmaPool::new(4096), 512),
        TestBuffer::host(512),
    ] {
        let probe = Arc::new(Probe::default());
        let (result, buffer) = answered(block_on(device.read(0, foreign.with_probe(&probe))));
        assert_eq!(result, Err(IoError::DeviceFault));
        assert_eq!(probe.completions(), [Err(IoError::DeviceFault)]);
        assert_eq!(probe.drops(), 0, "nothing was published, so it comes back");
        drop(buffer);
    }
    assert_eq!(device.object().transport.kick_count(), kicks);
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
fn a_readonly_device_refuses_writes() {
    let device = device_with(BLK_F_RO, |_| {});

    let (result, _buffer) = answered(block_on(device.write(0, TestBuffer::new(&device, 512))));
    assert_eq!(result, Err(IoError::ReadOnly));
}

#[test]
fn a_dropped_waiter_leaves_the_request_to_finish_and_release_its_buffer() {
    let device = device();
    let pool = device.object().transport.pool().clone();
    let probe = Arc::new(Probe::default());
    let token = next_token(&device);
    let abandoned = TestBuffer::probed(&device, 512, &probe);
    let abandoned_bytes = (abandoned.device_address(), 512);
    {
        let mut read = pin!(device.read(4, abandoned));
        assert!(block_on(poll_once(read.as_mut())).is_none());
    }
    // The task is gone, but the device still has the request: the driver
    // keeps the buffer it may still write into.
    assert_eq!(probe.drops(), 0);
    assert!(probe.completions().is_empty());
    assert!(
        !pool.is_released(abandoned_bytes.0, abandoned_bytes.1),
        "the buffer of a dropped read is still the device's"
    );

    // The device answers with nobody waiting, into memory that is still
    // the request's. The interrupt finishes the request: the buffer hears
    // the answer, and goes.
    {
        let ring = ring(&device);
        let written = pool.respond_into_live_memory(&ring, token, &read_response(0x44, 512));
        ring.device_complete(token, written);
    }
    device.handle_interrupt();
    assert_eq!(probe.completions(), [Ok(())]);
    assert_eq!(probe.drops(), 1);
    assert!(
        pool.is_released(abandoned_bytes.0, abandoned_bytes.1),
        "the device's answer is what frees the dropped read's buffer"
    );
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
        max_segments: 1,
        max_segment_bytes: u32::MAX,
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

    fn memory(&self, bytes: usize) -> IoResult<ConfinedDmaBuffer> {
        let layout =
            Layout::from_size_align(bytes, PhysFrame::SIZE).map_err(|_| IoError::OutOfBounds)?;
        self.memory.allocate_zeroed(layout)
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
