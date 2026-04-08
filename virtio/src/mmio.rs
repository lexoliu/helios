use core::ptr::NonNull;

use helios_hal::fs::BlockDeviceRights;
use helios_hal::io::IoResult;

use crate::block::{VirtioBlockDevice, VirtioBlockResource};
use crate::bus::{IdentityDmaPool, MmioBus};
use crate::console::VirtioConsoleDevice;
use crate::net::VirtioNetDevice;
use crate::transport::VirtioMmioTransport;

pub type VirtioMmioBlockDevice = VirtioBlockResource<VirtioMmioTransport<MmioBus>>;
pub type VirtioMmioConsoleDevice = VirtioConsoleDevice<VirtioMmioTransport<MmioBus>>;
pub type VirtioMmioNetDevice = VirtioNetDevice<VirtioMmioTransport<MmioBus>>;

/// Builds a VirtIO block resource from a permanently mapped MMIO header.
///
/// The current backend assumes identity-mapped DMA addresses, which matches
/// the bare-metal no-MMU configuration Helios uses today. A future bus-backed
/// driver runtime can swap in a different `DmaPool` without changing the block
/// driver layer.
///
/// # Safety
///
/// `header..header+mmio_size` must refer to a valid, permanently mapped VirtIO
/// MMIO register block for a block device, and no other code may violate the
/// transport's register access invariants while the returned driver is alive.
pub unsafe fn block_from_mmio(
    header: NonNull<u8>,
    mmio_size: usize,
    rights: BlockDeviceRights,
) -> IoResult<VirtioMmioBlockDevice> {
    let bus = unsafe { MmioBus::new(header, mmio_size, IdentityDmaPool) }?;
    let transport = VirtioMmioTransport::new(bus)?;
    VirtioBlockDevice::new_resource(transport, rights)
}

/// Builds a VirtIO console device from a permanently mapped MMIO header.
///
/// # Safety
///
/// `header..header+mmio_size` must refer to a valid, permanently mapped VirtIO
/// MMIO register block for a console device, and no other code may violate the
/// transport's register access invariants while the returned driver is alive.
pub unsafe fn console_from_mmio(
    header: NonNull<u8>,
    mmio_size: usize,
) -> IoResult<VirtioMmioConsoleDevice> {
    let bus = unsafe { MmioBus::new(header, mmio_size, IdentityDmaPool) }?;
    let transport = VirtioMmioTransport::new(bus)?;
    VirtioConsoleDevice::new(transport)
}

/// Builds a VirtIO network device from a permanently mapped MMIO header.
///
/// # Safety
///
/// `header..header+mmio_size` must refer to a valid, permanently mapped VirtIO
/// MMIO register block for a network device, and no other code may violate the
/// transport's register access invariants while the returned driver is alive.
pub unsafe fn net_from_mmio(
    header: NonNull<u8>,
    mmio_size: usize,
) -> IoResult<VirtioMmioNetDevice> {
    let bus = unsafe { MmioBus::new(header, mmio_size, IdentityDmaPool) }?;
    let transport = VirtioMmioTransport::new(bus)?;
    VirtioNetDevice::new(transport)
}
