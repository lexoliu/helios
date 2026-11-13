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
    ConsoleDescription, GicDescription, GrantableDevice, MAX_GRANTABLE_DEVICES, MmioRegion,
    PlatformDescription, PlatformError, PlatformSource, PsciConduit, Slots, SpiInterrupt,
    VirtioMmioSlot,
};

/// `compatible` string of the interrupt controller this backend drives.
const GIC_V3: &str = "arm,gic-v3";
/// `compatible` strings of the PSCI service node, 0.2 and later.
const PSCI: [&str; 2] = ["arm,psci-1.0", "arm,psci-0.2"];
/// `compatible` string of the console UART.
const PL011: &str = "arm,pl011";
/// `compatible` string of the real-time clock.
const PL031: &str = "arm,pl031";
/// `compatible` string of a virtio transport slot, which the virtio
/// walk already claims.
const VIRTIO_MMIO: &str = "virtio,mmio";
/// `compatible` string of a TPM's FIFO interface over MMIO, the one
/// QEMU writes for its `tpm-tis-device` (`hw/core/sysbus-fdt.c`,
/// `add_tpm_tis_fdt_node`).
const TPM_TIS_MMIO: &str = "tcg,tpm-tis-mmio";

/// The mapping granule a grant's region has to respect.
const FRAME: usize = helios_hal::pmm::PhysFrame::SIZE;

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
        interrupt: node_interrupt(fdt, &node).ok_or(PlatformError::DeviceTreeMissing(
            "shared peripheral interrupt on the arm,pl011 console UART node",
        ))?,
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
        psci: psci(fdt)?,
        rtc: find_compatible(fdt, PL031)
            .map(|node| first_region(&node, PL031))
            .transpose()?,
        virtio,
        grantable: grantable(fdt),
        tpm: tpm(fdt)?,
        boot_entropy_seed: helios_hal::entropy::device_tree_seed(fdt),
    })
}

/// The TPM's FIFO interface, when the tree has a `tcg,tpm-tis-mmio`
/// node.
///
/// QEMU puts the node on its platform bus (`/platform-bus@c000000`),
/// whose children name addresses relative to the bus, so the node's
/// `reg` is translated through the bus's `ranges` into the root's
/// address space. A node directly under the root needs no translation;
/// one nested deeper than one bus is refused rather than read at the
/// wrong address.
fn tpm(fdt: &Fdt<'static>) -> Result<Option<super::TpmDescription>, PlatformError> {
    let is_tpm = |node: &FdtNode<'_, '_>| {
        node.compatible()
            .is_some_and(|entries| entries.all().any(|entry| entry == TPM_TIS_MMIO))
    };
    let described = |base: usize| super::TpmDescription::Described {
        base: base as u64,
        interface: helios_tpm::InterfaceDescription::DeviceTreeFifo,
    };
    let root_cells = fdt.root().cell_sizes();
    for node in fdt
        .find_node("/")
        .into_iter()
        .flat_map(|root| root.children())
    {
        if is_tpm(&node) {
            return Ok(Some(described(first_region(&node, TPM_TIS_MMIO)?.base)));
        }
        for child in node.children() {
            if is_tpm(&child) {
                let region = first_region(&child, TPM_TIS_MMIO)?;
                let base = translate(&node, root_cells.address_cells, region.base)?;
                return Ok(Some(described(base)));
            }
        }
    }
    if find_compatible(fdt, TPM_TIS_MMIO).is_some() {
        return Err(PlatformError::DeviceTree(
            "the tcg,tpm-tis-mmio node sits deeper than one bus below the root, whose address \
             translation this backend does not follow",
        ));
    }
    Ok(None)
}

/// The root-space address of `address` on `bus`, through the bus's
/// `ranges`: each entry is a child address, a parent address and a
/// length, in the bus's `#address-cells`, its parent's
/// `#address-cells` and the bus's `#size-cells` (Devicetree
/// Specification, "ranges"). An empty `ranges` is the identity.
fn translate(
    bus: &FdtNode<'_, '_>,
    parent_address_cells: usize,
    address: usize,
) -> Result<usize, PlatformError> {
    let ranges = bus.property("ranges").ok_or(PlatformError::DeviceTree(
        "the bus above the tcg,tpm-tis-mmio node has no ranges property, so its addresses do \
         not reach the processor",
    ))?;
    if ranges.value.is_empty() {
        return Ok(address);
    }
    let cells = bus.cell_sizes();
    let child_bytes = cells.address_cells * 4;
    let parent_bytes = parent_address_cells * 4;
    let size_bytes = cells.size_cells * 4;
    let entry_bytes = child_bytes + parent_bytes + size_bytes;
    if entry_bytes == 0 || !ranges.value.len().is_multiple_of(entry_bytes) {
        return Err(PlatformError::DeviceTree(
            "the bus above the tcg,tpm-tis-mmio node has a ranges property of the wrong size",
        ));
    }
    for entry in ranges.value.chunks_exact(entry_bytes) {
        let (child, rest) = entry.split_at(child_bytes);
        let (parent, size) = rest.split_at(parent_bytes);
        let (Some(child), Some(parent), Some(size)) = (
            crate::fdt_cells(child),
            crate::fdt_cells(parent),
            crate::fdt_cells(size),
        ) else {
            return Err(PlatformError::DeviceTree(
                "a ranges entry above the tcg,tpm-tis-mmio node is not one or two cells per field",
            ));
        };
        if let Some(offset) = address.checked_sub(child).filter(|offset| *offset < size) {
            return parent.checked_add(offset).ok_or(PlatformError::DeviceTree(
                "the tcg,tpm-tis-mmio node translates past the end of the address space",
            ));
        }
    }
    Err(PlatformError::DeviceTree(
        "no ranges entry of its bus covers the tcg,tpm-tis-mmio node's registers",
    ))
}

/// Every node this backend has no driver for and can hand to one in
/// user memory.
///
/// The filter is deliberately narrow, because a grant is a real
/// capability over real hardware. A node qualifies when it has a
/// register window, raises an interrupt this GIC can route, and is not
/// something the kernel drives itself. Nodes that describe the machine
/// rather than a device — the processors, the memory, `chosen`, the
/// architected timer — have no `reg` and an `interrupts` property at
/// most, so they never reach the region check.
///
/// A node whose window is not frame-aligned is skipped with a warning
/// rather than refused: it is a device this backend cannot isolate,
/// not a machine it cannot boot. Mapping it would put a neighbour's
/// registers in the same page, which is the one thing a grant must
/// never do.
fn grantable(fdt: &Fdt<'static>) -> Slots<GrantableDevice, MAX_GRANTABLE_DEVICES> {
    let mut devices = Slots::new();
    for node in fdt.all_nodes() {
        if drives_itself(&node) {
            continue;
        }
        // The region is checked first: most of what a tree describes is
        // the machine rather than a device — the processors, the
        // memory, `chosen`, the architected timer — and none of it has
        // a register window.
        let Some(region) = described_region(&node) else {
            continue;
        };
        let Some(interrupt) = node_interrupt(fdt, &node) else {
            continue;
        };
        if !region.base.is_multiple_of(FRAME) || !region.size.is_multiple_of(FRAME) {
            tracing::warn!(
                node = node.name,
                base = region.base,
                size = region.size,
                "the device tree describes a device whose window is not frame-aligned; \
                 it cannot be isolated and is not offered to a driver"
            );
            continue;
        }
        devices.push(
            GrantableDevice {
                name: node.name,
                region,
                interrupt,
                coherent: node.property("dma-coherent").is_some(),
            },
            "grantable devices",
        );
    }
    devices
}

/// The register window a node declares, when it declares one this
/// backend can read.
///
/// Unlike [`first_region`], a missing or oddly shaped `reg` is an
/// answer rather than an error: this walks nodes nobody chose.
fn described_region(node: &FdtNode<'_, '_>) -> Option<MmioRegion> {
    let region = node.raw_reg().and_then(|mut regions| regions.next())?;
    let base = crate::fdt_cells(region.address)?;
    let size = crate::fdt_cells(region.size)?;
    (size != 0).then_some(MmioRegion { base, size })
}

/// Whether the kernel drives this node itself.
fn drives_itself(node: &FdtNode<'_, '_>) -> bool {
    node.compatible().is_some_and(|entries| {
        entries
            .all()
            .any(|entry| matches!(entry, GIC_V3 | PL011 | PL031 | VIRTIO_MMIO | TPM_TIS_MMIO))
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

/// The conduit of the `psci` node, when the tree has one this backend
/// can call.
///
/// Only the PSCI 0.2 and later bindings are taken: they fix the function
/// identifiers, where the original `arm,psci` binding names them in
/// properties of the node and is not something any platform this backend
/// runs on publishes.
fn psci(fdt: &Fdt<'_>) -> Result<Option<PsciConduit>, PlatformError> {
    let Some(node) = fdt.all_nodes().find(|node| {
        node.compatible()
            .is_some_and(|entries| entries.all().any(|entry| PSCI.contains(&entry)))
    }) else {
        return Ok(None);
    };
    let method = node
        .property("method")
        .and_then(|method| method.as_str())
        .ok_or(PlatformError::DeviceTree(
            "the psci node has no method property",
        ))?;
    match method {
        "hvc" => Ok(Some(PsciConduit::Hvc)),
        "smc" => Ok(Some(PsciConduit::Smc)),
        _ => Err(PlatformError::DeviceTree(
            "the psci node's method is neither hvc nor smc",
        )),
    }
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
    match helios_virtio::node_interrupt_kind(fdt, node)? {
        helios_virtio::NodeInterrupt::Shared(interrupt) => spi(interrupt).ok(),
        // A private peripheral interrupt belongs to one processor and
        // is not routed by affinity, so it is not something this
        // backend can hand to a driver.
        helios_virtio::NodeInterrupt::Private => None,
    }
}
