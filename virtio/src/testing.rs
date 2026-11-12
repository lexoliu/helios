//! Recording device fakes shared by the virtio unit tests.
//!
//! Two fakes live here. [`MmioRegisterBus`] is a plain register file used
//! to exercise the MMIO transport's register layout. [`FakeTransport`] is
//! a transport that allocates real host memory for the virtqueue rings
//! and records every driver-visible side effect — queue programming,
//! kicks, notification-data payloads and per-queue resets — so the queue
//! tests can assert on them and can play the device side by writing into
//! the rings directly.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use helios_hal::dma::{DeviceAddress, DmaAddressing, DmaBuffer, DmaError, DmaMemory, DmaSpaceMark};
use helios_hal::io::{IoError, IoResult};
use helios_hal::iommu::DmaAccess;
use spin::Mutex;

use crate::bus::{DeviceBus, IdentityDmaPool};
use crate::transport::{DeviceStatus, DeviceType, InterruptStatus, VirtioTransport};

const REG_MAGIC_VALUE: usize = 0x000;
const REG_VERSION: usize = 0x004;
const REG_DEVICE_ID: usize = 0x008;
const REG_DEVICE_FEATURES: usize = 0x010;
const REG_DEVICE_FEATURES_SEL: usize = 0x014;
const REG_QUEUE_NUM_MAX: usize = 0x034;
const REG_CONFIG_SPACE: usize = 0x100;
const MAGIC_VALUE: u32 = 0x7472_6976;
const MODERN_VERSION: u32 = 2;
const REGISTER_WORDS: usize = 128;

/// Offsets in the select/sub-select configuration register file, as
/// `struct virtio_input_config` lays it out.
const CFG_SELECT: usize = 0;
const CFG_SUBSEL: usize = 1;
const CFG_SIZE: usize = 2;
const CFG_PAYLOAD: usize = 8;
const CFG_PAYLOAD_BYTES: usize = 128;

/// A virtio-mmio register file backed by an array.
pub(crate) struct MmioRegisterBus {
    registers: UnsafeCell<[u32; REGISTER_WORDS]>,
    offered_features: u64,
    dma: IdentityDmaPool,
}

impl MmioRegisterBus {
    pub(crate) fn new(device_type: DeviceType, offered_features: u64) -> Self {
        let mut registers = [0_u32; REGISTER_WORDS];
        registers[REG_MAGIC_VALUE / 4] = MAGIC_VALUE;
        registers[REG_VERSION / 4] = MODERN_VERSION;
        registers[REG_DEVICE_ID / 4] = device_type as u32;
        registers[REG_QUEUE_NUM_MAX / 4] = 16;
        registers[REG_CONFIG_SPACE / 4] = 0xfeed_beef;
        Self {
            registers: UnsafeCell::new(registers),
            offered_features,
            dma: IdentityDmaPool,
        }
    }

    /// The last value written to (or preset in) `offset`.
    pub(crate) fn register(&self, offset: usize) -> u32 {
        unsafe { (*self.registers.get())[offset / 4] }
    }
}

impl DeviceBus for MmioRegisterBus {
    type DmaPool = IdentityDmaPool;

    fn read_u16(&self, offset: usize) -> u16 {
        let word = self.read_u32(offset & !0x3);
        (word >> ((offset & 0x3) * 8)) as u16
    }

    fn read_u32(&self, offset: usize) -> u32 {
        if offset == REG_DEVICE_FEATURES {
            let half = self.register(REG_DEVICE_FEATURES_SEL);
            return match half {
                0 => self.offered_features as u32,
                1 => (self.offered_features >> 32) as u32,
                value => panic!("unexpected device feature selector {value}"),
            };
        }
        self.register(offset)
    }

    fn write_u8(&self, offset: usize, value: u8) {
        let word = offset & !0x3;
        let shift = (offset & 0x3) * 8;
        let current = self.read_u32(word) & !(0xff_u32 << shift);
        self.write_u32(word, current | (u32::from(value) << shift));
    }

    fn write_u32(&self, offset: usize, value: u32) {
        unsafe {
            (*self.registers.get())[offset / 4] = value;
        }
    }

    fn dma(&self) -> &Self::DmaPool {
        &self.dma
    }
}

unsafe impl Send for MmioRegisterBus {}
unsafe impl Sync for MmioRegisterBus {}

/// Host-memory bus: virtqueue rings are ordinary heap allocations whose
/// DMA address is their virtual address, put through whatever platform
/// translation the test gave the bus.
pub(crate) struct HeapBus<P = IdentityDmaPool> {
    /// Wide enough for the largest device configuration a driver in
    /// this crate reads: virtio-input's select/sub-select register file
    /// ends at offset 0x88, and virtio-blk's structure at 0x40.
    config: UnsafeCell<[u32; 64]>,
    dma: P,
}

impl<P> HeapBus<P> {
    fn new(dma: P) -> Self {
        Self {
            config: UnsafeCell::new([0; 64]),
            dma,
        }
    }
}

impl<P: DmaMemory> DeviceBus for HeapBus<P> {
    fn read_u16(&self, offset: usize) -> u16 {
        let word = self.read_u32(offset & !0x3);
        (word >> ((offset & 0x3) * 8)) as u16
    }

    fn read_u32(&self, offset: usize) -> u32 {
        unsafe { (*self.config.get())[offset / 4] }
    }

    fn write_u8(&self, offset: usize, value: u8) {
        let word = offset & !0x3;
        let shift = (offset & 0x3) * 8;
        let current = self.read_u32(word) & !(0xff_u32 << shift);
        self.write_u32(word, current | (u32::from(value) << shift));
    }

    fn write_u32(&self, offset: usize, value: u32) {
        unsafe {
            (*self.config.get())[offset / 4] = value;
        }
    }

    type DmaPool = P;

    fn dma(&self) -> &Self::DmaPool {
        &self.dma
    }
}

unsafe impl<P: Send> Send for HeapBus<P> {}
unsafe impl<P: Sync> Sync for HeapBus<P> {}

/// How a [`FakeTransport`] presents itself to a driver.
pub(crate) struct FakeTransportConfig {
    pub(crate) device_type: DeviceType,
    pub(crate) offered_features: u64,
    pub(crate) queue_size: u16,
    pub(crate) supports_queue_reset: bool,
    /// Queue indices this device does not present. Selecting one reads
    /// a queue size of zero, which is how real hardware says "there is
    /// no queue here" — the only way a driver can tell that a device
    /// advertised more queues than it built.
    pub(crate) absent_queues: &'static [u16],
}

impl Default for FakeTransportConfig {
    fn default() -> Self {
        Self {
            device_type: DeviceType::Block,
            offered_features: crate::transport::VirtioFeatures::VERSION_1.bits(),
            queue_size: 8,
            supports_queue_reset: true,
            absent_queues: &[],
        }
    }
}

/// The areas a driver programmed for one virtqueue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct QueueProgramming {
    pub(crate) index: u16,
    pub(crate) size: u16,
    pub(crate) descriptor_area: u64,
    pub(crate) driver_area: u64,
    pub(crate) device_area: u64,
}

#[derive(Default)]
struct FakeTransportLog {
    status: u32,
    driver_features: u64,
    kicks: Vec<u16>,
    notification_data: Vec<u32>,
    programmed: Vec<QueueProgramming>,
    resets: Vec<u16>,
    /// Whole-device resets the driver issued.
    driver_resets: usize,
    /// Whether the device drops `FEATURES_OK` from every status write.
    refuse_features_ok: bool,
    acknowledged_interrupts: usize,
    /// Interrupt status register the next acknowledgement reads and
    /// clears, as a device would present it.
    pending_interrupt: u32,
}

/// A platform with `PROCESSORS` processors, all of which report the
/// caller as running on processor zero.
///
/// A driver that picks a queue by processor only needs those two facts,
/// and a test that pins every request to one queue is what makes the
/// completions it plays back deterministic.
pub(crate) struct FakeAffinity<const PROCESSORS: usize>;

impl<const PROCESSORS: usize> crate::block::QueueAffinity for FakeAffinity<PROCESSORS> {
    fn current_processor(&self) -> usize {
        0
    }

    fn processor_count(&self) -> usize {
        PROCESSORS
    }
}

/// A virtio transport that records everything a driver does to it.
pub(crate) struct FakeTransport<P = IdentityDmaPool> {
    bus: HeapBus<P>,
    device_type: DeviceType,
    /// What the device offers right now. Atomic so a test can have the
    /// device withdraw features between a reset and the driver's second
    /// handshake.
    offered_features: AtomicU64,
    queue_size: u16,
    supports_queue_reset: bool,
    absent_queues: &'static [u16],
    /// Where the device's configuration structure ends. A real device
    /// sizes it to the last feature it offers and answers any access
    /// that runs past the end with all-ones, so a test that sets this
    /// sees exactly what a driver reading a field with the wrong width
    /// would see.
    config_len: AtomicUsize,
    /// The blocks a select/sub-select configuration register file
    /// answers with, keyed by the pair that selects them.
    ///
    /// virtio-input presents its whole self-description this way
    /// (virtio 1.2 §5.8.5), so a fake that only held a flat byte array
    /// could not present a device at all. Empty for every other device
    /// class, which leaves the flat array untouched.
    config_blocks: Mutex<Vec<(u8, u8, Vec<u8>)>>,
    /// The device's own memory windows, by region id, as a transport
    /// that publishes shared memory answers for them.
    shared_memory: Mutex<Vec<(u8, helios_hal::iommu::PhysicalRange)>>,
    log: Mutex<FakeTransportLog>,
}

impl FakeTransport {
    pub(crate) fn new(config: FakeTransportConfig) -> Self {
        Self::with_dma(config, IdentityDmaPool)
    }
}

impl<P: DmaMemory> FakeTransport<P> {
    /// A transport whose driver publishes addresses from `dma`.
    ///
    /// This is what a test uses to put a device behind a platform
    /// translation: the pool decides what kind of address the rings
    /// carry, and therefore what the feature handshake has to ask for.
    pub(crate) fn with_dma(config: FakeTransportConfig, dma: P) -> Self {
        Self {
            bus: HeapBus::new(dma),
            device_type: config.device_type,
            offered_features: AtomicU64::new(config.offered_features),
            queue_size: config.queue_size,
            supports_queue_reset: config.supports_queue_reset,
            absent_queues: config.absent_queues,
            config_len: AtomicUsize::new(usize::MAX),
            config_blocks: Mutex::new(Vec::new()),
            shared_memory: Mutex::new(Vec::new()),
            log: Mutex::new(FakeTransportLog::default()),
        }
    }

    /// Publishes a shared-memory region under `id`, the way a device
    /// with a host-visible aperture does.
    pub(crate) fn set_shared_memory_region(
        &self,
        id: u8,
        region: helios_hal::iommu::PhysicalRange,
    ) {
        self.shared_memory.lock().push((id, region));
    }

    /// Makes the device stop offering the features in `mask`, the way a
    /// device model that changed under a resumed guest does.
    pub(crate) fn withdraw_features(&self, mask: u64) {
        self.offered_features.fetch_and(!mask, Ordering::Relaxed);
    }

    /// Makes the device offer the features in `mask` as well.
    pub(crate) fn offer_features(&self, mask: u64) {
        self.offered_features.fetch_or(mask, Ordering::Relaxed);
    }

    /// Makes the device clear `FEATURES_OK` when the driver sets it, the
    /// way a device that dislikes the selection does.
    pub(crate) fn refuse_features_ok(&self) {
        self.log.lock().refuse_features_ok = true;
    }

    /// Plays the host resetting the device behind the driver's back: the
    /// status the driver wrote is gone. The queue and feature state the
    /// driver programmed is whatever the next handshake writes again.
    pub(crate) fn reset_behind_the_driver(&self) {
        self.log.lock().status = 0;
    }

    /// How many times the driver itself reset the device.
    pub(crate) fn driver_reset_count(&self) -> usize {
        self.log.lock().driver_resets
    }

    /// Ends the device configuration structure at `len` bytes, the way
    /// a device that offers nothing past a given feature does.
    pub(crate) fn bound_config_len(&self, len: usize) {
        self.config_len.store(len, Ordering::Relaxed);
    }

    fn config_access_fits(&self, offset: usize, width: usize) -> bool {
        offset + width <= self.config_len.load(Ordering::Relaxed)
    }

    /// The feature word the driver wrote back to the device.
    pub(crate) fn driver_features(&self) -> u64 {
        self.log.lock().driver_features
    }

    /// Number of queue kicks the driver issued.
    pub(crate) fn kick_count(&self) -> usize {
        self.log.lock().kicks.len()
    }

    /// Every VIRTIO_F_NOTIFICATION_DATA payload the driver published.
    pub(crate) fn notification_data(&self) -> Vec<u32> {
        self.log.lock().notification_data.clone()
    }

    /// Every queue programming the driver performed, in order.
    pub(crate) fn programmed_queues(&self) -> Vec<QueueProgramming> {
        self.log.lock().programmed.clone()
    }

    /// Every queue index the driver reset, in order.
    pub(crate) fn queue_resets(&self) -> Vec<u16> {
        self.log.lock().resets.clone()
    }

    /// Raises an interrupt with `status` in the device's interrupt
    /// status register; the driver's acknowledgement reads and clears
    /// it.
    pub(crate) fn raise_interrupt(&self, status: u32) {
        self.log.lock().pending_interrupt |= status;
    }

    /// Number of interrupts the driver acknowledged.
    pub(crate) fn acknowledged_interrupts(&self) -> usize {
        self.log.lock().acknowledged_interrupts
    }

    /// Presets one 32-bit device configuration field.
    pub(crate) fn set_config_u32(&self, offset: usize, value: u32) {
        self.bus.write_u32(offset, value);
    }

    /// Presets one 16-bit device configuration field, which may sit in
    /// either half of a configuration word.
    pub(crate) fn set_config_u16(&self, offset: usize, value: u16) {
        let word = offset & !0x3;
        let shift = (offset & 0x3) * 8;
        let mut current = self.bus.read_u32(word);
        current &= !(0xffff_u32 << shift);
        current |= u32::from(value) << shift;
        self.bus.write_u32(word, current);
    }

    /// Presets what the configuration register file answers for one
    /// `(select, subsel)` pair.
    ///
    /// A pair with no block answers a size of zero, which is how a real
    /// device says it does not support the question.
    pub(crate) fn set_config_block(&self, select: u8, subsel: u8, payload: &[u8]) {
        let mut blocks = self.config_blocks.lock();
        blocks.retain(|(block_select, block_subsel, _)| {
            (*block_select, *block_subsel) != (select, subsel)
        });
        blocks.push((select, subsel, payload.to_vec()));
    }

    /// Answers the currently selected pair, the way a device refills
    /// its configuration structure when the driver writes the
    /// selectors.
    fn publish_selected_block(&self) {
        let blocks = self.config_blocks.lock();
        if blocks.is_empty() {
            return;
        }
        let select = self.bus.read_u8(CFG_SELECT);
        let subsel = self.bus.read_u8(CFG_SUBSEL);
        let payload = blocks
            .iter()
            .find(|(block_select, block_subsel, _)| {
                *block_select == select && *block_subsel == subsel
            })
            .map(|(_, _, payload)| payload.as_slice())
            .unwrap_or(&[]);
        self.bus.write_u8(
            CFG_SIZE,
            u8::try_from(payload.len()).expect("a configuration block fits its size field"),
        );
        for offset in 0..CFG_PAYLOAD_BYTES {
            self.bus.write_u8(
                CFG_PAYLOAD + offset,
                payload.get(offset).copied().unwrap_or(0),
            );
        }
    }

    /// Presets one 8-bit device configuration field.
    pub(crate) fn set_config_u8(&self, offset: usize, value: u8) {
        let word = offset & !0x3;
        let shift = (offset & 0x3) * 8;
        let mut current = self.bus.read_u32(word);
        current &= !(0xff_u32 << shift);
        current |= u32::from(value) << shift;
        self.bus.write_u32(word, current);
    }
}

impl<P: DmaMemory> VirtioTransport for FakeTransport<P> {
    type Bus = HeapBus<P>;

    fn bus(&self) -> &Self::Bus {
        &self.bus
    }

    fn device_type(&self) -> DeviceType {
        self.device_type
    }

    fn reset(&self) {
        let mut log = self.log.lock();
        log.status = 0;
        log.driver_resets += 1;
    }

    fn status(&self) -> DeviceStatus {
        DeviceStatus::from_bits_retain(self.log.lock().status)
    }

    fn set_status(&self, status: DeviceStatus) {
        let mut log = self.log.lock();
        log.status = if log.refuse_features_ok {
            (status - DeviceStatus::FEATURES_OK).bits()
        } else {
            status.bits()
        };
    }

    fn device_features(&self) -> u64 {
        self.offered_features.load(Ordering::Relaxed)
    }

    fn set_driver_features(&self, features: u64) {
        self.log.lock().driver_features = features;
    }

    fn queue_max_size(&self, index: u16) -> u16 {
        if self.absent_queues.contains(&index) {
            return 0;
        }
        self.queue_size
    }

    fn set_queue(
        &self,
        index: u16,
        size: u16,
        descriptor_area: u64,
        driver_area: u64,
        device_area: u64,
    ) {
        self.log.lock().programmed.push(QueueProgramming {
            index,
            size,
            descriptor_area,
            driver_area,
            device_area,
        });
    }

    fn notify_queue(&self, index: u16) {
        self.log.lock().kicks.push(index);
    }

    fn notify_queue_with_data(&self, index: u16, data: u32) {
        let mut log = self.log.lock();
        log.kicks.push(index);
        log.notification_data.push(data);
    }

    fn supports_queue_reset(&self) -> bool {
        self.supports_queue_reset
    }

    fn reset_queue(&self, index: u16) -> IoResult<()> {
        if !self.supports_queue_reset {
            return Err(IoError::Unsupported);
        }
        self.log.lock().resets.push(index);
        Ok(())
    }

    fn ack_interrupt(&self) -> InterruptStatus {
        let mut log = self.log.lock();
        log.acknowledged_interrupts += 1;
        InterruptStatus::from_isr(core::mem::take(&mut log.pending_interrupt))
    }

    fn read_config_u8(&self, offset: usize) -> u8 {
        if !self.config_access_fits(offset, 1) {
            return u8::MAX;
        }
        self.bus.read_u8(offset)
    }

    fn read_config_u16(&self, offset: usize) -> u16 {
        if !self.config_access_fits(offset, 2) {
            return u16::MAX;
        }
        self.bus.read_u16(offset)
    }

    fn read_config_u32(&self, offset: usize) -> u32 {
        if !self.config_access_fits(offset, 4) {
            return u32::MAX;
        }
        self.bus.read_u32(offset)
    }

    fn write_config_u32(&self, offset: usize, value: u32) {
        self.bus.write_u32(offset, value);
    }

    fn write_config_u8(&self, offset: usize, value: u8) {
        self.bus.write_u8(offset, value);
        if offset == CFG_SELECT || offset == CFG_SUBSEL {
            self.publish_selected_block();
        }
    }

    fn shared_memory_region(&self, id: u8) -> Option<helios_hal::iommu::PhysicalRange> {
        self.shared_memory
            .lock()
            .iter()
            .find(|(published, _)| *published == id)
            .map(|(_, region)| *region)
    }
}

/// How long an allocation of a test pool lives, as its caller asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lifetime {
    /// [`DmaMemory::allocate_resident`]: for the life of the device.
    Resident,
    /// [`DmaMemory::allocate`]: carried by a request and given back.
    PerRequest,
}

/// One allocation a test pool served: what a driver asked for, so a test
/// can assert how long its memory lives and which way the device moves it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Allocation {
    pub(crate) lifetime: Lifetime,
    pub(crate) access: DmaAccess,
    pub(crate) bytes: usize,
}

/// The allocations a test pool served, in order.
#[derive(Default)]
struct AllocationLog(Mutex<Vec<Allocation>>);

impl AllocationLog {
    fn record(&self, lifetime: Lifetime, access: DmaAccess, layout: Layout) {
        self.0.lock().push(Allocation {
            lifetime,
            access,
            bytes: layout.size(),
        });
    }

    fn snapshot(&self) -> Vec<Allocation> {
        self.0.lock().clone()
    }
}

/// Where the device-visible window of a [`WindowDmaPool`] starts.
///
/// Any value below 16 TiB keeps the window's 4 KiB frame numbers inside
/// the 32 bits the balloon protocol carries; starting above 4 GiB also
/// keeps a test from mistaking a small offset for a host pointer.
pub(crate) const DEVICE_WINDOW_BASE: u64 = 1 << 32;

/// Where the storage behind a [`WindowDmaPool`] lives and how much of it
/// has been handed out.
struct WindowArena {
    storage: Vec<u8>,
    /// Bytes of the storage, from the page-aligned base, the device
    /// window covers. Storage past it is real memory the device cannot
    /// reach, which lets a test build a slice that straddles the end.
    window: usize,
    next: AtomicUsize,
    allocations: AllocationLog,
}

/// A DMA pool that carves its buffers — ring memory and the pages a
/// test posts alike — out of one arena and publishes them as a compact
/// device address window starting at [`DEVICE_WINDOW_BASE`].
///
/// [`IdentityDmaPool`] hands a device the host heap addresses as they
/// are, which a Linux heap places far above the 32-bit page-frame space
/// some devices count in. Tests of such devices take the memory whose
/// frame numbers matter from this pool instead, and the window keeps
/// those small no matter where the host placed the arena; memory from
/// anywhere else still translates identically. The pool is a bump
/// allocator: nothing is ever freed, which is fine for a test's few
/// kilobytes.
#[derive(Clone)]
pub(crate) struct WindowDmaPool {
    arena: Arc<WindowArena>,
}

pub(crate) struct WindowDmaBuffer {
    ptr: NonNull<u8>,
    len: usize,
    device_address: DeviceAddress,
}

unsafe impl Send for WindowDmaBuffer {}
unsafe impl Sync for WindowDmaBuffer {}

impl WindowDmaPool {
    /// A pool over `size` bytes, page aligned at the start.
    pub(crate) fn new(size: usize) -> Self {
        Self::with_unmapped_tail(size, 0)
    }

    /// A pool whose window covers `size` bytes and is followed by
    /// `tail` bytes of memory the device cannot reach.
    pub(crate) fn with_unmapped_tail(size: usize, tail: usize) -> Self {
        Self {
            arena: Arc::new(WindowArena {
                storage: alloc::vec![0; size + tail + PAGE_ALIGNMENT],
                window: size,
                next: AtomicUsize::new(0),
                allocations: AllocationLog::default(),
            }),
        }
    }

    /// Every allocation the pool served, in order.
    pub(crate) fn allocations(&self) -> Vec<Allocation> {
        self.arena.allocations.snapshot()
    }

    /// A slice of real memory that starts `inside` bytes before the end
    /// of the window and runs `outside` bytes past it.
    pub(crate) fn straddling_the_window_end(&self, inside: usize, outside: usize) -> &[u8] {
        let end = self.arena_base() + self.arena_len();
        let storage_end = self.arena.storage.as_ptr() as usize + self.arena.storage.len();
        assert!(inside <= self.arena_len() && end + outside <= storage_end);
        // SAFETY: the range lies inside `storage`, which the arena owns
        // for as long as `self` lives.
        unsafe { core::slice::from_raw_parts((end - inside) as *const u8, inside + outside) }
    }

    fn arena_base(&self) -> usize {
        (self.arena.storage.as_ptr() as usize).next_multiple_of(PAGE_ALIGNMENT)
    }

    fn arena_len(&self) -> usize {
        self.arena.window
    }

    /// A zeroed, page-aligned buffer of `pages` 4 KiB pages.
    pub(crate) fn pages(&self, pages: usize) -> WindowDmaBuffer {
        let layout = Layout::from_size_align(pages * PAGE_ALIGNMENT, PAGE_ALIGNMENT)
            .expect("a page count small enough for a test");
        self.allocate(layout, DmaAccess::READ | DmaAccess::WRITE)
            .expect("the test arena is large enough for its pages")
    }
}

const PAGE_ALIGNMENT: usize = 4096;

impl WindowDmaPool {
    fn carve(
        &self,
        layout: Layout,
        access: DmaAccess,
        lifetime: Lifetime,
    ) -> Result<WindowDmaBuffer, DmaError> {
        let exhausted = DmaError::Exhausted {
            bytes: layout.size(),
        };
        let base = self.arena_base();
        let mut offset = 0;
        let taken = self.arena.next.try_update(
            core::sync::atomic::Ordering::SeqCst,
            core::sync::atomic::Ordering::SeqCst,
            |next| {
                offset = next.next_multiple_of(layout.align());
                let end = offset.checked_add(layout.size())?;
                (end <= self.arena_len()).then_some(end)
            },
        );
        taken.map_err(|_| exhausted)?;
        let ptr = NonNull::new((base + offset) as *mut u8).ok_or(exhausted)?;
        self.arena.allocations.record(lifetime, access, layout);
        Ok(WindowDmaBuffer {
            ptr,
            len: layout.size(),
            device_address: DeviceAddress::new(DEVICE_WINDOW_BASE + offset as u64),
        })
    }
}

impl DmaMemory for WindowDmaPool {
    type Buffer = WindowDmaBuffer;

    fn allocate_resident(
        &self,
        layout: Layout,
        access: DmaAccess,
    ) -> Result<Self::Buffer, DmaError> {
        self.carve(layout, access, Lifetime::Resident)
    }

    fn allocate(&self, layout: Layout, access: DmaAccess) -> Result<Self::Buffer, DmaError> {
        self.carve(layout, access, Lifetime::PerRequest)
    }

    fn want_space(&self) -> DmaSpaceMark {
        DmaSpaceMark::new(0)
    }

    async fn space_changed(&self, _mark: DmaSpaceMark) {}

    fn short_of_space(&self) -> bool {
        false
    }

    fn space_mark(&self) -> DmaSpaceMark {
        DmaSpaceMark::new(0)
    }

    /// Arena memory translates into the window, and a slice that starts
    /// in the window has to end in it. Everything that starts outside —
    /// a request header a driver builds on its stack, say — keeps its
    /// host address, the way a kernel's linear map would.
    fn device_range(&self, bytes: &[u8]) -> Result<DeviceAddress, DmaError> {
        let host = bytes.as_ptr() as usize;
        match host.checked_sub(self.arena_base()) {
            Some(offset) if offset < self.arena_len() => {
                if bytes.len() > self.arena_len() - offset {
                    return Err(DmaError::Unreachable {
                        address: host,
                        bytes: bytes.len(),
                    });
                }
                Ok(DeviceAddress::new(DEVICE_WINDOW_BASE + offset as u64))
            }
            _ => Ok(DeviceAddress::new(host as u64)),
        }
    }

    /// A run named by its address is already in the window, the way a
    /// guest's own pinned pages reach a real device.
    fn foreign_range(&self, physical: u64, _bytes: u64) -> IoResult<u64> {
        Ok(physical)
    }

    fn addressing(&self) -> DmaAddressing {
        DmaAddressing::Physical
    }
}

/// How much memory a [`ConfinedDmaPool`] carves buffers from. Every
/// driver test's rings, slots and caller buffers fit in it.
const CONFINED_ARENA_BYTES: usize = 4 << 20;

/// Where the storage behind a [`ConfinedDmaPool`] lives and how much has
/// been handed out.
struct ConfinedArena {
    storage: Vec<u8>,
    next: AtomicUsize,
    /// Every buffer that was dropped, as `(start, len)`: memory the pool
    /// would be free to hand out again. The arena never does, so a test
    /// can ask afterwards whether a device wrote into memory its owner had
    /// already given back.
    released: Mutex<Vec<(usize, usize)>>,
    allocations: AllocationLog,
}

/// A DMA pool that reaches only the memory it handed out.
///
/// Rings, control slots and the buffers a test posts all come out of one
/// arena, and a device address is the host address, so the test's device
/// side can read a chain by dereferencing its descriptors. What the pool
/// refuses is what a confined device could not reach: a slice anywhere
/// else, whether a stack array, a heap `Vec` or a future's frame, and one
/// that starts inside the arena and runs past what was allocated. A
/// driver that submits a request header from its future therefore fails
/// its test with [`DmaError::Unreachable`] instead of passing because the
/// host happened to map the stack.
///
/// [`ConfinedDmaPool::foreign_range`] still answers every physical run
/// with itself: pinned runs belong to a guest and have no device window
/// until the pins are mapped per device.
#[derive(Clone)]
pub(crate) struct ConfinedDmaPool {
    arena: Arc<ConfinedArena>,
}

pub(crate) struct ConfinedDmaBuffer {
    ptr: NonNull<u8>,
    len: usize,
    /// Where the buffer reports itself released when it drops.
    arena: Arc<ConfinedArena>,
}

impl Drop for ConfinedDmaBuffer {
    fn drop(&mut self) {
        self.arena
            .released
            .lock()
            .push((self.ptr.as_ptr() as usize, self.len));
    }
}

// SAFETY: the buffer names memory owned by the pool's arena, which the
// test keeps alive for as long as any buffer is in use.
unsafe impl Send for ConfinedDmaBuffer {}
// SAFETY: as above.
unsafe impl Sync for ConfinedDmaBuffer {}

impl ConfinedDmaPool {
    pub(crate) fn new(size: usize) -> Self {
        Self {
            arena: Arc::new(ConfinedArena {
                storage: alloc::vec![0; size + PAGE_ALIGNMENT],
                next: AtomicUsize::new(0),
                released: Mutex::new(Vec::new()),
                allocations: AllocationLog::default(),
            }),
        }
    }

    fn base(&self) -> usize {
        (self.arena.storage.as_ptr() as usize).next_multiple_of(PAGE_ALIGNMENT)
    }

    /// A zeroed buffer of `bytes` bytes the device can reach, for the
    /// memory a caller hands a driver to read from or write into.
    pub(crate) fn buffer(&self, bytes: usize) -> ConfinedDmaBuffer {
        let layout = Layout::from_size_align(bytes, 16).expect("a buffer size a test can ask for");
        self.allocate(layout, DmaAccess::READ | DmaAccess::WRITE)
            .expect("the confined test arena has room for the buffer")
    }

    /// Whether `bytes` lies wholly inside memory this pool handed out.
    pub(crate) fn reaches(&self, bytes: &[u8]) -> bool {
        let start = bytes.as_ptr() as usize;
        let handed_out = self.base() + self.arena.next.load(Ordering::SeqCst);
        start >= self.base()
            && start
                .checked_add(bytes.len())
                .is_some_and(|end| end <= handed_out)
    }

    /// Whether any of the `len` bytes at device address `address` belong
    /// to a buffer that has been dropped: a write there is a write into
    /// memory its owner already gave back.
    pub(crate) fn is_released(&self, address: u64, len: usize) -> bool {
        let start = address as usize;
        let end = start + len;
        self.arena
            .released
            .lock()
            .iter()
            .any(|&(freed, freed_len)| start < freed + freed_len && freed < end)
    }

    /// Plays a device that writes `response` into the writable buffers of
    /// chain `token` of `queue`, after checking that every byte it is
    /// about to write still belongs to a live buffer.
    ///
    /// # Panics
    ///
    /// When a writable descriptor names memory that was released: the
    /// driver let a buffer go while the device could still write it.
    pub(crate) fn respond_into_live_memory<T: VirtioTransport>(
        &self,
        queue: &crate::queue::VirtQueue<T>,
        token: u16,
        response: &[u8],
    ) -> u32 {
        for (address, len, writable) in queue.device_chain(token) {
            if writable {
                assert!(
                    !self.is_released(address, len as usize),
                    "the device would write {len} bytes at {address:#x}, memory that was already released"
                );
            }
        }
        queue.device_respond(token, response)
    }
}

impl ConfinedDmaPool {
    fn carve(
        &self,
        layout: Layout,
        access: DmaAccess,
        lifetime: Lifetime,
    ) -> Result<ConfinedDmaBuffer, DmaError> {
        let exhausted = DmaError::Exhausted {
            bytes: layout.size(),
        };
        let align = layout.align().max(1);
        let mut offset = 0;
        self.arena
            .next
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |next| {
                offset = next.next_multiple_of(align);
                let end = offset.checked_add(layout.size())?;
                (end <= self.arena.storage.len() - PAGE_ALIGNMENT).then_some(end)
            })
            .map_err(|_| exhausted)?;
        let ptr = NonNull::new((self.base() + offset) as *mut u8).ok_or(exhausted)?;
        self.arena.allocations.record(lifetime, access, layout);
        Ok(ConfinedDmaBuffer {
            ptr,
            len: layout.size(),
            arena: self.arena.clone(),
        })
    }

    /// Every allocation the pool served, in order.
    pub(crate) fn allocations(&self) -> Vec<Allocation> {
        self.arena.allocations.snapshot()
    }
}

impl DmaMemory for ConfinedDmaPool {
    type Buffer = ConfinedDmaBuffer;

    fn allocate_resident(
        &self,
        layout: Layout,
        access: DmaAccess,
    ) -> Result<Self::Buffer, DmaError> {
        self.carve(layout, access, Lifetime::Resident)
    }

    fn allocate(&self, layout: Layout, access: DmaAccess) -> Result<Self::Buffer, DmaError> {
        self.carve(layout, access, Lifetime::PerRequest)
    }

    fn want_space(&self) -> DmaSpaceMark {
        DmaSpaceMark::new(0)
    }

    async fn space_changed(&self, _mark: DmaSpaceMark) {}

    fn short_of_space(&self) -> bool {
        false
    }

    fn space_mark(&self) -> DmaSpaceMark {
        DmaSpaceMark::new(0)
    }

    fn device_range(&self, bytes: &[u8]) -> Result<DeviceAddress, DmaError> {
        if self.reaches(bytes) {
            Ok(DeviceAddress::new(bytes.as_ptr() as u64))
        } else {
            Err(DmaError::Unreachable {
                address: bytes.as_ptr() as usize,
                bytes: bytes.len(),
            })
        }
    }

    fn foreign_range(&self, physical: u64, _bytes: u64) -> IoResult<u64> {
        Ok(physical)
    }

    fn addressing(&self) -> DmaAddressing {
        DmaAddressing::Physical
    }
}

impl DmaBuffer for ConfinedDmaBuffer {
    fn device_address(&self) -> DeviceAddress {
        DeviceAddress::new(self.ptr.as_ptr() as u64)
    }

    fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    fn len(&self) -> usize {
        self.len
    }
}

impl ConfinedDmaBuffer {
    /// The buffer as memory a caller can hand a driver.
    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: the buffer owns `len` bytes of the pool's arena and
        // nothing else names them.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

/// A fake transport behind a [`ConfinedDmaPool`].
pub(crate) type ConfinedTransport = FakeTransport<ConfinedDmaPool>;

impl FakeTransport<ConfinedDmaPool> {
    /// A transport whose device reaches only what its own pool handed
    /// out.
    pub(crate) fn confined(config: FakeTransportConfig) -> Self {
        Self::with_dma(config, ConfinedDmaPool::new(CONFINED_ARENA_BYTES))
    }

    /// The pool, for the test's own caller buffers.
    pub(crate) fn pool(&self) -> &ConfinedDmaPool {
        self.bus().dma()
    }
}

impl DmaBuffer for WindowDmaBuffer {
    fn device_address(&self) -> DeviceAddress {
        self.device_address
    }

    fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    fn len(&self) -> usize {
        self.len
    }
}

impl WindowDmaBuffer {
    /// The buffer as guest memory a driver can be handed.
    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl AsRef<[u8]> for ConfinedDmaBuffer {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: the buffer owns `len` bytes of the pool's arena and
        // nothing else names them.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl AsMut<[u8]> for ConfinedDmaBuffer {
    fn as_mut(&mut self) -> &mut [u8] {
        self.as_mut_slice()
    }
}

impl AsRef<[u8]> for WindowDmaBuffer {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: the buffer owns `len` bytes of the pool's arena and
        // nothing else names them.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl AsMut<[u8]> for WindowDmaBuffer {
    fn as_mut(&mut self) -> &mut [u8] {
        self.as_mut_slice()
    }
}
