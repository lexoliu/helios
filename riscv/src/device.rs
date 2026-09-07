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
//! Masking is not a single store. A PLIC enable bit lives in a 32-bit
//! word shared with thirty-one other sources, and the `plic` crate
//! reads, modifies and writes that whole word — so two harts arming or
//! holding off two *different* sources in the same word lose a bit
//! between them. [`set_source_enabled`] is the only writer of those
//! words in this backend, and it serialises them; see the lock's own
//! comment for why the critical section is part of that.

use alloc::vec::Vec;
use core::num::NonZeroU32;

use fdt::Fdt;
use fdt::node::FdtNode;
use helios_hal::device::{DeviceRegion, DeviceRegionAttributes, DmaCapability, DmaPlacement};
use helios_hal::iommu::{DmaTranslation, PhysicalRange};
use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{AddressSpace, AddressSpaceError, PageFlags, VirtRange};
use helios_kernel::{
    DEFAULT_DMA_BUDGET_BYTES, DeviceGrant, DeviceGrantRegistry, DeviceInterruptHooks,
    DeviceInterruptRoute, DeviceName, DeviceVmHooks, DmaBudget, GrantError, GrantInterrupt,
};
use plic::Plic;
use spin::{Mutex, Once};

use crate::net::{InterruptSourceId, PlicContext};

/// The controller the masking hooks drive, and the context a granted
/// source is enabled on.
///
/// The hooks are plain function pointers, by design — see
/// `DeviceVmHooks` — so what they act on is reached through a
/// write-once cell rather than carried in a closure. Every platform
/// device on this backend shares one PLIC context, which is why one
/// pair suffices.
static CONTROLLER: Once<(&'static Plic, PlicContext)> = Once::new();

fn controller() -> (&'static Plic, PlicContext) {
    *CONTROLLER
        .get()
        .expect("a granted device's interrupt was masked before the controller was published")
}

/// Serialises the PLIC's enable words.
///
/// `plic::Plic::enable` and `disable` are a read-modify-write of the
/// 32-bit word that carries a source's enable bit, and that word covers
/// thirty-two sources. Two harts touching two different sources in the
/// same word therefore race, and one of them loses its bit — a granted
/// device that stops delivering, or worse, a kernel device that does.
///
/// The lock is taken with interrupts masked on the local hart because
/// one of the writers is a granted device's [`mask`], which runs in
/// interrupt context: a task holding this lock and then interrupted
/// into `mask` on its own hart would spin on itself.
static ENABLE_WORDS: Mutex<()> = Mutex::new(());

/// Arm or hold off one PLIC source, without disturbing the thirty-one
/// that share its enable word.
pub(crate) fn set_source_enabled(
    plic: &Plic,
    context: PlicContext,
    source: InterruptSourceId,
    enabled: bool,
) {
    critical_section::with(|_| {
        let _words = ENABLE_WORDS.lock();
        if enabled {
            plic.enable(source, context);
        } else {
            plic.disable(source, context);
        }
    });
}

fn source(raw: u32) -> InterruptSourceId {
    InterruptSourceId(
        NonZeroU32::new(raw).unwrap_or_else(|| panic!("PLIC source 0 is not an interrupt")),
    )
}

fn map_device(virt: VirtRange, region: DeviceRegion) -> Result<(), AddressSpaceError> {
    crate::vmm::user_address_space_or_panic().map_device(virt, region)
}

fn unmap_device(virt: VirtRange) -> Result<(), AddressSpaceError> {
    crate::vmm::user_address_space_or_panic().unmap_device(virt)
}

fn commit_contiguous(
    virt: VirtRange,
    flags: PageFlags,
    placement: DmaPlacement,
) -> Result<PhysFrame, AddressSpaceError> {
    crate::vmm::user_address_space_or_panic().commit_contiguous(virt, flags, placement)
}

fn release_contiguous(virt: VirtRange, align: u64) -> Result<(), AddressSpaceError> {
    crate::vmm::user_address_space_or_panic().release_contiguous(virt, align)
}

/// Sv48 leaves map 4 KiB, which is also the granularity the kernel's
/// page tables are built with.
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

fn mask(raw: u32) {
    let (plic, context) = controller();
    set_source_enabled(plic, context, source(raw), false);
}

fn unmask(raw: u32) {
    let (plic, context) = controller();
    set_source_enabled(plic, context, source(raw), true);
}

static INTERRUPT_HOOKS: DeviceInterruptHooks = DeviceInterruptHooks { mask, unmask };

/// Install the backend's half of the device path.
///
/// Runs before any grant is published, which the registry checks: a
/// grant claimable before the hooks existed would be a driver holding a
/// device the kernel cannot map.
pub(crate) fn install_hooks(plic: &'static Plic, context: PlicContext) {
    CONTROLLER.call_once(|| (plic, context));
    helios_kernel::install_device_vm_hooks(&VM_HOOKS);
    helios_kernel::install_device_interrupt_hooks(&INTERRUPT_HOOKS);
}

/// One device the kernel has no driver for.
#[derive(Clone, Copy)]
pub(crate) struct GrantableDevice {
    pub(crate) name: &'static str,
    base: usize,
    bytes: usize,
    pub(crate) source: InterruptSourceId,
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
/// kernel driver claimed, and return the sources they need routed.
///
/// Each source is given a priority and then left disabled at the PLIC.
/// Nothing owns the device yet, so an interrupt arriving now would have
/// nowhere to go; the first `unmask` from the driver that claims it is
/// what arms the hardware.
pub(crate) fn publish_grants(
    fdt: &Fdt<'static>,
    registry: &DeviceGrantRegistry,
) -> Vec<(InterruptSourceId, DeviceInterruptRoute)> {
    let discovered = discover(fdt);
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

    let (plic, context) = controller();
    let mut routes = Vec::new();
    for device in &discovered {
        let Some(published) = registry.device(device.name) else {
            continue;
        };
        plic.set_priority(device.source, 1);
        set_source_enabled(plic, context, device.source, false);
        routes.push((
            device.source,
            DeviceInterruptRoute::new(
                published.clone(),
                GrantInterrupt::new(device.source.0.get()),
            ),
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
/// never do.
fn discover(fdt: &Fdt<'static>) -> Vec<GrantableDevice> {
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
        // that names one names something this controller can route.
        let Some(interrupt) = helios_virtio::node_interrupt(fdt, &node) else {
            continue;
        };
        let Some(number) = NonZeroU32::new(interrupt.number) else {
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
            source: InterruptSourceId(number),
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
    .with_interrupt(GrantInterrupt::new(device.source.0.get()))
}
