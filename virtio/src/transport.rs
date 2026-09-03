use bitflags::bitflags;
use helios_hal::io::{IoError, IoResult};

use crate::bus::DeviceBus;

const MAGIC_VALUE: u32 = 0x7472_6976;
const MODERN_VERSION: u32 = 2;

const REG_MAGIC_VALUE: usize = 0x000;
const REG_VERSION: usize = 0x004;
const REG_DEVICE_ID: usize = 0x008;
const REG_DEVICE_FEATURES: usize = 0x010;
const REG_DEVICE_FEATURES_SEL: usize = 0x014;
const REG_DRIVER_FEATURES: usize = 0x020;
const REG_DRIVER_FEATURES_SEL: usize = 0x024;
const REG_QUEUE_SEL: usize = 0x030;
const REG_QUEUE_NUM_MAX: usize = 0x034;
const REG_QUEUE_NUM: usize = 0x038;
const REG_QUEUE_READY: usize = 0x044;
const REG_QUEUE_NOTIFY: usize = 0x050;
const REG_INTERRUPT_STATUS: usize = 0x060;
const REG_INTERRUPT_ACK: usize = 0x064;
const REG_STATUS: usize = 0x070;
const REG_QUEUE_DESC_LOW: usize = 0x080;
const REG_QUEUE_DESC_HIGH: usize = 0x084;
const REG_QUEUE_DRIVER_LOW: usize = 0x090;
const REG_QUEUE_DRIVER_HIGH: usize = 0x094;
const REG_QUEUE_DEVICE_LOW: usize = 0x0a0;
const REG_QUEUE_DEVICE_HIGH: usize = 0x0a4;
const CONFIG_SPACE_OFFSET: usize = 0x100;

/// The virtio device kinds this kernel drives.
///
/// A device id that is absent here is one no Helios driver claims; the
/// transports reject it rather than mapping it to a placeholder. The
/// platform console is a UART on every backend — it has to work before
/// the allocator and on the panic path — so virtio-console is not a
/// device kind this kernel has a driver for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum DeviceType {
    Network = 1,
    Block = 2,
    Entropy = 4,
    MemoryBalloon = 5,
    _9P = 9,
    /// virtio-vsock: the host/guest socket transport the inspector RPC
    /// and the debugger ride on.
    Vsock = 19,
    /// virtio-iommu: the translation unit the platform's confined
    /// devices issue their DMA through.
    Iommu = 23,
}

impl DeviceType {
    /// Maps a virtio device type id onto the device kinds this kernel
    /// drives. Every transport reports the same numbering, so the match
    /// lives here rather than once per transport.
    pub fn from_id(id: u32) -> Option<Self> {
        match id {
            1 => Some(Self::Network),
            2 => Some(Self::Block),
            4 => Some(Self::Entropy),
            5 => Some(Self::MemoryBalloon),
            9 => Some(Self::_9P),
            19 => Some(Self::Vsock),
            23 => Some(Self::Iommu),
            _ => None,
        }
    }
}

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct DeviceStatus: u32 {
        const ACKNOWLEDGE = 1 << 0;
        const DRIVER = 1 << 1;
        const DRIVER_OK = 1 << 2;
        const FEATURES_OK = 1 << 3;
        const FAILED = 1 << 7;
    }
}

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct VirtioFeatures: u64 {
        const RING_INDIRECT_DESC = 1 << 28;
        const RING_EVENT_IDX = 1 << 29;
        const VERSION_1 = 1 << 32;
        /// VIRTIO_F_ACCESS_PLATFORM: the device issues addresses the
        /// platform translates rather than raw physical addresses.
        const ACCESS_PLATFORM = 1 << 33;
        const RING_PACKED = 1 << 34;
        const IN_ORDER = 1 << 35;
        const NOTIFICATION_DATA = 1 << 38;
        const RING_RESET = 1 << 40;
    }
}

/// Why a device raised the interrupt the driver is now servicing.
///
/// virtio has exactly two causes and one status register bit for each
/// (virtio 1.2 §4.1.4.5, §4.2.2): a virtqueue used-buffer notification
/// and a device configuration change. Drivers that own configuration
/// state — virtio-net watches its link status — need the distinction, so
/// acknowledging an interrupt reports what it was for instead of
/// swallowing the register read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterruptStatus {
    /// The device used a buffer on at least one virtqueue.
    pub used_buffer: bool,
    /// The device changed its configuration space.
    pub config_change: bool,
}

/// `VIRTIO_MMIO_INT_VRING` / ISR bit 0: a virtqueue was used.
const ISR_USED_BUFFER: u32 = 1 << 0;
/// `VIRTIO_MMIO_INT_CONFIG` / ISR bit 1: the configuration changed.
const ISR_CONFIG_CHANGE: u32 = 1 << 1;

impl InterruptStatus {
    /// Decodes an interrupt status register value.
    pub const fn from_isr(bits: u32) -> Self {
        Self {
            used_buffer: bits & ISR_USED_BUFFER != 0,
            config_change: bits & ISR_CONFIG_CHANGE != 0,
        }
    }

    /// No cause: the device did not raise this interrupt.
    pub const fn none() -> Self {
        Self {
            used_buffer: false,
            config_change: false,
        }
    }

    /// Both causes are possible and the transport cannot tell them
    /// apart.
    ///
    /// A virtio-PCI function driven through MSI-X does not touch its ISR
    /// register at all — it signals through the vector — and helios binds
    /// the configuration-change vector to the same entry as the
    /// virtqueues, so an MSI-X interrupt could be either. Reporting both
    /// keeps the driver correct: it drains its queues and re-reads the
    /// configuration it cares about, which is a register read, not a
    /// wait.
    pub const fn indistinguishable() -> Self {
        Self {
            used_buffer: true,
            config_change: true,
        }
    }

    /// Whether the device raised the interrupt for any reason at all.
    pub const fn is_pending(self) -> bool {
        self.used_buffer || self.config_change
    }
}

pub trait VirtioTransport: Send + Sync + 'static {
    type Bus: DeviceBus;

    fn bus(&self) -> &Self::Bus;
    fn device_type(&self) -> DeviceType;
    fn reset(&self);
    fn status(&self) -> DeviceStatus;
    fn set_status(&self, status: DeviceStatus);
    fn device_features(&self) -> u64;
    fn set_driver_features(&self, features: u64);
    fn queue_max_size(&self, index: u16) -> u16;
    fn set_queue(
        &self,
        index: u16,
        size: u16,
        descriptor_area: u64,
        driver_area: u64,
        device_area: u64,
    );
    fn notify_queue(&self, index: u16);

    /// Kicks `index` carrying a VIRTIO_F_NOTIFICATION_DATA payload.
    ///
    /// `data` already encodes the queue index in its low half and the
    /// ring position the driver has published up to in its high half;
    /// the transport only has to deliver the whole word to the
    /// notification register.
    fn notify_queue_with_data(&self, index: u16, data: u32);

    /// Whether this transport exposes a per-queue reset register, and
    /// therefore whether VIRTIO_F_RING_RESET may be negotiated at all.
    fn supports_queue_reset(&self) -> bool;

    /// Resets a single virtqueue on the device side.
    ///
    /// On return the device has dropped every buffer the driver made
    /// available on `index` and the queue is disabled; the driver
    /// re-programs it through [`VirtioTransport::set_queue`].
    fn reset_queue(&self, index: u16) -> IoResult<()>;

    /// Acknowledges the pending interrupt and reports what it was for.
    fn ack_interrupt(&self) -> InterruptStatus;

    fn read_config_u32(&self, offset: usize) -> u32;

    /// Writes one 32-bit device configuration field.
    ///
    /// Most device configuration is read-only to the driver; the fields
    /// that are not — virtio-balloon's `actual` — are how the driver
    /// reports its own state back, so the write path belongs to the
    /// transport alongside the read.
    fn write_config_u32(&self, offset: usize, value: u32);

    fn read_config_u8(&self, offset: usize) -> u8 {
        let word_offset = offset & !0x3;
        let byte_index = offset & 0x3;
        self.read_config_u32(word_offset).to_le_bytes()[byte_index]
    }
}

pub struct VirtioMmioTransport<B: DeviceBus> {
    bus: B,
    device_type: DeviceType,
}

impl<B: DeviceBus> VirtioMmioTransport<B> {
    pub fn new(bus: B) -> IoResult<Self> {
        if bus.read_u32(REG_MAGIC_VALUE) != MAGIC_VALUE {
            return Err(IoError::Unsupported);
        }

        if bus.read_u32(REG_VERSION) != MODERN_VERSION {
            return Err(IoError::Unsupported);
        }

        let device_type =
            DeviceType::from_id(bus.read_u32(REG_DEVICE_ID)).ok_or(IoError::Unsupported)?;

        Ok(Self { bus, device_type })
    }

    fn select_queue(&self, index: u16) {
        self.bus.write_u32(REG_QUEUE_SEL, u32::from(index));
    }
}

impl<B: DeviceBus> VirtioTransport for VirtioMmioTransport<B> {
    type Bus = B;

    fn bus(&self) -> &Self::Bus {
        &self.bus
    }

    fn device_type(&self) -> DeviceType {
        self.device_type
    }

    fn reset(&self) {
        self.bus.write_u32(REG_STATUS, 0);
    }

    fn status(&self) -> DeviceStatus {
        DeviceStatus::from_bits_retain(self.bus.read_u32(REG_STATUS))
    }

    fn set_status(&self, status: DeviceStatus) {
        self.bus.write_u32(REG_STATUS, status.bits());
    }

    fn device_features(&self) -> u64 {
        self.bus.write_u32(REG_DEVICE_FEATURES_SEL, 0);
        let low = self.bus.read_u32(REG_DEVICE_FEATURES) as u64;
        self.bus.write_u32(REG_DEVICE_FEATURES_SEL, 1);
        let high = self.bus.read_u32(REG_DEVICE_FEATURES) as u64;
        low | (high << 32)
    }

    fn set_driver_features(&self, features: u64) {
        self.bus.write_u32(REG_DRIVER_FEATURES_SEL, 0);
        self.bus.write_u32(REG_DRIVER_FEATURES, features as u32);
        self.bus.write_u32(REG_DRIVER_FEATURES_SEL, 1);
        self.bus
            .write_u32(REG_DRIVER_FEATURES, (features >> 32) as u32);
    }

    fn queue_max_size(&self, index: u16) -> u16 {
        self.select_queue(index);
        self.bus.read_u32(REG_QUEUE_NUM_MAX) as u16
    }

    fn set_queue(
        &self,
        index: u16,
        size: u16,
        descriptor_area: u64,
        driver_area: u64,
        device_area: u64,
    ) {
        self.select_queue(index);
        self.bus.write_u32(REG_QUEUE_NUM, u32::from(size));
        self.bus
            .write_u32(REG_QUEUE_DESC_LOW, descriptor_area as u32);
        self.bus
            .write_u32(REG_QUEUE_DESC_HIGH, (descriptor_area >> 32) as u32);
        self.bus.write_u32(REG_QUEUE_DRIVER_LOW, driver_area as u32);
        self.bus
            .write_u32(REG_QUEUE_DRIVER_HIGH, (driver_area >> 32) as u32);
        self.bus.write_u32(REG_QUEUE_DEVICE_LOW, device_area as u32);
        self.bus
            .write_u32(REG_QUEUE_DEVICE_HIGH, (device_area >> 32) as u32);
        self.bus.write_u32(REG_QUEUE_READY, 1);
    }

    fn notify_queue(&self, index: u16) {
        self.bus.write_u32(REG_QUEUE_NOTIFY, u32::from(index));
    }

    fn notify_queue_with_data(&self, _index: u16, data: u32) {
        self.bus.write_u32(REG_QUEUE_NOTIFY, data);
    }

    fn supports_queue_reset(&self) -> bool {
        // The virtio-mmio register layout (virtio 1.2 §4.2.2) has no
        // per-queue reset register: QueueReady only gates whether the
        // device may use a queue, and clearing it is not defined to
        // drop the buffers already made available. VIRTIO_F_RING_RESET
        // is therefore unreachable over this transport, and
        // `negotiate` masks the bit out for us.
        false
    }

    fn reset_queue(&self, _index: u16) -> IoResult<()> {
        Err(IoError::Unsupported)
    }

    fn ack_interrupt(&self) -> InterruptStatus {
        let status = self.bus.read_u32(REG_INTERRUPT_STATUS);
        if status != 0 {
            self.bus.write_u32(REG_INTERRUPT_ACK, status);
        }
        InterruptStatus::from_isr(status)
    }

    fn read_config_u32(&self, offset: usize) -> u32 {
        self.bus.read_u32(CONFIG_SPACE_OFFSET + offset)
    }

    fn write_config_u32(&self, offset: usize, value: u32) {
        self.bus.write_u32(CONFIG_SPACE_OFFSET + offset, value);
    }

    fn read_config_u8(&self, offset: usize) -> u8 {
        self.bus.read_u8(CONFIG_SPACE_OFFSET + offset)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DeviceStatus, DeviceType, InterruptStatus, REG_DRIVER_FEATURES, REG_DRIVER_FEATURES_SEL,
        REG_INTERRUPT_ACK, REG_INTERRUPT_STATUS, REG_QUEUE_DESC_HIGH, REG_QUEUE_DESC_LOW,
        REG_QUEUE_DEVICE_HIGH, REG_QUEUE_DEVICE_LOW, REG_QUEUE_DRIVER_HIGH, REG_QUEUE_DRIVER_LOW,
        REG_QUEUE_NOTIFY, REG_QUEUE_NUM, REG_QUEUE_READY, REG_QUEUE_SEL, REG_STATUS,
        VirtioFeatures, VirtioMmioTransport, VirtioTransport,
    };
    use crate::bus::DeviceBus;
    use crate::testing::MmioRegisterBus;

    #[test]
    fn mmio_transport_reads_identity_and_programs_queue() {
        let bus = MmioRegisterBus::new(
            DeviceType::Block,
            VirtioFeatures::RING_EVENT_IDX.bits() | VirtioFeatures::VERSION_1.bits(),
        );
        let transport = VirtioMmioTransport::new(bus).expect("transport should initialize");

        assert_eq!(transport.device_type(), DeviceType::Block);
        assert_eq!(
            transport.device_features(),
            VirtioFeatures::RING_EVENT_IDX.bits() | VirtioFeatures::VERSION_1.bits()
        );

        transport.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER);
        assert_eq!(transport.bus().register(REG_STATUS), 0b11);

        transport.set_driver_features(VirtioFeatures::VERSION_1.bits());
        assert_eq!(transport.bus().register(REG_DRIVER_FEATURES_SEL), 1);
        assert_eq!(transport.bus().register(REG_DRIVER_FEATURES), 1);

        transport.set_queue(
            0,
            8,
            0x1122_3344_5566_7788,
            0x99aa_bbcc_ddee_ff00,
            0x1234_5678_9abc_def0,
        );
        assert_eq!(transport.bus().register(REG_QUEUE_SEL), 0);
        assert_eq!(transport.bus().register(REG_QUEUE_NUM), 8);
        assert_eq!(transport.bus().register(REG_QUEUE_DESC_LOW), 0x5566_7788);
        assert_eq!(transport.bus().register(REG_QUEUE_DESC_HIGH), 0x1122_3344);
        assert_eq!(transport.bus().register(REG_QUEUE_DRIVER_LOW), 0xddee_ff00);
        assert_eq!(transport.bus().register(REG_QUEUE_DRIVER_HIGH), 0x99aa_bbcc);
        assert_eq!(transport.bus().register(REG_QUEUE_DEVICE_LOW), 0x9abc_def0);
        assert_eq!(transport.bus().register(REG_QUEUE_DEVICE_HIGH), 0x1234_5678);
        assert_eq!(transport.bus().register(REG_QUEUE_READY), 1);

        transport.bus().write_u32(REG_INTERRUPT_STATUS, 3);
        assert_eq!(
            transport.ack_interrupt(),
            InterruptStatus {
                used_buffer: true,
                config_change: true,
            }
        );
        assert_eq!(transport.bus().register(REG_INTERRUPT_ACK), 3);

        transport.notify_queue(0);
        assert_eq!(transport.bus().register(REG_QUEUE_NOTIFY), 0);

        // VIRTIO_F_NOTIFICATION_DATA replaces the bare queue index with
        // the index plus the published ring position.
        transport.notify_queue_with_data(0, 0x0007_0000);
        assert_eq!(transport.bus().register(REG_QUEUE_NOTIFY), 0x0007_0000);

        assert_eq!(transport.read_config_u32(0), 0xfeed_beef);
    }

    /// The two interrupt causes have to be told apart: virtio-net
    /// re-reads its link status on a configuration change, and a driver
    /// that treated every interrupt as a used-buffer notification would
    /// never see the link move.
    #[test]
    fn mmio_transport_reports_each_interrupt_cause_separately() {
        let bus = MmioRegisterBus::new(DeviceType::Network, VirtioFeatures::VERSION_1.bits());
        let transport = VirtioMmioTransport::new(bus).expect("transport should initialize");

        assert_eq!(transport.ack_interrupt(), InterruptStatus::none());
        assert_eq!(transport.bus().register(REG_INTERRUPT_ACK), 0);

        transport.bus().write_u32(REG_INTERRUPT_STATUS, 1);
        let status = transport.ack_interrupt();
        assert!(status.used_buffer);
        assert!(!status.config_change);
        assert_eq!(transport.bus().register(REG_INTERRUPT_ACK), 1);

        transport.bus().write_u32(REG_INTERRUPT_STATUS, 2);
        let status = transport.ack_interrupt();
        assert!(!status.used_buffer);
        assert!(status.config_change);
        assert!(status.is_pending());
        assert_eq!(transport.bus().register(REG_INTERRUPT_ACK), 2);
    }

    #[test]
    fn mmio_transport_cannot_reset_a_single_queue() {
        let bus = MmioRegisterBus::new(DeviceType::Block, VirtioFeatures::VERSION_1.bits());
        let transport = VirtioMmioTransport::new(bus).expect("transport should initialize");

        assert!(!transport.supports_queue_reset());
        transport
            .reset_queue(0)
            .expect_err("virtio-mmio defines no per-queue reset register");
    }
}
