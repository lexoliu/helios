//! Modern virtio-over-PCI transport (virtio 1.0+).
//!
//! The transport locates the virtio structures through the PCI vendor
//! capability list (`VIRTIO_PCI_CAP_COMMON_CFG`, `..._NOTIFY_CFG`,
//! `..._ISR_CFG`, `..._DEVICE_CFG`), maps the BAR windows they point at
//! through a backend-provided [`PciMmioMapper`], and drives the common
//! configuration structure. Legacy (pre-1.0) virtio-pci is deliberately
//! unsupported: a function that exposes no modern capabilities is
//! rejected instead of being driven through the legacy I/O port layout.
//!
//! Concurrency contract: discovery, BAR mapping and queue programming
//! (`queue_max_size`, `set_queue`) run once during single-processor
//! bring-up. Only `notify_queue`, `ack_interrupt` and the config-space
//! reads are used afterwards, and those touch either per-queue notify
//! addresses or read-to-clear registers, so they need no lock.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU16, Ordering};

use helios_hal::fs::BlockDeviceRights;
use helios_hal::io::{IoError, IoResult};
use pci_types::capability::PciCapability;
use pci_types::{Bar, CommandRegister, ConfigRegionAccess, EndpointHeader, PciAddress, PciHeader};

use crate::block::{QueueAffinity, VirtioBlockDevice, VirtioBlockResource};
use crate::bus::{DeviceBus, DmaPool};
use crate::net::VirtioNetDevice;
use crate::p9::Virtio9pDevice;
use crate::rng::VirtioRngDevice;
use crate::transport::{DeviceStatus, DeviceType, VirtioTransport};

/// PCI vendor id shared by every virtio function.
pub const VIRTIO_PCI_VENDOR_ID: u16 = 0x1af4;

/// Modern functions use device id `0x1040 + virtio device type`.
const MODERN_DEVICE_ID_BASE: u16 = 0x1040;
const MODERN_DEVICE_ID_END: u16 = 0x107f;
/// Transitional functions keep their legacy device id and carry the
/// virtio device type in the PCI subsystem device id instead.
const TRANSITIONAL_DEVICE_ID_BASE: u16 = 0x1000;
const TRANSITIONAL_DEVICE_ID_END: u16 = 0x103f;

const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

/// `struct virtio_pci_cap` field offsets, read as aligned dwords.
const CAP_TYPE_WORD: u16 = 0x00;
const CAP_BAR_WORD: u16 = 0x04;
const CAP_OFFSET_WORD: u16 = 0x08;
const CAP_LENGTH_WORD: u16 = 0x0c;
const CAP_NOTIFY_MULTIPLIER_WORD: u16 = 0x10;

/// `struct virtio_pci_common_cfg` field offsets.
const COMMON_DEVICE_FEATURE_SELECT: usize = 0x00;
const COMMON_DEVICE_FEATURE: usize = 0x04;
const COMMON_DRIVER_FEATURE_SELECT: usize = 0x08;
const COMMON_DRIVER_FEATURE: usize = 0x0c;
const COMMON_CONFIG_MSIX_VECTOR: usize = 0x10;
const COMMON_DEVICE_STATUS: usize = 0x14;
const COMMON_QUEUE_SELECT: usize = 0x16;
const COMMON_QUEUE_SIZE: usize = 0x18;
const COMMON_QUEUE_MSIX_VECTOR: usize = 0x1a;
const COMMON_QUEUE_ENABLE: usize = 0x1c;
const COMMON_QUEUE_NOTIFY_OFF: usize = 0x1e;
const COMMON_QUEUE_DESC: usize = 0x20;
const COMMON_QUEUE_DRIVER: usize = 0x28;
const COMMON_QUEUE_DEVICE: usize = 0x30;
const COMMON_CFG_BYTES: usize = 0x38;
/// `queue_reset` of `struct virtio_pci_modern_common_cfg`, the two-byte
/// field virtio 1.2 appended after `queue_notify_data` for
/// VIRTIO_F_RING_RESET.
const COMMON_QUEUE_RESET: usize = 0x3a;
/// Bytes a common configuration window needs before `queue_reset` may be
/// touched at all.
const COMMON_CFG_RESET_BYTES: usize = COMMON_QUEUE_RESET + 2;

/// `VIRTIO_MSI_NO_VECTOR`: no MSI-X vector is bound to the structure.
const MSI_NO_VECTOR: u16 = 0xffff;

/// Static capacity for the per-queue notification offsets. virtio-net
/// tops out at 16 queue pairs plus a control queue, so 64 covers every
/// device this kernel drives with room to spare.
const MAX_VIRTQUEUES: usize = 64;

/// Backend hook that turns a PCI BAR region into a CPU-visible MMIO
/// pointer.
///
/// This mirrors the role [`crate::bus::DeviceBus`] plays for the MMIO
/// transport: the virtio crate owns the register layout, the backend
/// owns address translation and page-table setup for the window.
///
/// Concurrency contract: called only from device construction on the
/// bootstrap processor, so implementations may take address-space locks.
pub trait PciMmioMapper {
    /// Maps `bytes` starting at the CPU-side address of `bar_address`,
    /// returning a pointer valid for the lifetime of the kernel.
    fn map_region(&self, bar_address: u64, bytes: usize) -> IoResult<NonNull<u8>>;
}

/// A mapped MMIO window inside a virtio-PCI BAR.
struct BarWindow {
    base: NonNull<u8>,
    len: usize,
}

impl BarWindow {
    fn byte_ptr(&self, offset: usize, width: usize) -> *mut u8 {
        let end = offset
            .checked_add(width)
            .unwrap_or_else(|| panic!("virtio-pci window offset {offset:#x} overflowed"));
        assert!(
            end <= self.len,
            "virtio-pci window access at {offset:#x}+{width} exceeds the mapped length {:#x}",
            self.len
        );
        assert!(
            offset.is_multiple_of(width),
            "virtio-pci window access at {offset:#x} is not {width}-byte aligned"
        );
        unsafe { self.base.as_ptr().add(offset) }
    }

    fn read_u8(&self, offset: usize) -> u8 {
        unsafe { self.byte_ptr(offset, 1).read_volatile() }
    }

    fn read_u16(&self, offset: usize) -> u16 {
        unsafe { self.byte_ptr(offset, 2).cast::<u16>().read_volatile() }
    }

    fn read_u32(&self, offset: usize) -> u32 {
        unsafe { self.byte_ptr(offset, 4).cast::<u32>().read_volatile() }
    }

    fn write_u8(&self, offset: usize, value: u8) {
        unsafe { self.byte_ptr(offset, 1).write_volatile(value) }
    }

    fn write_u16(&self, offset: usize, value: u16) {
        unsafe { self.byte_ptr(offset, 2).cast::<u16>().write_volatile(value) }
    }

    fn write_u32(&self, offset: usize, value: u32) {
        unsafe { self.byte_ptr(offset, 4).cast::<u32>().write_volatile(value) }
    }

    /// Writes a 64-bit common-configuration field as the two 32-bit
    /// halves the virtio specification allows, low half first.
    fn write_u64_halves(&self, offset: usize, value: u64) {
        self.write_u32(offset, value as u32);
        self.write_u32(offset + 4, (value >> 32) as u32);
    }
}

unsafe impl Send for BarWindow {}
unsafe impl Sync for BarWindow {}

/// Device-side view a virtio driver gets from a PCI function: the
/// device-specific configuration window plus the DMA pool.
pub struct VirtioPciBus<P: DmaPool> {
    device_config: Option<BarWindow>,
    dma: P,
}

impl<P: DmaPool> VirtioPciBus<P> {
    fn config(&self) -> &BarWindow {
        self.device_config.as_ref().unwrap_or_else(|| {
            panic!("virtio-pci function exposes no device configuration capability")
        })
    }
}

impl<P: DmaPool> DeviceBus for VirtioPciBus<P> {
    type DmaPool = P;

    fn read_u8(&self, offset: usize) -> u8 {
        self.config().read_u8(offset)
    }

    fn read_u32(&self, offset: usize) -> u32 {
        self.config().read_u32(offset)
    }

    fn write_u32(&self, offset: usize, value: u32) {
        self.config().write_u32(offset, value);
    }

    fn dma(&self) -> &Self::DmaPool {
        &self.dma
    }
}

/// Modern virtio-PCI transport over one PCI function.
pub struct VirtioPciTransport<P: DmaPool> {
    bus: VirtioPciBus<P>,
    common: BarWindow,
    notify: BarWindow,
    isr: BarWindow,
    notify_off_multiplier: u32,
    device_type: DeviceType,
    msix_vector: Option<u16>,
    queue_notify_offsets: [AtomicU16; MAX_VIRTQUEUES],
}

/// The virtio device type a PCI function implements, or `None` when the
/// function is not a virtio device.
pub fn virtio_pci_device_type<A: ConfigRegionAccess>(
    access: &A,
    address: PciAddress,
) -> Option<DeviceType> {
    let header = PciHeader::new(address);
    let (vendor_id, device_id) = header.id(access);
    if vendor_id != VIRTIO_PCI_VENDOR_ID {
        return None;
    }
    let endpoint = EndpointHeader::from_header(header, access)?;
    let (subsystem_id, _) = endpoint.subsystem(access);
    device_type_from_ids(device_id, subsystem_id)
}

fn device_type_from_ids(device_id: u16, subsystem_id: u16) -> Option<DeviceType> {
    let raw = match device_id {
        MODERN_DEVICE_ID_BASE..=MODERN_DEVICE_ID_END => device_id - MODERN_DEVICE_ID_BASE,
        TRANSITIONAL_DEVICE_ID_BASE..=TRANSITIONAL_DEVICE_ID_END => subsystem_id,
        _ => return None,
    };
    DeviceType::from_id(u32::from(raw))
}

/// One virtio structure capability parsed out of the PCI capability list.
#[derive(Clone, Copy)]
struct VirtioCapability {
    bar: u8,
    offset: u32,
    length: u32,
}

#[derive(Default)]
struct VirtioCapabilities {
    common: Option<VirtioCapability>,
    notify: Option<VirtioCapability>,
    isr: Option<VirtioCapability>,
    device: Option<VirtioCapability>,
    notify_off_multiplier: Option<u32>,
}

impl<P: DmaPool> VirtioPciTransport<P> {
    /// Brings up a modern virtio-PCI function.
    ///
    /// `msix_vector` binds the device configuration and every virtqueue
    /// to one MSI-X table entry the backend has already programmed. Pass
    /// `None` to leave the function on its INTx pin.
    pub fn new<A, M>(
        access: &A,
        address: PciAddress,
        mapper: &M,
        dma: P,
        msix_vector: Option<u16>,
    ) -> IoResult<Self>
    where
        A: ConfigRegionAccess,
        M: PciMmioMapper,
    {
        let header = PciHeader::new(address);
        let (vendor_id, device_id) = header.id(access);
        if vendor_id != VIRTIO_PCI_VENDOR_ID {
            return Err(IoError::Unsupported);
        }
        let mut endpoint =
            EndpointHeader::from_header(header, access).ok_or(IoError::Unsupported)?;
        let (subsystem_id, _) = endpoint.subsystem(access);
        let device_type =
            device_type_from_ids(device_id, subsystem_id).ok_or(IoError::Unsupported)?;

        // BAR sizing writes all-ones probe values, so decode has to be
        // off while the capabilities and BARs are read.
        endpoint.update_command(access, |command| {
            (command | CommandRegister::INTERRUPT_DISABLE)
                & !(CommandRegister::MEMORY_ENABLE | CommandRegister::IO_ENABLE)
        });

        let capabilities = parse_capabilities(access, &endpoint);
        let common = capabilities
            .common
            .ok_or(IoError::InvalidDeviceConfig(
                "virtio-pci function exposes no common configuration capability",
            ))
            .and_then(|capability| map_capability(access, &endpoint, mapper, capability))?;
        if common.len < COMMON_CFG_BYTES {
            return Err(IoError::InvalidDeviceConfig(
                "virtio-pci common configuration window is shorter than the modern layout",
            ));
        }
        let notify = capabilities
            .notify
            .ok_or(IoError::InvalidDeviceConfig(
                "virtio-pci function exposes no notification capability",
            ))
            .and_then(|capability| map_capability(access, &endpoint, mapper, capability))?;
        let notify_off_multiplier =
            capabilities
                .notify_off_multiplier
                .ok_or(IoError::InvalidDeviceConfig(
                    "virtio-pci notification capability is missing its offset multiplier",
                ))?;
        let isr = capabilities
            .isr
            .ok_or(IoError::InvalidDeviceConfig(
                "virtio-pci function exposes no interrupt status capability",
            ))
            .and_then(|capability| map_capability(access, &endpoint, mapper, capability))?;
        // Device-specific configuration is read through the shared
        // `read_config_u32` contract, so the window has to cover a whole
        // number of dwords even when the device declares a shorter
        // struct (virtio-net stops at `max_virtqueue_pairs`, two bytes
        // into the dword the driver reads it from).
        let device_config = capabilities
            .device
            .map(|capability| VirtioCapability {
                length: capability.length.next_multiple_of(4),
                ..capability
            })
            .map(|capability| map_capability(access, &endpoint, mapper, capability))
            .transpose()?;

        // Memory decode and bus mastering are both required before the
        // device may read the descriptor rings out of guest memory.
        endpoint.update_command(access, |command| {
            let command =
                command | CommandRegister::MEMORY_ENABLE | CommandRegister::BUS_MASTER_ENABLE;
            if msix_vector.is_some() {
                command | CommandRegister::INTERRUPT_DISABLE
            } else {
                command & !CommandRegister::INTERRUPT_DISABLE
            }
        });

        Ok(Self {
            bus: VirtioPciBus { device_config, dma },
            common,
            notify,
            isr,
            notify_off_multiplier,
            device_type,
            msix_vector,
            queue_notify_offsets: [const { AtomicU16::new(0) }; MAX_VIRTQUEUES],
        })
    }

    fn select_queue(&self, index: u16) {
        self.common.write_u16(COMMON_QUEUE_SELECT, index);
    }

    /// Byte offset of `index`'s doorbell inside the notification window.
    fn notify_offset(&self, index: u16) -> usize {
        let notify_off = u64::from(self.notify_slot(index).load(Ordering::Acquire));
        let offset = notify_off * u64::from(self.notify_off_multiplier);
        usize::try_from(offset).unwrap_or_else(|_| {
            panic!("virtio-pci notify offset {offset:#x} does not fit in usize")
        })
    }

    fn notify_slot(&self, index: u16) -> &AtomicU16 {
        self.queue_notify_offsets
            .get(usize::from(index))
            .unwrap_or_else(|| {
                panic!(
                    "virtio-pci queue index {index} exceeds the transport capacity {MAX_VIRTQUEUES}"
                )
            })
    }
}

impl<P: DmaPool> VirtioTransport for VirtioPciTransport<P> {
    type Bus = VirtioPciBus<P>;

    fn bus(&self) -> &Self::Bus {
        &self.bus
    }

    fn device_type(&self) -> DeviceType {
        self.device_type
    }

    fn reset(&self) {
        self.common.write_u8(COMMON_DEVICE_STATUS, 0);
        // The reset completes when the device reads back status 0. It
        // is a register handshake inside the device model, not a wait
        // on software state, so a spin is the correct primitive here.
        while self.common.read_u8(COMMON_DEVICE_STATUS) != 0 {
            core::hint::spin_loop();
        }
        // A reset clears the MSI-X bindings, so re-publish the
        // configuration-change vector as soon as the device is idle;
        // per-queue vectors follow in `set_queue`.
        self.common.write_u16(
            COMMON_CONFIG_MSIX_VECTOR,
            self.msix_vector.unwrap_or(MSI_NO_VECTOR),
        );
    }

    fn status(&self) -> DeviceStatus {
        DeviceStatus::from_bits_retain(u32::from(self.common.read_u8(COMMON_DEVICE_STATUS)))
    }

    fn set_status(&self, status: DeviceStatus) {
        let bits = u8::try_from(status.bits())
            .unwrap_or_else(|_| panic!("virtio device status {status:?} does not fit in a byte"));
        self.common.write_u8(COMMON_DEVICE_STATUS, bits);
    }

    fn device_features(&self) -> u64 {
        self.common.write_u32(COMMON_DEVICE_FEATURE_SELECT, 0);
        let low = u64::from(self.common.read_u32(COMMON_DEVICE_FEATURE));
        self.common.write_u32(COMMON_DEVICE_FEATURE_SELECT, 1);
        let high = u64::from(self.common.read_u32(COMMON_DEVICE_FEATURE));
        low | (high << 32)
    }

    fn set_driver_features(&self, features: u64) {
        self.common.write_u32(COMMON_DRIVER_FEATURE_SELECT, 0);
        self.common
            .write_u32(COMMON_DRIVER_FEATURE, features as u32);
        self.common.write_u32(COMMON_DRIVER_FEATURE_SELECT, 1);
        self.common
            .write_u32(COMMON_DRIVER_FEATURE, (features >> 32) as u32);
    }

    fn queue_max_size(&self, index: u16) -> u16 {
        self.select_queue(index);
        self.common.read_u16(COMMON_QUEUE_SIZE)
    }

    fn set_queue(
        &self,
        index: u16,
        size: u16,
        descriptor_area: u64,
        driver_area: u64,
        device_area: u64,
    ) {
        let slot = self.notify_slot(index);
        self.select_queue(index);
        self.common.write_u16(COMMON_QUEUE_SIZE, size);
        self.common
            .write_u64_halves(COMMON_QUEUE_DESC, descriptor_area);
        self.common
            .write_u64_halves(COMMON_QUEUE_DRIVER, driver_area);
        self.common
            .write_u64_halves(COMMON_QUEUE_DEVICE, device_area);
        self.common.write_u16(
            COMMON_QUEUE_MSIX_VECTOR,
            self.msix_vector.unwrap_or(MSI_NO_VECTOR),
        );
        if let Some(vector) = self.msix_vector {
            assert_eq!(
                self.common.read_u16(COMMON_QUEUE_MSIX_VECTOR),
                vector,
                "virtio-pci device rejected MSI-X vector {vector} for queue {index}"
            );
        }
        let notify_off = self.common.read_u16(COMMON_QUEUE_NOTIFY_OFF);
        let notify_offset = u64::from(notify_off) * u64::from(self.notify_off_multiplier);
        let notify_offset = usize::try_from(notify_offset).unwrap_or_else(|_| {
            panic!(
                "virtio-pci queue {index} notify offset {notify_offset:#x} does not fit in usize"
            )
        });
        assert!(
            notify_offset
                .checked_add(core::mem::size_of::<u16>())
                .is_some_and(|end| end <= self.notify.len),
            "virtio-pci queue {index} notify offset {notify_offset:#x} is outside the mapped notify window"
        );
        slot.store(notify_off, Ordering::Release);
        self.common.write_u16(COMMON_QUEUE_ENABLE, 1);
    }

    fn notify_queue(&self, index: u16) {
        self.notify.write_u16(self.notify_offset(index), index);
    }

    fn notify_queue_with_data(&self, index: u16, data: u32) {
        self.notify.write_u32(self.notify_offset(index), data);
    }

    fn supports_queue_reset(&self) -> bool {
        self.common.len >= COMMON_CFG_RESET_BYTES
    }

    fn reset_queue(&self, index: u16) -> IoResult<()> {
        if !self.supports_queue_reset() {
            return Err(IoError::Unsupported);
        }
        self.select_queue(index);
        self.common.write_u16(COMMON_QUEUE_RESET, 1);
        // Both polls are register handshakes inside the device model,
        // not waits on software state, so spinning is the right
        // primitive: the device clears queue_reset once it has dropped
        // the buffers and then reports the queue as disabled.
        while self.common.read_u16(COMMON_QUEUE_RESET) != 0 {
            core::hint::spin_loop();
        }
        while self.common.read_u16(COMMON_QUEUE_ENABLE) != 0 {
            core::hint::spin_loop();
        }
        Ok(())
    }

    fn ack_interrupt(&self) {
        // The ISR status byte is read-to-clear and is what an INTx-driven
        // function needs to deassert its pin. MSI-X functions report 0
        // here, so the read is harmless either way.
        let _ = self.isr.read_u8(0);
    }

    fn read_config_u32(&self, offset: usize) -> u32 {
        self.bus.read_u32(offset)
    }

    fn read_config_u8(&self, offset: usize) -> u8 {
        self.bus.read_u8(offset)
    }
}

fn parse_capabilities<A: ConfigRegionAccess>(
    access: &A,
    endpoint: &EndpointHeader,
) -> VirtioCapabilities {
    let mut capabilities = VirtioCapabilities::default();
    for capability in endpoint.capabilities(access) {
        let PciCapability::Vendor(location) = capability else {
            continue;
        };
        let address = location.address;
        let offset = location.offset;
        let type_word = unsafe { access.read(address, offset + CAP_TYPE_WORD) };
        let cfg_type = (type_word >> 24) as u8;
        let bar = unsafe { access.read(address, offset + CAP_BAR_WORD) } as u8;
        let region = VirtioCapability {
            bar,
            offset: unsafe { access.read(address, offset + CAP_OFFSET_WORD) },
            length: unsafe { access.read(address, offset + CAP_LENGTH_WORD) },
        };
        match cfg_type {
            VIRTIO_PCI_CAP_COMMON_CFG => capabilities.common = Some(region),
            VIRTIO_PCI_CAP_NOTIFY_CFG => {
                capabilities.notify = Some(region);
                capabilities.notify_off_multiplier =
                    Some(unsafe { access.read(address, offset + CAP_NOTIFY_MULTIPLIER_WORD) });
            }
            VIRTIO_PCI_CAP_ISR_CFG => capabilities.isr = Some(region),
            VIRTIO_PCI_CAP_DEVICE_CFG => capabilities.device = Some(region),
            _ => {}
        }
    }
    capabilities
}

fn map_capability<A, M>(
    access: &A,
    endpoint: &EndpointHeader,
    mapper: &M,
    capability: VirtioCapability,
) -> IoResult<BarWindow>
where
    A: ConfigRegionAccess,
    M: PciMmioMapper,
{
    let bar = endpoint
        .bar(capability.bar, access)
        .ok_or(IoError::InvalidDeviceConfig(
            "virtio-pci capability points at an unimplemented BAR",
        ))?;
    let (bar_address, bar_size) = match bar {
        Bar::Memory32 { address, size, .. } => (u64::from(address), u64::from(size)),
        Bar::Memory64 { address, size, .. } => (address, size),
        Bar::Io { .. } => {
            return Err(IoError::InvalidDeviceConfig(
                "virtio-pci capability points at an I/O BAR; modern transports are memory mapped",
            ));
        }
    };
    if bar_address == 0 {
        // Helios boots through UEFI firmware that assigns every BAR
        // before handing control over; an unassigned BAR means the
        // platform handed us a device it never enumerated.
        return Err(IoError::InvalidDeviceConfig(
            "virtio-pci BAR was left unassigned by platform firmware",
        ));
    }
    let end = u64::from(capability.offset)
        .checked_add(u64::from(capability.length))
        .ok_or(IoError::InvalidDeviceConfig(
            "virtio-pci capability window overflowed its BAR",
        ))?;
    if end > bar_size {
        return Err(IoError::InvalidDeviceConfig(
            "virtio-pci capability window extends past the end of its BAR",
        ));
    }
    let length = usize::try_from(capability.length).map_err(|_| IoError::OutOfBounds)?;
    if length == 0 {
        return Err(IoError::InvalidDeviceConfig(
            "virtio-pci capability declared a zero-length window",
        ));
    }
    let base = mapper.map_region(bar_address + u64::from(capability.offset), length)?;
    Ok(BarWindow { base, len: length })
}

/// Builds a virtio-net driver on top of a modern virtio-PCI function.
pub fn net_from_pci<A, M, P>(
    access: &A,
    address: PciAddress,
    mapper: &M,
    dma: P,
    msix_vector: Option<u16>,
) -> IoResult<VirtioNetDevice<VirtioPciTransport<P>>>
where
    A: ConfigRegionAccess,
    M: PciMmioMapper,
    P: DmaPool,
{
    let transport = VirtioPciTransport::new(access, address, mapper, dma, msix_vector)?;
    VirtioNetDevice::new(transport)
}

/// Builds a virtio-9p driver on top of a modern virtio-PCI function.
pub fn p9_from_pci<A, M, P>(
    access: &A,
    address: PciAddress,
    mapper: &M,
    dma: P,
    msix_vector: Option<u16>,
) -> IoResult<Virtio9pDevice<VirtioPciTransport<P>>>
where
    A: ConfigRegionAccess,
    M: PciMmioMapper,
    P: DmaPool,
{
    let transport = VirtioPciTransport::new(access, address, mapper, dma, msix_vector)?;
    Virtio9pDevice::new(transport)
}

/// Builds a virtio-blk driver on top of a modern virtio-PCI function.
pub fn block_from_pci<A, M, P, C>(
    access: &A,
    address: PciAddress,
    mapper: &M,
    dma: P,
    msix_vector: Option<u16>,
    cpu: C,
    rights: BlockDeviceRights,
) -> IoResult<VirtioBlockResource<VirtioPciTransport<P>, C>>
where
    A: ConfigRegionAccess,
    M: PciMmioMapper,
    P: DmaPool,
    C: QueueAffinity,
{
    let transport = VirtioPciTransport::new(access, address, mapper, dma, msix_vector)?;
    VirtioBlockDevice::new_resource(transport, cpu, rights)
}

/// Builds a virtio-entropy driver on top of a modern virtio-PCI function.
pub fn rng_from_pci<A, M, P>(
    access: &A,
    address: PciAddress,
    mapper: &M,
    dma: P,
    msix_vector: Option<u16>,
) -> IoResult<VirtioRngDevice<VirtioPciTransport<P>>>
where
    A: ConfigRegionAccess,
    M: PciMmioMapper,
    P: DmaPool,
{
    let transport = VirtioPciTransport::new(access, address, mapper, dma, msix_vector)?;
    VirtioRngDevice::new(transport)
}

#[cfg(test)]
mod tests {
    use super::{MODERN_DEVICE_ID_BASE, device_type_from_ids};
    use crate::transport::DeviceType;

    #[test]
    fn modern_device_ids_encode_the_virtio_type() {
        assert_eq!(
            device_type_from_ids(MODERN_DEVICE_ID_BASE + 1, 0),
            Some(DeviceType::Network)
        );
        assert_eq!(
            device_type_from_ids(MODERN_DEVICE_ID_BASE + 9, 0),
            Some(DeviceType::_9P)
        );
    }

    #[test]
    fn transitional_device_ids_take_the_type_from_the_subsystem_id() {
        assert_eq!(device_type_from_ids(0x1000, 1), Some(DeviceType::Network));
        assert_eq!(device_type_from_ids(0x1009, 9), Some(DeviceType::_9P));
    }

    #[test]
    fn non_virtio_device_ids_are_rejected() {
        assert_eq!(device_type_from_ids(0x2000, 1), None);
        assert_eq!(device_type_from_ids(MODERN_DEVICE_ID_BASE + 0x20, 0), None);
    }
}
