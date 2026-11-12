//! The backend's half of the kernel's device path.
//!
//! x86-64 publishes no device grants (`docs/device-grants.md`): a PCI
//! function's register file is mapped by its driver and its line is
//! routed through the IDT. What the kernel reaches through these hooks
//! on this backend is pinned, physically contiguous memory shown in an
//! instance's linear memory — a display frame buffer the display engine
//! reads by physical address, and a compositor surface a second
//! instance shows too — and a device region shown the same way, such as
//! a virtio-gpu host blob placed in the host-visible aperture, mapped
//! with the memory type its attributes call for (`crate::memory_type`). All of
//! it is the user address space's own [`AddressSpace`] surface,
//! installed here the moment the address space exists and before any
//! device is discovered.
//!
//! # SMP contract
//!
//! Every hook is one address-space method, under the locks that method
//! takes for every other mutation, and a mutation that removes or
//! narrows a translation shoots down every processor before it returns.
//! The hooks hold no state of their own.

use helios_hal::device::{DeviceRegion, DmaPlacement};
use helios_hal::iommu::PhysicalRange;
use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{AddressSpace, AddressSpaceError, PageFlags, VirtAddr, VirtRange};
use helios_kernel::DeviceVmHooks;

fn allocate_run(bytes: usize, placement: DmaPlacement) -> Result<PhysicalRange, AddressSpaceError> {
    crate::vmm::user_address_space().allocate_run(bytes, placement)
}

fn free_run(run: PhysicalRange, align: u64) -> Result<(), AddressSpaceError> {
    crate::vmm::user_address_space().free_run(run, align)
}

fn substitute_memory(
    virt: VirtRange,
    physical: PhysicalRange,
    flags: PageFlags,
) -> Result<(), AddressSpaceError> {
    crate::vmm::user_address_space().substitute_memory(virt, physical, flags)
}

fn substitute_device(virt: VirtRange, region: DeviceRegion) -> Result<(), AddressSpaceError> {
    crate::vmm::user_address_space().substitute_device(virt, region)
}

fn restore(virt: VirtRange) -> Result<(), AddressSpaceError> {
    crate::vmm::user_address_space().restore(virt)
}

fn withdraw(virt: VirtRange) -> Result<(), AddressSpaceError> {
    crate::vmm::user_address_space().withdraw(virt)
}

fn mapping_granule() -> u64 {
    PhysFrame::SIZE as u64
}

fn kernel_alias(frame: PhysFrame) -> VirtAddr {
    crate::vmm::user_address_space().kernel_alias(frame)
}

static VM_HOOKS: DeviceVmHooks = DeviceVmHooks {
    allocate_run,
    free_run,
    substitute_memory,
    substitute_device,
    restore,
    withdraw,
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
