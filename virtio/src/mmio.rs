use core::ptr::NonNull;

use helios_hal::fs::BlockDeviceRights;
use helios_hal::io::{IoError, IoResult};
use virtio_drivers::Hal;
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};
use virtio_drivers::transport::{DeviceType, Transport};

use crate::block::{VirtioBlockDevice, VirtioBlockResource};
use crate::error::map_mmio_error;

pub type VirtioMmioBlockDevice<H> = VirtioBlockResource<H, MmioTransport<'static>>;

/// Builds a VirtIO block resource from a permanently mapped MMIO header.
///
/// The MMIO region must remain valid for the lifetime of the device, which is
/// why this constructor requires the caller to uphold the `'static` mapping
/// invariant.
pub unsafe fn block_from_mmio<H: Hal>(
    header: NonNull<VirtIOHeader>,
    mmio_size: usize,
    rights: BlockDeviceRights,
) -> IoResult<VirtioMmioBlockDevice<H>> {
    let transport = unsafe { MmioTransport::new(header, mmio_size) }.map_err(map_mmio_error)?;

    if transport.device_type() != DeviceType::Block {
        return Err(IoError::Unsupported);
    }

    VirtioBlockDevice::new_resource(transport, rights)
}
