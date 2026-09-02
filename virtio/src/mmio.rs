use core::ptr::NonNull;

use helios_hal::fs::BlockDeviceRights;
use helios_hal::io::IoResult;

use crate::block::{VirtioBlockDevice, VirtioBlockResource};
use crate::bus::{DmaPool, IdentityDmaPool, MmioBus};
use crate::net::VirtioNetDevice;
use crate::p9::Virtio9pDevice;
use crate::rng::VirtioRngDevice;
use crate::transport::VirtioMmioTransport;

pub type VirtioMmioBlockDevice = VirtioBlockResource<VirtioMmioTransport<MmioBus>>;
pub type VirtioMmioNetDevice = VirtioNetDevice<VirtioMmioTransport<MmioBus>>;
pub type VirtioMmio9pDevice = Virtio9pDevice<VirtioMmioTransport<MmioBus>>;
pub type VirtioMmioRngDevice = VirtioRngDevice<VirtioMmioTransport<MmioBus>>;

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

pub unsafe fn net_from_mmio_with_dma<P>(
    header: NonNull<u8>,
    mmio_size: usize,
    dma: P,
) -> IoResult<VirtioNetDevice<VirtioMmioTransport<MmioBus<P>>>>
where
    P: DmaPool,
{
    let bus = unsafe { MmioBus::new(header, mmio_size, dma) }?;
    let transport = VirtioMmioTransport::new(bus)?;
    VirtioNetDevice::new(transport)
}

/// Builds a VirtIO 9P device from a permanently mapped MMIO header.
///
/// # Safety
///
/// `header..header+mmio_size` must refer to a valid, permanently mapped VirtIO
/// MMIO register block for a 9P device, and no other code may violate the
/// transport's register access invariants while the returned driver is alive.
pub unsafe fn p9_from_mmio(header: NonNull<u8>, mmio_size: usize) -> IoResult<VirtioMmio9pDevice> {
    let bus = unsafe { MmioBus::new(header, mmio_size, IdentityDmaPool) }?;
    let transport = VirtioMmioTransport::new(bus)?;
    Virtio9pDevice::new(transport)
}

pub unsafe fn p9_from_mmio_with_dma<P>(
    header: NonNull<u8>,
    mmio_size: usize,
    dma: P,
) -> IoResult<Virtio9pDevice<VirtioMmioTransport<MmioBus<P>>>>
where
    P: DmaPool,
{
    let bus = unsafe { MmioBus::new(header, mmio_size, dma) }?;
    let transport = VirtioMmioTransport::new(bus)?;
    Virtio9pDevice::new(transport)
}

/// Builds a VirtIO entropy device from a permanently mapped MMIO header.
///
/// # Safety
///
/// `header..header+mmio_size` must refer to a valid, permanently mapped VirtIO
/// MMIO register block for an entropy device, and no other code may violate the
/// transport's register access invariants while the returned driver is alive.
pub unsafe fn rng_from_mmio(
    header: NonNull<u8>,
    mmio_size: usize,
) -> IoResult<VirtioMmioRngDevice> {
    let bus = unsafe { MmioBus::new(header, mmio_size, IdentityDmaPool) }?;
    let transport = VirtioMmioTransport::new(bus)?;
    VirtioRngDevice::new(transport)
}
