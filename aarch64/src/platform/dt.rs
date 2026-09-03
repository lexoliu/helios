//! The flattened device tree, decoded into a [`PlatformDescription`].
//!
//! This is the description QEMU's `virt` board publishes when it is
//! booted with ACPI disabled, and the one every real device-tree
//! platform publishes. The walk is the one the backend has always done;
//! it now produces the shared description instead of handing each
//! device module its own view of the blob.

use arm_gic::Trigger;
use fdt::Fdt;
use fdt::node::FdtNode;
use helios_virtio::{InterruptTrigger, MmioInterrupt};

use super::{
    ConsoleDescription, GicDescription, MmioRegion, PlatformDescription, PlatformError,
    PlatformSource, Slots, SpiInterrupt, VirtioMmioSlot,
};

/// `compatible` string of the interrupt controller this backend drives.
const GIC_V3: &str = "arm,gic-v3";
/// `compatible` string of the console UART.
const PL011: &str = "arm,pl011";
/// `compatible` string of the real-time clock.
const PL031: &str = "arm,pl031";

/// Parses the blob Limine handed over.
pub(super) fn parse(dtb: usize) -> Result<Fdt<'static>, PlatformError> {
    // SAFETY: Limine's DTB response points at a flattened device tree
    // that stays mapped for the life of the kernel, and `Fdt::from_ptr`
    // validates the header before reading anything else.
    unsafe { Fdt::from_ptr(dtb as *const u8) }
        .map_err(|_| PlatformError::DeviceTree("Limine's DTB response is not a valid blob"))
}

/// The console UART, without touching anything else in the tree.
pub(super) fn console(fdt: &Fdt<'static>) -> Result<ConsoleDescription, PlatformError> {
    let node = find_compatible(fdt, PL011).ok_or(PlatformError::DeviceTreeMissing(
        "arm,pl011 console UART node",
    ))?;
    Ok(ConsoleDescription {
        region: first_region(&node, PL011)?,
        interrupt: node_interrupt(fdt, &node),
    })
}

pub(super) fn describe(
    fdt: &Fdt<'static>,
    console: ConsoleDescription,
) -> Result<PlatformDescription, PlatformError> {
    let mut virtio = Slots::new();
    for candidate in helios_virtio::mmio_candidates(fdt) {
        let interrupt = candidate.interrupt.ok_or(PlatformError::DeviceTree(
            "a virtio,mmio node declares no interrupt, so its completions could only be polled",
        ))?;
        virtio.push(
            VirtioMmioSlot {
                region: MmioRegion {
                    base: candidate.base,
                    size: candidate.size,
                },
                interrupt: spi(interrupt)?,
            },
            "virtio-mmio transports",
        );
    }
    Ok(PlatformDescription {
        source: PlatformSource::DeviceTree,
        console,
        gic: gic(fdt)?,
        rtc: find_compatible(fdt, PL031)
            .map(|node| first_region(&node, PL031))
            .transpose()?,
        virtio,
        boot_entropy_seed: helios_hal::entropy::device_tree_seed(fdt),
    })
}

fn gic(fdt: &Fdt<'_>) -> Result<GicDescription, PlatformError> {
    let node = find_compatible(fdt, GIC_V3).ok_or(PlatformError::DeviceTreeMissing(
        "arm,gic-v3 interrupt controller node",
    ))?;
    if let Some(regions) = node.property("#redistributor-regions") {
        let regions =
            crate::fdt_cells_to_usize(regions.value, "AArch64 GIC #redistributor-regions");
        if regions != 1 {
            return Err(PlatformError::DeviceTree(
                "the GIC declares more than one redistributor region",
            ));
        }
    }
    let mut reg = node.raw_reg().ok_or(PlatformError::DeviceTree(
        "the GIC node has no reg property",
    ))?;
    let distributor = reg.next().ok_or(PlatformError::DeviceTree(
        "the GIC node has no distributor reg entry",
    ))?;
    let redistributor = reg.next().ok_or(PlatformError::DeviceTree(
        "the GIC node has no redistributor reg entry",
    ))?;
    Ok(GicDescription {
        distributor: MmioRegion {
            base: crate::fdt_cells_to_usize(distributor.address, "AArch64 GICD reg address"),
            size: crate::fdt_cells_to_usize(distributor.size, "AArch64 GICD reg size"),
        },
        redistributor: MmioRegion {
            base: crate::fdt_cells_to_usize(redistributor.address, "AArch64 GICR reg address"),
            size: crate::fdt_cells_to_usize(redistributor.size, "AArch64 GICR reg size"),
        },
        // A device tree names the redistributor range but not which
        // frame in it belongs to which processor; the driver matches
        // each frame by the affinity it reports for itself.
        redistributors: Slots::new(),
    })
}

fn spi(interrupt: MmioInterrupt) -> Result<SpiInterrupt, PlatformError> {
    let trigger = interrupt.trigger.ok_or(PlatformError::DeviceTree(
        "a device interrupt specifier declares no trigger mode",
    ))?;
    Ok(SpiInterrupt {
        number: interrupt.number,
        trigger: match trigger {
            InterruptTrigger::Edge => Trigger::Edge,
            InterruptTrigger::Level => Trigger::Level,
        },
    })
}

fn find_compatible<'b, 'a: 'b>(fdt: &'b Fdt<'a>, compatible: &str) -> Option<FdtNode<'b, 'a>> {
    fdt.all_nodes().find(|node| {
        node.compatible()
            .is_some_and(|entries| entries.all().any(|entry| entry == compatible))
    })
}

fn first_region(node: &FdtNode<'_, '_>, what: &str) -> Result<MmioRegion, PlatformError> {
    let region = node
        .raw_reg()
        .and_then(|mut regions| regions.next())
        .ok_or(PlatformError::DeviceTree(
            "a device node has no reg property",
        ))?;
    let size = crate::fdt_cells_to_usize(region.size, what);
    if size == 0 {
        return Err(PlatformError::DeviceTree(
            "a device node's reg property has zero size",
        ));
    }
    Ok(MmioRegion {
        base: crate::fdt_cells_to_usize(region.address, what),
        size,
    })
}

/// The interrupt a non-virtio device node declares, if any.
///
/// Reuses the virtio walk's specifier decoder: the `interrupts`
/// property has the same shape whatever device declares it, and the
/// GIC's three-cell binding is the only one an AArch64 tree uses.
fn node_interrupt<'b, 'a: 'b>(fdt: &'b Fdt<'a>, node: &FdtNode<'b, 'a>) -> Option<SpiInterrupt> {
    let interrupt = helios_virtio::node_interrupt(fdt, node)?;
    Some(spi(interrupt).expect("a console interrupt specifier declares a trigger mode"))
}
