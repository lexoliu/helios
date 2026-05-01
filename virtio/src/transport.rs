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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum DeviceType {
    Network = 1,
    Block = 2,
    Console = 3,
    Entropy = 4,
    _9P = 9,
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
    fn ack_interrupt(&self);
    fn read_config_u32(&self, offset: usize) -> u32;

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

        let device_type = match bus.read_u32(REG_DEVICE_ID) {
            1 => DeviceType::Network,
            2 => DeviceType::Block,
            3 => DeviceType::Console,
            4 => DeviceType::Entropy,
            9 => DeviceType::_9P,
            _ => return Err(IoError::Unsupported),
        };

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

    fn ack_interrupt(&self) {
        let status = self.bus.read_u32(REG_INTERRUPT_STATUS);
        if status != 0 {
            self.bus.write_u32(REG_INTERRUPT_ACK, status);
        }
    }

    fn read_config_u32(&self, offset: usize) -> u32 {
        self.bus.read_u32(CONFIG_SPACE_OFFSET + offset)
    }

    fn read_config_u8(&self, offset: usize) -> u8 {
        self.bus.read_u8(CONFIG_SPACE_OFFSET + offset)
    }
}

#[cfg(test)]
mod tests {
    use core::cell::UnsafeCell;

    use super::{
        DeviceStatus, DeviceType, REG_DEVICE_FEATURES, REG_DEVICE_FEATURES_SEL, REG_DEVICE_ID,
        REG_DRIVER_FEATURES, REG_DRIVER_FEATURES_SEL, REG_INTERRUPT_ACK, REG_INTERRUPT_STATUS,
        REG_MAGIC_VALUE, REG_QUEUE_DESC_HIGH, REG_QUEUE_DESC_LOW, REG_QUEUE_DEVICE_HIGH,
        REG_QUEUE_DEVICE_LOW, REG_QUEUE_DRIVER_HIGH, REG_QUEUE_DRIVER_LOW, REG_QUEUE_NOTIFY,
        REG_QUEUE_NUM, REG_QUEUE_NUM_MAX, REG_QUEUE_READY, REG_QUEUE_SEL, REG_STATUS, REG_VERSION,
        VirtioFeatures, VirtioMmioTransport, VirtioTransport,
    };
    use crate::bus::{DeviceBus, IdentityDmaPool};

    struct FakeBus {
        regs: UnsafeCell<[u32; 128]>,
        dma: IdentityDmaPool,
    }

    impl FakeBus {
        fn new() -> Self {
            let mut regs = [0u32; 128];
            regs[REG_MAGIC_VALUE / 4] = 0x7472_6976;
            regs[REG_VERSION / 4] = 2;
            regs[REG_DEVICE_ID / 4] = DeviceType::Block as u32;
            regs[REG_QUEUE_NUM_MAX / 4] = 16;
            regs[0x100 / 4] = 0xfeed_beef;
            Self {
                regs: UnsafeCell::new(regs),
                dma: IdentityDmaPool,
            }
        }

        fn regs(&self) -> &[u32; 128] {
            unsafe { &*self.regs.get() }
        }
    }

    impl DeviceBus for FakeBus {
        type DmaPool = IdentityDmaPool;

        fn read_u32(&self, offset: usize) -> u32 {
            if offset == REG_DEVICE_FEATURES {
                let selector = self.regs()[REG_DEVICE_FEATURES_SEL / 4];
                return match selector {
                    0 => VirtioFeatures::RING_EVENT_IDX.bits() as u32,
                    1 => (VirtioFeatures::VERSION_1.bits() >> 32) as u32,
                    value => panic!("unexpected device feature selector {value}"),
                };
            }

            self.regs()[offset / 4]
        }

        fn write_u32(&self, offset: usize, value: u32) {
            unsafe {
                (*self.regs.get())[offset / 4] = value;
            }
        }

        fn dma(&self) -> &Self::DmaPool {
            &self.dma
        }
    }

    unsafe impl Send for FakeBus {}
    unsafe impl Sync for FakeBus {}

    #[test]
    fn mmio_transport_reads_identity_and_programs_queue() {
        let bus = FakeBus::new();
        let transport = VirtioMmioTransport::new(bus).expect("transport should initialize");

        assert_eq!(transport.device_type(), DeviceType::Block);
        assert_eq!(
            transport.device_features(),
            VirtioFeatures::RING_EVENT_IDX.bits() | VirtioFeatures::VERSION_1.bits()
        );

        transport.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER);
        assert_eq!(transport.bus().regs()[REG_STATUS / 4], 0b11);

        transport.set_driver_features(VirtioFeatures::VERSION_1.bits());
        assert_eq!(transport.bus().regs()[REG_DRIVER_FEATURES_SEL / 4], 1);
        assert_eq!(transport.bus().regs()[REG_DRIVER_FEATURES / 4], 1);

        transport.set_queue(
            0,
            8,
            0x1122_3344_5566_7788,
            0x99aa_bbcc_ddee_ff00,
            0x1234_5678_9abc_def0,
        );
        assert_eq!(transport.bus().regs()[REG_QUEUE_SEL / 4], 0);
        assert_eq!(transport.bus().regs()[REG_QUEUE_NUM / 4], 8);
        assert_eq!(transport.bus().regs()[REG_QUEUE_DESC_LOW / 4], 0x5566_7788);
        assert_eq!(transport.bus().regs()[REG_QUEUE_DESC_HIGH / 4], 0x1122_3344);
        assert_eq!(
            transport.bus().regs()[REG_QUEUE_DRIVER_LOW / 4],
            0xddee_ff00
        );
        assert_eq!(
            transport.bus().regs()[REG_QUEUE_DRIVER_HIGH / 4],
            0x99aa_bbcc
        );
        assert_eq!(
            transport.bus().regs()[REG_QUEUE_DEVICE_LOW / 4],
            0x9abc_def0
        );
        assert_eq!(
            transport.bus().regs()[REG_QUEUE_DEVICE_HIGH / 4],
            0x1234_5678
        );
        assert_eq!(transport.bus().regs()[REG_QUEUE_READY / 4], 1);

        transport.bus().write_u32(REG_INTERRUPT_STATUS, 3);
        transport.ack_interrupt();
        assert_eq!(transport.bus().regs()[REG_INTERRUPT_ACK / 4], 3);

        transport.notify_queue(0);
        assert_eq!(transport.bus().regs()[REG_QUEUE_NOTIFY / 4], 0);

        assert_eq!(transport.read_config_u32(0), 0xfeed_beef);
    }
}
