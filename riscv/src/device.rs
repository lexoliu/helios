//! Handing this machine's unclaimed hardware to user-mode drivers.
//!
//! The backend's whole part in the device path is here: two tables of
//! function pointers that let the kernel map a register window and hold
//! an interrupt source off, and a device-tree walk that turns what the
//! firmware described into published grants. Nothing about drivers,
//! plugins or Wasmtime appears in this file — a grant is a capability
//! over hardware, and who ends up holding it is the kernel's business.
//!
//! # Concurrency contract
//!
//! Both tables are installed once, on the bootstrap hart, before any
//! secondary is started.
//!
//! A granted source is masked and unmasked on its own PLIC, at the
//! context of the hart that takes that PLIC's interrupts. The number a
//! grant carries is the backend's encoding of both (see
//! `net::InterruptDomains::grant_interrupt`), and the hooks decode it
//! through the domains the bootstrap hart publishes at the end of
//! bring-up. No hook runs before then: the kernel calls them only for a
//! grant's owner, or from that grant's interrupt route.
//!
//! Masking is not a single store. A PLIC enable bit lives in a 32-bit
//! word shared with thirty-one other sources, and the `plic` crate
//! reads, modifies and writes that whole word — so two harts arming or
//! holding off two *different* sources in the same word lose a bit
//! between them. `net::ExternalInterrupts::set_source_enabled` is the
//! only writer of those words in this backend, and it serialises them
//! per PLIC; see its lock's comment for why the critical section is
//! part of that.

use alloc::vec::Vec;

use fdt::Fdt;
use fdt::node::FdtNode;
use helios_hal::device::{DeviceRegion, DeviceRegionAttributes, DmaCapability, DmaPlacement};
use helios_hal::iommu::{DmaTranslation, PhysicalRange};
use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{AddressSpace, AddressSpaceError, PageFlags, VirtAddr, VirtRange};
use helios_kernel::{
    DEFAULT_DMA_BUDGET_BYTES, DeviceGrant, DeviceGrantRegistry, DeviceInterruptHooks,
    DeviceInterruptRoute, DeviceName, DeviceVmHooks, DmaBudget, GrantError, GrantInterrupt,
};

use crate::net::{InterruptDomains, PlicInterrupt};

fn allocate_run(bytes: usize, placement: DmaPlacement) -> Result<PhysicalRange, AddressSpaceError> {
    crate::vmm::user_address_space_or_panic().allocate_run(bytes, placement)
}

fn free_run(run: PhysicalRange, align: u64) -> Result<(), AddressSpaceError> {
    crate::vmm::user_address_space_or_panic().free_run(run, align)
}

fn substitute_memory(
    virt: VirtRange,
    physical: PhysicalRange,
    flags: PageFlags,
) -> Result<(), AddressSpaceError> {
    crate::vmm::user_address_space_or_panic().substitute_memory(virt, physical, flags)
}

fn substitute_device(virt: VirtRange, region: DeviceRegion) -> Result<(), AddressSpaceError> {
    crate::vmm::user_address_space_or_panic().substitute_device(virt, region)
}

fn restore(virt: VirtRange) -> Result<(), AddressSpaceError> {
    crate::vmm::user_address_space_or_panic().restore(virt)
}

fn withdraw(virt: VirtRange) -> Result<(), AddressSpaceError> {
    crate::vmm::user_address_space_or_panic().withdraw(virt)
}

/// Sv48 leaves map 4 KiB, which is also the granularity the kernel's
/// page tables are built with.
fn mapping_granule() -> u64 {
    PhysFrame::SIZE as u64
}

/// The kernel identity-maps physical memory on this backend, so a
/// frame's kernel address is its physical address.
fn kernel_alias(frame: PhysFrame) -> VirtAddr {
    VirtAddr::new(frame.phys_addr())
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

fn mask(raw: u32) {
    let (domain, source) = crate::net::interrupt_domains().granted_source(raw);
    domain.set_source_enabled(source, false);
}

fn unmask(raw: u32) {
    let (domain, source) = crate::net::interrupt_domains().granted_source(raw);
    domain.set_source_enabled(source, true);
}

static INTERRUPT_HOOKS: DeviceInterruptHooks = DeviceInterruptHooks { mask, unmask };

/// Install the backend's half of the device path.
///
/// Runs before any grant is published, which the registry checks: a
/// grant claimable before the hooks existed would be a driver holding a
/// device the kernel cannot map.
pub(crate) fn install_hooks() {
    helios_kernel::install_device_vm_hooks(&VM_HOOKS);
    helios_kernel::install_device_interrupt_hooks(&INTERRUPT_HOOKS);
}

/// One device the kernel has no driver for.
#[derive(Clone, Copy)]
pub(crate) struct GrantableDevice {
    pub(crate) name: &'static str,
    base: usize,
    bytes: usize,
    /// The PLIC line the device raises.
    pub(crate) line: PlicInterrupt,
    /// The number the grant carries for `line`.
    interrupt: GrantInterrupt,
    coherent: bool,
}

/// `compatible` strings of everything this backend drives itself.
///
/// A device tree describes far more than the kernel drives. What is
/// left once these are removed is, by definition, hardware nobody in
/// the kernel claims — which is exactly what a driver plugin exists
/// for.
const KERNEL_DRIVEN: &[&str] = &[
    "virtio,mmio",
    "riscv,plic0",
    "sifive,plic-1.0.0",
    "ns16550a",
    "ns16550",
    "google,goldfish-rtc",
    "riscv,clint0",
    "sifive,clint0",
    "sifive,test0",
    "sifive,test1",
];

/// Publish a grant for every device the firmware described and no
/// kernel driver claimed, and return the lines they need routed.
///
/// Each line is attached to its own PLIC's domain by the caller, which
/// gives it a priority and leaves it disabled. Nothing owns the device
/// yet, so an interrupt arriving now would have nowhere to go; the
/// first `unmask` from the driver that claims it is what arms the
/// hardware.
pub(crate) fn publish_grants(
    fdt: &Fdt<'static>,
    registry: &DeviceGrantRegistry,
    domains: &InterruptDomains,
) -> Vec<(PlicInterrupt, DeviceInterruptRoute)> {
    let discovered = discover(fdt, domains);
    let mut grants = Vec::new();
    for device in &discovered {
        match build_grant(device) {
            Ok(grant) => grants.push(grant),
            // A device this backend cannot express as a grant is
            // reported and skipped. The machine still boots and the log
            // names the device and the reason, rather than the kernel
            // refusing to start over hardware nothing needs yet.
            Err(error) => tracing::warn!(
                node = device.name,
                %error,
                "the device tree describes a device that cannot be granted"
            ),
        }
    }
    if grants.is_empty() {
        return Vec::new();
    }
    registry.publish(grants).unwrap_or_else(|error| {
        panic!("RISC-V could not publish the machine's device grants: {error}")
    });

    let mut routes = Vec::new();
    for device in &discovered {
        let Some(published) = registry.device(device.name) else {
            continue;
        };
        routes.push((
            device.line,
            DeviceInterruptRoute::new(published.clone(), device.interrupt),
        ));
    }
    routes
}

/// Every node with a register window and a routable interrupt that the
/// kernel does not drive itself.
///
/// A node whose window is not frame-aligned is skipped with a warning
/// rather than refused: it is a device this backend cannot isolate, not
/// a machine it cannot boot. Mapping it would put a neighbour's
/// registers in the same page, which is the one thing a grant must
/// never do. A node wired to a controller no hart takes interrupts from
/// is skipped the same way: a driver could map it but never hear it.
fn discover(fdt: &Fdt<'static>, domains: &InterruptDomains) -> Vec<GrantableDevice> {
    let frame = PhysFrame::SIZE;
    let mut devices = Vec::new();
    for node in fdt.all_nodes() {
        if drives_itself(&node) {
            continue;
        }
        let Some((base, bytes)) = described_region(&node) else {
            continue;
        };
        // The PLIC's specifier is a bare source number, so any node
        // that names one on a PLIC names something it can route.
        let Some(line) =
            helios_virtio::node_interrupt(fdt, &node).and_then(PlicInterrupt::from_specifier)
        else {
            continue;
        };
        let Some(interrupt) = domains.grant_interrupt(line) else {
            tracing::warn!(
                node = node.name,
                controller = line.controller.base(),
                "the device tree wires a device to an interrupt controller no hart takes \
                 interrupts from; it is not offered to a driver"
            );
            continue;
        };
        if !base.is_multiple_of(frame) || !bytes.is_multiple_of(frame) {
            tracing::warn!(
                node = node.name,
                base,
                bytes,
                "the device tree describes a device whose window is not frame-aligned; \
                 it cannot be isolated and is not offered to a driver"
            );
            continue;
        }
        devices.push(GrantableDevice {
            name: node.name,
            base,
            bytes,
            line,
            interrupt,
            coherent: node.property("dma-coherent").is_some(),
        });
    }
    devices
}

fn drives_itself(node: &FdtNode<'_, '_>) -> bool {
    node.compatible()
        .is_some_and(|entries| entries.all().any(|entry| KERNEL_DRIVEN.contains(&entry)))
}

/// The register window a node declares, when it declares one this
/// backend can read.
///
/// A missing or zero-sized `reg` is an answer rather than an error:
/// this walks nodes nobody chose, and a processor or a PCI function
/// carries a `reg` that names no window at all.
fn described_region(node: &FdtNode<'_, '_>) -> Option<(usize, usize)> {
    let region = node.reg()?.next()?;
    let bytes = region.size?;
    (bytes != 0).then_some((region.starting_address as usize, bytes))
}

/// Turn one described device into a grant.
fn build_grant(device: &GrantableDevice) -> Result<DeviceGrant, GrantError> {
    let name = DeviceName::new(device.name)?;
    DeviceGrant::new(
        name,
        DmaBudget {
            capability: DmaCapability {
                // A node that declares no `dma-ranges` masters the
                // machine's own physical address space, and this
                // backend identity-maps all of it.
                address_bits: u64::BITS,
                coherent: device.coherent,
                translation: DmaTranslation::direct(),
            },
            byte_budget: DEFAULT_DMA_BUDGET_BYTES,
        },
    )
    .with_region(DeviceRegion {
        physical: PhysicalRange {
            start: device.base as u64,
            bytes: device.bytes as u64,
        },
        // Every window a device-tree `reg` names is a register file
        // until something says otherwise. A prefetchable aperture is
        // described by a bus binding rather than by a bare `reg`, and
        // this walk does not decode one.
        attributes: DeviceRegionAttributes::REGISTERS,
    })?
    .with_interrupt(device.interrupt)
}
