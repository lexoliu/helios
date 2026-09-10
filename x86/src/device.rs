//! The backend's half of the kernel's device path.
//!
//! x86-64 publishes no device grants (`docs/device-grants.md`): a PCI
//! function's register file is mapped by its driver and its line is
//! routed through the IDT, so nothing maps a device region through the
//! kernel's grant path here, and [`map_device`] and [`unmap_device`]
//! answer exactly that. What the kernel does reach through these hooks
//! on this backend is pinned, physically contiguous memory under an
//! instance's reservation — a display frame buffer the display engine
//! reads by physical address, and a compositor surface a second
//! instance holds a view of — and that is the user address space's own
//! [`AddressSpace`] surface, installed here the moment the address space
//! exists and before any device is discovered.
//!
//! # SMP contract
//!
//! Every hook is one address-space method, under the locks that method
//! takes for every other mutation, and a mutation shoots down every
//! processor before it returns. The hooks hold no state of their own.

use helios_hal::device::{DeviceRegion, DmaPlacement};
use helios_hal::iommu::PhysicalRange;
use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{AddressSpace, AddressSpaceError, PageFlags, VirtAddr, VirtRange};
use helios_kernel::DeviceVmHooks;

fn map_device(_virt: VirtRange, _region: DeviceRegion) -> Result<(), AddressSpaceError> {
    Err(AddressSpaceError::DeviceMappingUnsupported)
}

fn unmap_device(_virt: VirtRange) -> Result<(), AddressSpaceError> {
    Err(AddressSpaceError::DeviceMappingUnsupported)
}

fn map_shared(
    virt: VirtRange,
    physical: PhysicalRange,
    flags: PageFlags,
) -> Result<(), AddressSpaceError> {
    crate::vmm::user_address_space().map_shared(virt, physical, flags)
}

fn unmap_shared(virt: VirtRange) -> Result<(), AddressSpaceError> {
    crate::vmm::user_address_space().unmap_shared(virt)
}

fn commit_contiguous(
    virt: VirtRange,
    flags: PageFlags,
    placement: DmaPlacement,
) -> Result<PhysFrame, AddressSpaceError> {
    crate::vmm::user_address_space().commit_contiguous(virt, flags, placement)
}

fn release_contiguous(virt: VirtRange, align: u64) -> Result<(), AddressSpaceError> {
    crate::vmm::user_address_space().release_contiguous(virt, align)
}

fn mapping_granule() -> u64 {
    PhysFrame::SIZE as u64
}

fn kernel_alias(frame: PhysFrame) -> VirtAddr {
    crate::vmm::user_address_space().kernel_alias(frame)
}

static VM_HOOKS: DeviceVmHooks = DeviceVmHooks {
    map_device,
    unmap_device,
    map_shared,
    unmap_shared,
    commit_contiguous,
    release_contiguous,
    mapping_granule,
    kernel_alias,
};

/// Install the backend's half of the device path.
///
/// Runs right after the user address space exists and before the PCI
/// bus is walked, so that the first instance to pin a frame buffer finds
/// the hooks rather than the registry's bring-up-order panic.
pub(crate) fn install_hooks() {
    helios_kernel::install_device_vm_hooks(&VM_HOOKS);
}
