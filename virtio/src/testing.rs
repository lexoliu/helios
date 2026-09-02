//! Recording device fakes shared by the virtio unit tests.
//!
//! Two fakes live here. [`MmioRegisterBus`] is a plain register file used
//! to exercise the MMIO transport's register layout. [`FakeTransport`] is
//! a transport that allocates real host memory for the virtqueue rings
//! and records every driver-visible side effect — queue programming,
//! kicks, notification-data payloads and per-queue resets — so the queue
//! tests can assert on them and can play the device side by writing into
//! the rings directly.

use alloc::vec::Vec;
use core::cell::UnsafeCell;

use helios_hal::io::{IoError, IoResult};
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
/// DMA address is their virtual address.
pub(crate) struct HeapBus {
    /// Wide enough for the largest device configuration a driver in this
    /// crate reads (virtio-blk's runs to offset 0x40).
    config: UnsafeCell<[u32; 32]>,
    dma: IdentityDmaPool,
}

impl HeapBus {
    fn new() -> Self {
        Self {
            config: UnsafeCell::new([0; 32]),
            dma: IdentityDmaPool,
        }
    }
}

impl DeviceBus for HeapBus {
    fn read_u32(&self, offset: usize) -> u32 {
        unsafe { (*self.config.get())[offset / 4] }
    }

    fn write_u32(&self, offset: usize, value: u32) {
        unsafe {
            (*self.config.get())[offset / 4] = value;
        }
    }

    type DmaPool = IdentityDmaPool;

    fn dma(&self) -> &Self::DmaPool {
        &self.dma
    }
}

unsafe impl Send for HeapBus {}
unsafe impl Sync for HeapBus {}

/// How a [`FakeTransport`] presents itself to a driver.
pub(crate) struct FakeTransportConfig {
    pub(crate) device_type: DeviceType,
    pub(crate) offered_features: u64,
    pub(crate) queue_size: u16,
    pub(crate) supports_queue_reset: bool,
}

impl Default for FakeTransportConfig {
    fn default() -> Self {
        Self {
            device_type: DeviceType::Block,
            offered_features: crate::transport::VirtioFeatures::VERSION_1.bits(),
            queue_size: 8,
            supports_queue_reset: true,
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
pub(crate) struct FakeTransport {
    bus: HeapBus,
    device_type: DeviceType,
    offered_features: u64,
    queue_size: u16,
    supports_queue_reset: bool,
    log: Mutex<FakeTransportLog>,
}

impl FakeTransport {
    pub(crate) fn new(config: FakeTransportConfig) -> Self {
        Self {
            bus: HeapBus::new(),
            device_type: config.device_type,
            offered_features: config.offered_features,
            queue_size: config.queue_size,
            supports_queue_reset: config.supports_queue_reset,
            log: Mutex::new(FakeTransportLog::default()),
        }
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

impl VirtioTransport for FakeTransport {
    type Bus = HeapBus;

    fn bus(&self) -> &Self::Bus {
        &self.bus
    }

    fn device_type(&self) -> DeviceType {
        self.device_type
    }

    fn reset(&self) {
        self.log.lock().status = 0;
    }

    fn status(&self) -> DeviceStatus {
        DeviceStatus::from_bits_retain(self.log.lock().status)
    }

    fn set_status(&self, status: DeviceStatus) {
        self.log.lock().status = status.bits();
    }

    fn device_features(&self) -> u64 {
        self.offered_features
    }

    fn set_driver_features(&self, features: u64) {
        self.log.lock().driver_features = features;
    }

    fn queue_max_size(&self, _index: u16) -> u16 {
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

    fn read_config_u32(&self, offset: usize) -> u32 {
        self.bus.read_u32(offset)
    }
}
