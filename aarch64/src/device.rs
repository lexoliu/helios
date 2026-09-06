//! Handing this machine's unclaimed hardware to user-mode drivers.
//!
//! The backend's whole part in the device path is here: two tables of
//! function pointers that let the kernel map a register window and hold
//! an interrupt line off, and a walk that turns what the firmware
//! described into published grants. Nothing about drivers, plugins or
//! Wasmtime appears in this file — a grant is a capability over
//! hardware, and who ends up holding it is the kernel's business.
//!
//! # Concurrency contract
//!
//! Both tables are installed once, on the bootstrap processor, before
//! any secondary is started.
//!
//! [`mask`] runs in interrupt context and takes the GIC's distributor
//! lock, and [`unmask`] takes the same lock from an ordinary task with
//! interrupts enabled. Those two are the deadlock: without care, a
//! granted-device interrupt arriving on the processor whose task holds
//! the lock would spin on it forever. `Gic::with_registers` is what
//! rules that out — every taker of the lock holds it inside a critical
//! section, so no interrupt reaches the holding processor while it is
//! held. See the concurrency contract in `gic.rs`.

use alloc::vec::Vec;

use arm_gic::IntId;
use helios_hal::device::{DeviceRegion, DeviceRegionAttributes, DmaCapability, DmaPlacement};
use helios_hal::iommu::{DmaTranslation, PhysicalRange};
use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{AddressSpace, AddressSpaceError, PageFlags, VirtRange};
use helios_kernel::{
    DEFAULT_DMA_BUDGET_BYTES, DeviceGrant, DeviceGrantRegistry, DeviceInterruptHooks,
    DeviceInterruptRoute, DeviceName, DeviceVmHooks, DmaBudget, GrantError, GrantInterrupt,
};
use spin::Once;

use crate::gic::Gic;
use crate::platform::PlatformDescription;

/// The controller the masking hooks drive.
///
/// The hooks are plain function pointers, by design — see
/// `DeviceVmHooks` — so the controller they act on is reached through
/// a write-once cell rather than carried in a closure. It is the only
/// global this module has, and it exists for exactly this reason.
static CONTROLLER: Once<&'static Gic> = Once::new();

fn controller() -> &'static Gic {
    CONTROLLER
        .get()
        .copied()
        .expect("a granted device's interrupt was masked before the controller was published")
}

fn map_device(virt: VirtRange, region: DeviceRegion) -> Result<(), AddressSpaceError> {
    crate::vmm::user_address_space().map_device(virt, region)
}

fn unmap_device(virt: VirtRange) -> Result<(), AddressSpaceError> {
    crate::vmm::user_address_space().unmap_device(virt)
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

/// This backend maps at 4 KiB, the architecture's smallest granule and
/// the one the kernel's page tables are built with.
fn mapping_granule() -> u64 {
    PhysFrame::SIZE as u64
}

static VM_HOOKS: DeviceVmHooks = DeviceVmHooks {
    map_device,
    unmap_device,
    commit_contiguous,
    release_contiguous,
    mapping_granule,
};

fn mask(source: u32) {
    controller().set_device_interrupt_enabled(IntId::spi(source), false);
}

fn unmask(source: u32) {
    controller().set_device_interrupt_enabled(IntId::spi(source), true);
}

static INTERRUPT_HOOKS: DeviceInterruptHooks = DeviceInterruptHooks { mask, unmask };

/// Install the backend's half of the device path.
///
/// Runs before any grant is published, which the registry checks: a
/// grant claimable before the hooks existed would be a driver holding a
/// device the kernel cannot map.
pub(crate) fn install_hooks(gic: &'static Gic) {
    CONTROLLER.call_once(|| gic);
    helios_kernel::install_device_vm_hooks(&VM_HOOKS);
    helios_kernel::install_device_interrupt_hooks(&INTERRUPT_HOOKS);
}

/// Publish a grant for every device the firmware described and no
/// kernel driver claimed, and return the interrupt routes they need.
///
/// Each interrupt is routed at the GIC and then left masked. Nothing
/// owns the device yet, so a line arriving now would have nowhere to
/// go; the first `unmask` from the driver that claims it is what arms
/// the hardware.
pub(crate) fn publish_grants(
    platform: &PlatformDescription,
    registry: &DeviceGrantRegistry,
    bootstrap_mpidr: u64,
) -> Vec<(IntId, DeviceInterruptRoute)> {
    let mut grants = Vec::new();
    for device in platform.grantable.iter() {
        match build_grant(&device) {
            Ok(grant) => grants.push(grant),
            // A device this backend cannot express as a grant is
            // reported and skipped. The machine still boots and the log
            // names the device and the reason, rather than the kernel
            // refusing to start over hardware nothing needs yet.
            Err(error) => {
                tracing::warn!(
                    node = device.name,
                    %error,
                    "the platform describes a device that cannot be granted"
                );
            }
        }
    }
    if grants.is_empty() {
        return Vec::new();
    }
    registry.publish(grants).unwrap_or_else(|error| {
        panic!("AArch64 could not publish the machine's device grants: {error}")
    });

    let mut routes = Vec::new();
    for device in platform.grantable.iter() {
        let Some(published) = registry.device(device.name) else {
            continue;
        };
        let intid = device.interrupt.intid();
        controller().enable_device_interrupt(intid, device.interrupt.trigger, bootstrap_mpidr);
        controller().set_device_interrupt_enabled(intid, false);
        routes.push((
            intid,
            DeviceInterruptRoute::new(
                published.clone(),
                GrantInterrupt::new(device.interrupt.number),
            ),
        ));
    }
    routes
}

/// Turn one described device into a grant.
fn build_grant(device: &crate::platform::GrantableDevice) -> Result<DeviceGrant, GrantError> {
    let name = DeviceName::new(device.name)?;
    let grant = DeviceGrant::new(
        name,
        DmaBudget {
            capability: DmaCapability {
                // A device-tree node that declares no `dma-ranges` is
                // saying the device masters the processor's own
                // physical address space, which on this architecture is
                // as far as the address lines reach.
                address_bits: u64::BITS,
                coherent: device.coherent,
                translation: DmaTranslation::direct(),
            },
            byte_budget: DEFAULT_DMA_BUDGET_BYTES,
        },
    )
    .with_region(DeviceRegion {
        physical: PhysicalRange {
            start: device.region.base as u64,
            bytes: device.region.size as u64,
        },
        // Every window a device-tree `reg` names is a register file
        // until something says otherwise. A prefetchable aperture is
        // described by a bus binding rather than by a bare `reg`, and
        // this walk does not decode one.
        attributes: DeviceRegionAttributes::REGISTERS,
    })?
    .with_interrupt(GrantInterrupt::new(device.interrupt.number))?;
    Ok(grant)
}
