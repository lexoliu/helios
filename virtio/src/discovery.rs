//! FDT-driven discovery of `virtio,mmio` platform devices.
//!
//! Every MMIO-bus backend walks the same device-tree shape; only the
//! address translation in front of the probe differs per architecture.
//! The walk lives here and backends probe the candidates at whatever
//! virtual address their MMIO mapping provides.

use fdt::Fdt;
use fdt::node::FdtNode;

use crate::DeviceType;

const REG_MAGIC_VALUE: usize = 0x000;
const REG_VERSION: usize = 0x004;
const REG_DEVICE_ID: usize = 0x008;
const MAGIC_VALUE: u32 = 0x7472_6976;
const MODERN_VERSION: u32 = 2;

/// `#interrupt-cells` of a controller whose specifier is a bare source
/// number, such as the RISC-V PLIC.
const NUMBER_ONLY_CELLS: usize = 1;
/// `#interrupt-cells` of the Arm GIC binding: `<kind number flags>`.
const ARM_GIC_CELLS: usize = 3;
/// Arm GIC specifier kind for a Shared Peripheral Interrupt.
const ARM_GIC_KIND_SPI: u32 = 0;
/// `IRQ_TYPE_EDGE_RISING | IRQ_TYPE_EDGE_FALLING`.
const IRQ_TYPE_EDGE: u32 = 0b0011;
/// `IRQ_TYPE_LEVEL_HIGH | IRQ_TYPE_LEVEL_LOW`.
const IRQ_TYPE_LEVEL: u32 = 0b1100;

/// The trigger mode an interrupt specifier declares.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptTrigger {
    Edge,
    Level,
}

/// One decoded entry of a device node's `interrupts` property.
///
/// The specifier layout depends on the interrupt parent's
/// `#interrupt-cells`, so decoding always resolves the parent first.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmioInterrupt {
    /// Controller-relative interrupt number: the PLIC source for a
    /// one-cell binding, the SPI index for the three-cell Arm GIC
    /// binding (whose INTID is `32 + number`).
    pub number: u32,
    /// Trigger mode from the specifier. A one-cell binding does not
    /// encode one, leaving the controller's own default in place.
    pub trigger: Option<InterruptTrigger>,
}

/// One `virtio,mmio` node: physical base address, MMIO window size, and
/// the first declared interrupt, if any.
#[derive(Clone, Copy, Debug)]
pub struct MmioCandidate {
    pub base: usize,
    pub size: usize,
    pub interrupt: Option<MmioInterrupt>,
}

/// Iterates every `virtio,mmio` node in `fdt`.
///
/// Reg cells are parsed raw so the walk is independent of the parent
/// bus `#address-cells`; a malformed cell width on a matching node
/// panics rather than silently skipping the device.
pub fn mmio_candidates<'f>(fdt: &'f Fdt<'f>) -> impl Iterator<Item = MmioCandidate> + 'f {
    fdt.all_nodes().filter_map(move |node| {
        if !node
            .compatible()
            .is_some_and(|compatible| compatible.all().any(|entry| entry == "virtio,mmio"))
        {
            return None;
        }
        let region = node.raw_reg().and_then(|mut regions| regions.next())?;
        let base = reg_cells_to_usize(region.address, "virtio,mmio reg address");
        let size = reg_cells_to_usize(region.size, "virtio,mmio reg size");
        Some(MmioCandidate {
            base,
            size,
            interrupt: node_interrupt(fdt, &node),
        })
    })
}

/// Probes a mapped `virtio,mmio` register window for a modern virtio
/// device of the expected type.
///
/// # Safety
///
/// `virtual_base` must point at a currently mapped MMIO window with at
/// least the first three 32-bit registers readable.
pub unsafe fn mmio_device_matches(virtual_base: usize, expected: DeviceType) -> bool {
    unsafe {
        read_u32(virtual_base + REG_MAGIC_VALUE) == MAGIC_VALUE
            && read_u32(virtual_base + REG_VERSION) == MODERN_VERSION
            && read_u32(virtual_base + REG_DEVICE_ID) == expected as u32
    }
}

/// Decodes the first entry of `node`'s `interrupts` property against
/// the `#interrupt-cells` of its interrupt parent.
///
/// Shared with the backends: a `virtio,mmio` transport and a platform
/// UART write the same specifier shape, so the decode belongs in one
/// place rather than once per device kind.
pub fn node_interrupt<'b, 'a: 'b>(
    fdt: &'b Fdt<'a>,
    node: &FdtNode<'b, 'a>,
) -> Option<MmioInterrupt> {
    let interrupts = node.property("interrupts")?;
    let name = node.name;
    let cells = interrupt_cells(fdt, node, name);
    let mut values = interrupt_cell_values(interrupts.value, name);
    match cells {
        NUMBER_ONLY_CELLS => Some(MmioInterrupt {
            number: next_interrupt_cell(&mut values, name),
            trigger: None,
        }),
        ARM_GIC_CELLS => {
            let kind = next_interrupt_cell(&mut values, name);
            assert!(
                kind == ARM_GIC_KIND_SPI,
                "device tree node {name} declares Arm GIC interrupt kind {kind}, \
                 only shared peripheral interrupts are routable"
            );
            let number = next_interrupt_cell(&mut values, name);
            let flags = next_interrupt_cell(&mut values, name);
            Some(MmioInterrupt {
                number,
                trigger: Some(trigger_from_flags(flags, name)),
            })
        }
        cells => panic!(
            "device tree node {name} has an interrupt parent with unsupported \
             #interrupt-cells {cells}"
        ),
    }
}

fn interrupt_cells<'b, 'a: 'b>(fdt: &'b Fdt<'a>, node: &FdtNode<'b, 'a>, name: &str) -> usize {
    interrupt_parent(fdt, node, name)
        .interrupt_cells()
        .unwrap_or_else(|| {
            panic!("interrupt parent of device tree node {name} has no #interrupt-cells")
        })
}

/// Resolves the interrupt controller a node's `interrupts` property is
/// written against: its own `interrupt-parent`, or the one the root
/// node declares for the whole tree.
fn interrupt_parent<'b, 'a: 'b>(
    fdt: &'b Fdt<'a>,
    node: &FdtNode<'b, 'a>,
    name: &str,
) -> FdtNode<'b, 'a> {
    let phandle = node
        .property("interrupt-parent")
        .or_else(|| fdt.root().property("interrupt-parent"))
        .unwrap_or_else(|| {
            panic!("device tree node {name} declares interrupts without an interrupt parent")
        })
        .value;
    let phandle = u32::from_be_bytes(phandle.try_into().unwrap_or_else(|_| {
        panic!("interrupt-parent of device tree node {name} is not a single phandle cell")
    }));
    fdt.find_phandle(phandle).unwrap_or_else(|| {
        panic!("interrupt-parent phandle {phandle:#x} of device tree node {name} is unknown")
    })
}

fn interrupt_cell_values<'v>(bytes: &'v [u8], name: &str) -> impl Iterator<Item = u32> + 'v {
    assert!(
        bytes.len().is_multiple_of(4),
        "interrupts property of device tree node {name} is not a whole number of cells"
    );
    bytes.chunks_exact(4).map(|cell| {
        u32::from_be_bytes(
            cell.try_into()
                .unwrap_or_else(|_| panic!("interrupt cell had invalid width")),
        )
    })
}

fn next_interrupt_cell(values: &mut impl Iterator<Item = u32>, name: &str) -> u32 {
    values.next().unwrap_or_else(|| {
        panic!("interrupts property of device tree node {name} is shorter than one specifier")
    })
}

fn trigger_from_flags(flags: u32, name: &str) -> InterruptTrigger {
    if flags & IRQ_TYPE_LEVEL != 0 {
        InterruptTrigger::Level
    } else if flags & IRQ_TYPE_EDGE != 0 {
        InterruptTrigger::Edge
    } else {
        panic!("device tree node {name} declares interrupt flags {flags:#x} with no trigger")
    }
}

fn reg_cells_to_usize(bytes: &[u8], name: &str) -> usize {
    assert!(
        bytes.len() == 4 || bytes.len() == 8,
        "{name} must contain one or two 32-bit cells, got {} bytes",
        bytes.len()
    );
    let mut value = 0usize;
    for cell in bytes.chunks_exact(4) {
        value = value
            .checked_shl(32)
            .unwrap_or_else(|| panic!("{name} cell shift overflow"))
            | u32::from_be_bytes(
                cell.try_into()
                    .unwrap_or_else(|_| panic!("{name} cell had invalid width")),
            ) as usize;
    }
    value
}

unsafe fn read_u32(address: usize) -> u32 {
    unsafe { (address as *const u32).read_volatile() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// Minimal flattened-device-tree writer covering the two shapes the
    /// MMIO backends see: a PLIC-style one-cell controller reached
    /// through the node's own `interrupt-parent`, and an Arm GIC
    /// three-cell controller inherited from the root node.
    struct DtbBuilder {
        structure: Vec<u8>,
        strings: Vec<u8>,
    }

    const FDT_BEGIN_NODE: u32 = 1;
    const FDT_END_NODE: u32 = 2;
    const FDT_PROP: u32 = 3;
    const FDT_END: u32 = 9;
    const FDT_MAGIC: u32 = 0xd00d_feed;
    const FDT_VERSION: u32 = 17;
    const FDT_LAST_COMPATIBLE_VERSION: u32 = 16;
    const FDT_HEADER_BYTES: usize = 40;
    const MEMORY_RESERVATION_BYTES: usize = 16;

    impl DtbBuilder {
        fn new() -> Self {
            Self {
                structure: Vec::new(),
                strings: Vec::new(),
            }
        }

        fn begin_node(&mut self, name: &str) -> &mut Self {
            self.structure
                .extend_from_slice(&FDT_BEGIN_NODE.to_be_bytes());
            self.structure.extend_from_slice(name.as_bytes());
            self.structure.push(0);
            self.pad_structure();
            self
        }

        fn end_node(&mut self) -> &mut Self {
            self.structure
                .extend_from_slice(&FDT_END_NODE.to_be_bytes());
            self
        }

        fn property(&mut self, name: &str, value: &[u8]) -> &mut Self {
            let name_offset = self.intern(name);
            self.structure.extend_from_slice(&FDT_PROP.to_be_bytes());
            self.structure
                .extend_from_slice(&(value.len() as u32).to_be_bytes());
            self.structure.extend_from_slice(&name_offset.to_be_bytes());
            self.structure.extend_from_slice(value);
            self.pad_structure();
            self
        }

        fn cells(&mut self, name: &str, cells: &[u32]) -> &mut Self {
            let mut value = Vec::new();
            for cell in cells {
                value.extend_from_slice(&cell.to_be_bytes());
            }
            self.property(name, &value)
        }

        fn string(&mut self, name: &str, value: &str) -> &mut Self {
            let mut bytes = Vec::from(value.as_bytes());
            bytes.push(0);
            self.property(name, &bytes)
        }

        fn intern(&mut self, name: &str) -> u32 {
            let offset = self.strings.len() as u32;
            self.strings.extend_from_slice(name.as_bytes());
            self.strings.push(0);
            offset
        }

        fn pad_structure(&mut self) {
            while !self.structure.len().is_multiple_of(4) {
                self.structure.push(0);
            }
        }

        fn finish(mut self) -> Vec<u8> {
            self.structure.extend_from_slice(&FDT_END.to_be_bytes());
            let structure_offset = FDT_HEADER_BYTES + MEMORY_RESERVATION_BYTES;
            let strings_offset = structure_offset + self.structure.len();
            let total = strings_offset + self.strings.len();
            let mut blob = Vec::with_capacity(total);
            for word in [
                FDT_MAGIC,
                total as u32,
                structure_offset as u32,
                strings_offset as u32,
                FDT_HEADER_BYTES as u32,
                FDT_VERSION,
                FDT_LAST_COMPATIBLE_VERSION,
                0,
                self.strings.len() as u32,
                self.structure.len() as u32,
            ] {
                blob.extend_from_slice(&word.to_be_bytes());
            }
            blob.extend_from_slice(&[0; MEMORY_RESERVATION_BYTES]);
            blob.extend_from_slice(&self.structure);
            blob.extend_from_slice(&self.strings);
            blob
        }
    }

    /// The RISC-V `virt` shape: the device names its PLIC directly and
    /// the PLIC uses one-cell specifiers.
    fn plic_tree() -> Vec<u8> {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder
            .cells("#address-cells", &[2])
            .cells("#size-cells", &[2]);
        builder.begin_node("soc");
        builder
            .cells("#address-cells", &[2])
            .cells("#size-cells", &[2]);
        builder.begin_node("plic@c000000");
        builder
            .cells("phandle", &[5])
            .cells("#interrupt-cells", &[1])
            .property("interrupt-controller", &[]);
        builder.end_node();
        builder.begin_node("virtio_mmio@10008000");
        builder
            .string("compatible", "virtio,mmio")
            .cells("reg", &[0, 0x1000_8000, 0, 0x1000])
            .cells("interrupts", &[8])
            .cells("interrupt-parent", &[5]);
        builder.end_node();
        builder.end_node();
        builder.end_node();
        builder.finish()
    }

    /// The Arm `virt` shape: the root node names the GIC and the GIC
    /// uses three-cell `<kind number flags>` specifiers.
    fn arm_gic_tree(kind: u32, flags: u32) -> Vec<u8> {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder
            .cells("#address-cells", &[2])
            .cells("#size-cells", &[2])
            .cells("interrupt-parent", &[0x8005]);
        builder.begin_node("intc@8000000");
        builder
            .cells("phandle", &[0x8005])
            .cells("#interrupt-cells", &[3])
            .property("interrupt-controller", &[])
            .string("compatible", "arm,gic-v3");
        builder.end_node();
        builder.begin_node("virtio_mmio@a000000");
        builder
            .string("compatible", "virtio,mmio")
            .cells("reg", &[0, 0x0a00_0000, 0, 0x200])
            .cells("interrupts", &[kind, 0x10, flags]);
        builder.end_node();
        builder.end_node();
        builder.finish()
    }

    #[test]
    fn one_cell_specifier_yields_the_bare_source_number() {
        let blob = plic_tree();
        let fdt = Fdt::new(&blob).expect("plic tree parses");
        let candidates: Vec<_> = mmio_candidates(&fdt).collect();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].base, 0x1000_8000);
        assert_eq!(candidates[0].size, 0x1000);
        assert_eq!(
            candidates[0].interrupt,
            Some(MmioInterrupt {
                number: 8,
                trigger: None,
            })
        );
    }

    #[test]
    fn three_cell_gic_specifier_yields_the_spi_index_and_trigger() {
        let blob = arm_gic_tree(ARM_GIC_KIND_SPI, 4);
        let fdt = Fdt::new(&blob).expect("gic tree parses");
        let candidates: Vec<_> = mmio_candidates(&fdt).collect();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].base, 0x0a00_0000);
        assert_eq!(
            candidates[0].interrupt,
            Some(MmioInterrupt {
                number: 0x10,
                trigger: Some(InterruptTrigger::Level),
            })
        );
    }

    #[test]
    fn edge_trigger_flags_decode_as_edge() {
        let blob = arm_gic_tree(ARM_GIC_KIND_SPI, 1);
        let fdt = Fdt::new(&blob).expect("gic tree parses");
        let candidate = mmio_candidates(&fdt).next().expect("one candidate");
        assert_eq!(
            candidate.interrupt.expect("interrupt").trigger,
            Some(InterruptTrigger::Edge)
        );
    }

    #[test]
    #[should_panic(expected = "Arm GIC interrupt kind 1")]
    fn private_peripheral_interrupts_are_rejected() {
        let blob = arm_gic_tree(1, 4);
        let fdt = Fdt::new(&blob).expect("gic tree parses");
        let _ = mmio_candidates(&fdt).next();
    }
}
