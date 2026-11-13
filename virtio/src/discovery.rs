//! FDT-driven discovery of `virtio,mmio` platform devices.
//!
//! Every MMIO-bus backend walks the same device-tree shape; only the
//! address translation in front of the probe differs per architecture.
//! The walk lives here and backends probe the candidates at whatever
//! virtual address their MMIO mapping provides.

use fdt::Fdt;
use fdt::node::FdtNode;
use helios_hal::mmio;

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
/// `compatible` strings of the RISC-V platform-level interrupt
/// controller: the `sifive,plic-1.0.0` binding and the `riscv,plic0`
/// name QEMU's `virt` also lists.
const PLIC_COMPATIBLE: [&str; 2] = ["sifive,plic-1.0.0", "riscv,plic0"];
/// The cause a PLIC's `interrupts-extended` entry names for a hart's
/// supervisor-external context: interrupt 9 of `scause` in the RISC-V
/// privileged architecture. Machine-external (11) entries are contexts
/// too, so they advance the context index without being one a
/// supervisor kernel claims from.
const SUPERVISOR_EXTERNAL_CAUSE: u32 = 9;

/// The trigger mode an interrupt specifier declares.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptTrigger {
    Edge,
    Level,
}

/// An interrupt controller, identified by the physical base address of
/// the first register window its node declares.
///
/// A source number means something only together with the controller
/// it is numbered on: a machine may carry several controllers of one
/// kind — QEMU's RISC-V `virt` gives every socket its own PLIC — and
/// wire different devices to different ones. A backend matches a
/// device's interrupt against the controllers it brought up by this
/// key, which is also the address it drives the controller at.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterruptController {
    base: usize,
}

impl InterruptController {
    /// The controller `node` describes, when it declares a register
    /// window to be identified by.
    pub fn of_node(node: &FdtNode<'_, '_>) -> Option<Self> {
        let region = node.raw_reg().and_then(|mut regions| regions.next())?;
        Some(Self {
            base: reg_cells_to_usize(region.address, "interrupt controller reg address"),
        })
    }

    /// Physical base address of the controller's first register window.
    pub const fn base(self) -> usize {
        self.base
    }
}

/// One decoded entry of a device node's `interrupts` property.
///
/// The specifier layout depends on the interrupt parent's
/// `#interrupt-cells`, so decoding always resolves the parent first.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmioInterrupt {
    /// The controller the specifier is written against, which is the
    /// one that numbers [`Self::number`].
    pub controller: InterruptController,
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
    match node_interrupt_kind(fdt, node)? {
        NodeInterrupt::Shared(interrupt) => Some(interrupt),
        NodeInterrupt::Private => panic!(
            "device tree node {} declares a private peripheral interrupt, \
             and only shared peripheral interrupts are routable",
            node.name
        ),
    }
}

/// What kind of interrupt a node declares.
///
/// [`node_interrupt`] is for a caller that requires a routable line and
/// should fail loudly without one — a virtio transport whose
/// completions could otherwise only be polled. A caller walking nodes
/// it did not choose has to be able to tell the two apart instead,
/// because a private peripheral interrupt is a fact about that node
/// rather than a malformed tree.
pub enum NodeInterrupt {
    /// A line the controller routes by affinity, so it can be given to
    /// a driver.
    Shared(MmioInterrupt),
    /// A line that belongs to one processor. It is not routed, so it
    /// cannot be granted.
    Private,
}

/// Decodes the first entry of `node`'s `interrupts` property, reporting
/// the kind rather than requiring one.
pub fn node_interrupt_kind<'b, 'a: 'b>(
    fdt: &'b Fdt<'a>,
    node: &FdtNode<'b, 'a>,
) -> Option<NodeInterrupt> {
    let interrupts = node.property("interrupts")?;
    let name = node.name;
    let (parent, cells) = interrupt_parent(fdt, node, name);
    let controller = InterruptController::of_node(&parent).unwrap_or_else(|| {
        panic!(
            "interrupt controller {} of device tree node {name} declares no register window \
             to identify it by",
            parent.name
        )
    });
    let mut values = cell_values(interrupts.value, "interrupts", name);
    match cells {
        NUMBER_ONLY_CELLS => Some(NodeInterrupt::Shared(MmioInterrupt {
            controller,
            number: next_interrupt_cell(&mut values, name),
            trigger: None,
        })),
        ARM_GIC_CELLS => {
            let kind = next_interrupt_cell(&mut values, name);
            if kind != ARM_GIC_KIND_SPI {
                return Some(NodeInterrupt::Private);
            }
            let number = next_interrupt_cell(&mut values, name);
            let flags = next_interrupt_cell(&mut values, name);
            Some(NodeInterrupt::Shared(MmioInterrupt {
                controller,
                number,
                trigger: Some(trigger_from_flags(flags, name)),
            }))
        }
        cells => panic!(
            "device tree node {name} has an interrupt parent with unsupported \
             #interrupt-cells {cells}"
        ),
    }
}

/// Resolves the interrupt controller a node's `interrupts` property is
/// written against, together with that controller's `#interrupt-cells`.
///
/// The Devicetree Specification (v0.4, §2.4.1, `interrupt-parent`)
/// makes a node's interrupt parent the node its `interrupt-parent`
/// names or, where it has none, its devicetree parent. Linux's
/// `of_irq_find_parent` turns that into a walk, and this is the same
/// walk: follow the phandle or climb to the tree parent, and keep going
/// from whichever node that reached until it declares
/// `#interrupt-cells`, which every interrupt domain does. A bus node
/// carrying `interrupt-parent` for all its children, as the root does
/// on Arm's `virt` and `soc` does on StarFive's JH7110, is reached by
/// the climb.
fn interrupt_parent<'b, 'a: 'b>(
    fdt: &'b Fdt<'a>,
    node: &FdtNode<'b, 'a>,
    name: &str,
) -> (FdtNode<'b, 'a>, usize) {
    let mut current = *node;
    loop {
        current = match current.property("interrupt-parent") {
            Some(property) => {
                let phandle = u32::from_be_bytes(property.value.try_into().unwrap_or_else(|_| {
                    panic!(
                        "interrupt-parent of device tree node {} is not a single phandle cell",
                        current.name
                    )
                }));
                fdt.find_phandle(phandle).unwrap_or_else(|| {
                    panic!(
                        "interrupt-parent phandle {phandle:#x} of device tree node {} is unknown",
                        current.name
                    )
                })
            }
            None => tree_parent(fdt, &current).unwrap_or_else(|| {
                panic!("device tree node {name} declares interrupts without an interrupt parent")
            }),
        };
        if let Some(cells) = current.interrupt_cells() {
            return (current, cells);
        }
    }
}

/// The node `node` sits under, or `None` for the root.
///
/// `fdt` 0.1.5 keeps no parent link, so this searches down from the
/// root. A node is told apart from its namesakes by where its name sits
/// in the blob: every node's unit name is a distinct slice of the
/// structure block, and that is what `FdtNode::name` borrows.
fn tree_parent<'b, 'a: 'b>(fdt: &'b Fdt<'a>, node: &FdtNode<'b, 'a>) -> Option<FdtNode<'b, 'a>> {
    let root = fdt.all_nodes().next()?;
    parent_below(root, node)
}

fn parent_below<'b, 'a: 'b>(
    ancestor: FdtNode<'b, 'a>,
    node: &FdtNode<'b, 'a>,
) -> Option<FdtNode<'b, 'a>> {
    ancestor.children().find_map(|child| {
        if core::ptr::eq(child.name.as_ptr(), node.name.as_ptr()) {
            Some(ancestor)
        } else {
            parent_below(child, node)
        }
    })
}

/// A hart's supervisor-external context in one PLIC.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlicHartContext {
    /// The hart id its `/cpus` node's `reg` names.
    pub hart: u16,
    /// The context's index in the PLIC: its position in the
    /// controller's `interrupts-extended` list, which is how the PLIC
    /// numbers the enable, threshold and claim registers it gives each
    /// context.
    pub context: usize,
}

/// One PLIC the device tree describes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlicDescription {
    pub controller: InterruptController,
    /// The context that takes this controller's interrupts: the
    /// bootstrap hart's when it has one here, otherwise the
    /// lowest-numbered hart's that does. `None` when no hart has a
    /// supervisor-external context in it, so nothing wired to it can
    /// interrupt a supervisor kernel.
    ///
    /// The bootstrap hart first because it is the hart that brings the
    /// devices up and runs before any other, and a machine with one
    /// PLIC keeps every device on it; the lowest-numbered otherwise so
    /// the choice is the same on every boot of the same machine.
    pub taking: Option<PlicHartContext>,
}

/// Every PLIC the device tree describes, with the context chosen to
/// take each one's interrupts.
pub fn plic_descriptions<'b, 'a: 'b>(
    fdt: &'b Fdt<'a>,
    bootstrap_hart: u16,
) -> impl Iterator<Item = PlicDescription> + 'b {
    fdt.all_nodes()
        .filter(|node| {
            node.compatible().is_some_and(|compatible| {
                compatible.all().any(|name| PLIC_COMPATIBLE.contains(&name))
            })
        })
        .map(move |node| PlicDescription {
            controller: InterruptController::of_node(&node)
                .unwrap_or_else(|| panic!("PLIC node {} declares no register window", node.name)),
            taking: plic_supervisor_contexts(fdt, node)
                .min_by_key(|context| (context.hart != bootstrap_hart, context.hart)),
        })
}

/// The supervisor-external contexts a PLIC's `interrupts-extended`
/// gives harts, in context order.
fn plic_supervisor_contexts<'b, 'a: 'b>(
    fdt: &'b Fdt<'a>,
    plic: FdtNode<'b, 'a>,
) -> impl Iterator<Item = PlicHartContext> + 'b {
    let name = plic.name;
    let mut cells = cell_values(
        plic.property("interrupts-extended")
            .map_or(&[][..], |property| property.value),
        "interrupts-extended",
        name,
    );
    let mut next_context = 0usize;
    core::iter::from_fn(move || {
        loop {
            let phandle = cells.next()?;
            let controller = fdt.find_phandle(phandle).unwrap_or_else(|| {
                panic!("PLIC {name} references unknown interrupt controller phandle {phandle:#x}")
            });
            let specifier_cells = controller.interrupt_cells().unwrap_or_else(|| {
                panic!(
                    "interrupt controller {} that PLIC {name} references has no #interrupt-cells",
                    controller.name
                )
            });
            let cause = cells.next().unwrap_or_else(|| {
                panic!("interrupts-extended of PLIC {name} ends inside a specifier")
            });
            for _ in 1..specifier_cells {
                if cells.next().is_none() {
                    panic!("interrupts-extended of PLIC {name} ends inside a specifier");
                }
            }
            let context = next_context;
            next_context += 1;
            if cause != SUPERVISOR_EXTERNAL_CAUSE {
                continue;
            }
            let Some(hart) = hart_of_local_controller(fdt, &controller) else {
                continue;
            };
            return Some(PlicHartContext { hart, context });
        }
    })
}

/// The hart whose local interrupt controller `controller` is: the
/// `cpu` node it sits under, by that node's `reg`.
fn hart_of_local_controller<'b, 'a: 'b>(
    fdt: &'b Fdt<'a>,
    controller: &FdtNode<'b, 'a>,
) -> Option<u16> {
    let cpu = tree_parent(fdt, controller)?;
    if cpu
        .property("device_type")
        .is_none_or(|property| property.value != b"cpu\0")
    {
        return None;
    }
    let reg = cpu
        .raw_reg()
        .and_then(|mut regions| regions.next())
        .unwrap_or_else(|| panic!("cpu node {} has no reg naming its hart id", cpu.name));
    let hart = reg_cells_to_usize(reg.address, "cpu reg hart id");
    Some(u16::try_from(hart).unwrap_or_else(|_| {
        panic!(
            "hart id {hart:#x} of cpu node {} exceeds a processor id",
            cpu.name
        )
    }))
}

fn cell_values<'v>(bytes: &'v [u8], property: &str, name: &str) -> impl Iterator<Item = u32> + 'v {
    assert!(
        bytes.len().is_multiple_of(4),
        "{property} property of device tree node {name} is not a whole number of cells"
    );
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&cell| u32::from_be_bytes(cell))
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
    for &cell in bytes.as_chunks::<4>().0 {
        value = value
            .checked_shl(32)
            .unwrap_or_else(|| panic!("{name} cell shift overflow"))
            | u32::from_be_bytes(cell) as usize;
    }
    value
}

unsafe fn read_u32(address: usize) -> u32 {
    unsafe { mmio::read_u32(address as *const u32) }
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
            .property("interrupt-controller", &[])
            .cells("reg", &[0, 0x0c00_0000, 0, 0x60_0000]);
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
            .string("compatible", "arm,gic-v3")
            // Distributor, then the redistributor region.
            .cells(
                "reg",
                &[0, 0x0800_0000, 0, 0x1_0000, 0, 0x080a_0000, 0, 0xf6_0000],
            );
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
                controller: InterruptController { base: 0x0c00_0000 },
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
                controller: InterruptController { base: 0x0800_0000 },
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
    #[should_panic(expected = "declares a private peripheral interrupt")]
    fn private_peripheral_interrupts_are_rejected() {
        let blob = arm_gic_tree(1, 4);
        let fdt = Fdt::new(&blob).expect("gic tree parses");
        let _ = mmio_candidates(&fdt).next();
    }

    /// The same tree, through the form a walk over unchosen nodes uses.
    ///
    /// A private peripheral interrupt is a fact about that node rather
    /// than a malformed tree, so a caller that has to decide gets an
    /// answer where a caller that requires a routable line gets a
    /// panic.
    #[test]
    fn a_private_peripheral_interrupt_is_reported_rather_than_refused() {
        let blob = arm_gic_tree(1, 4);
        let fdt = Fdt::new(&blob).expect("gic tree parses");
        let node = fdt
            .all_nodes()
            .find(|node| node.name.starts_with("virtio_mmio"))
            .expect("the tree carries the transport node");
        assert!(matches!(
            node_interrupt_kind(&fdt, &node),
            Some(NodeInterrupt::Private)
        ));
    }

    #[test]
    fn a_shared_peripheral_interrupt_is_reported_with_its_trigger() {
        let blob = arm_gic_tree(0, 4);
        let fdt = Fdt::new(&blob).expect("gic tree parses");
        let node = fdt
            .all_nodes()
            .find(|node| node.name.starts_with("virtio_mmio"))
            .expect("the tree carries the transport node");
        let Some(NodeInterrupt::Shared(interrupt)) = node_interrupt_kind(&fdt, &node) else {
            panic!("a shared peripheral interrupt is reported as shared");
        };
        assert_eq!(interrupt.trigger, Some(InterruptTrigger::Level));
    }

    /// `scause` interrupt 11, machine external.
    const MACHINE_EXTERNAL_CAUSE: u32 = 11;

    /// One `/cpus/cpu@N` node with its hart-local interrupt controller,
    /// the way both trees below write them, plus whatever one-cell
    /// properties the tree puts on its cpu nodes besides.
    fn hart(builder: &mut DtbBuilder, hart: u32, cpu_cells: &[(&str, u32)], intc_phandle: u32) {
        builder.begin_node(&alloc::format!("cpu@{hart}"));
        builder
            .string("compatible", "riscv")
            .string("status", "okay")
            .cells("reg", &[hart])
            .string("device_type", "cpu");
        for &(name, value) in cpu_cells {
            builder.cells(name, &[value]);
        }
        builder.begin_node("interrupt-controller");
        builder
            .cells("phandle", &[intc_phandle])
            .string("compatible", "riscv,cpu-intc")
            .property("interrupt-controller", &[])
            .cells("#interrupt-cells", &[1]);
        builder.end_node();
        builder.end_node();
    }

    /// QEMU's RISC-V `virt` under TCG with `-smp 4,sockets=2` and a NUMA
    /// node per socket, as `hw/riscv/virt.c` (v9.2.0) writes it.
    ///
    /// Each socket has its own PLIC, whose `interrupts-extended` gives
    /// each of that socket's harts a machine-external and then a
    /// supervisor-external context. The UART is wired to socket 0's
    /// PLIC and every virtio-mmio transport to socket 1's. Phandles are
    /// the ones QEMU hands out: socket 1 first and each socket's harts
    /// from the last down, cpu then its `interrupt-controller`, then
    /// the PLICs in the same socket order.
    fn qemu_virt_two_socket_tree() -> Vec<u8> {
        const SOCKET0_PLIC_PHANDLE: u32 = 10;
        const SOCKET1_PLIC_PHANDLE: u32 = 9;
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder
            .cells("#address-cells", &[2])
            .cells("#size-cells", &[2])
            .string("compatible", "riscv-virtio")
            .string("model", "riscv-virtio,qemu");
        builder.begin_node("cpus");
        builder
            .cells("timebase-frequency", &[10_000_000])
            .cells("#size-cells", &[0])
            .cells("#address-cells", &[1]);
        // (hart, socket, cpu phandle, intc phandle)
        for (id, socket, cpu_phandle, intc_phandle) in
            [(0, 0, 7, 8), (1, 0, 5, 6), (2, 1, 3, 4), (3, 1, 1, 2)]
        {
            hart(
                &mut builder,
                id,
                &[("numa-node-id", socket), ("phandle", cpu_phandle)],
                intc_phandle,
            );
        }
        builder.begin_node("cpu-map");
        for (cluster, cores) in [("cluster0", [7, 5]), ("cluster1", [3, 1])] {
            builder.begin_node(cluster);
            for (core, cpu) in cores.into_iter().enumerate() {
                builder.begin_node(&alloc::format!("core{core}"));
                builder.cells("cpu", &[cpu]);
                builder.end_node();
            }
            builder.end_node();
        }
        builder.end_node();
        builder.end_node();
        builder.begin_node("soc");
        builder
            .cells("#address-cells", &[2])
            .cells("#size-cells", &[2])
            .string("compatible", "simple-bus")
            .property("ranges", &[]);
        // (base, socket, phandle, [intc of the socket's first hart, of its second])
        for (base, socket, phandle, intcs) in [
            (0x0c00_0000, 0, SOCKET0_PLIC_PHANDLE, [8, 6]),
            (0x0c60_0000, 1, SOCKET1_PLIC_PHANDLE, [4, 2]),
        ] {
            builder.begin_node(&alloc::format!("plic@{base:x}"));
            let mut contexts = Vec::new();
            for intc in intcs {
                contexts.extend_from_slice(&[
                    intc,
                    MACHINE_EXTERNAL_CAUSE,
                    intc,
                    SUPERVISOR_EXTERNAL_CAUSE,
                ]);
            }
            builder
                .cells("#interrupt-cells", &[1])
                .cells("#address-cells", &[0])
                .property("compatible", b"sifive,plic-1.0.0\0riscv,plic0\0")
                .property("interrupt-controller", &[])
                .cells("interrupts-extended", &contexts)
                // `VIRT_PLIC_SIZE(VIRT_CPUS_MAX * 2)`: a 0x200000-byte
                // header plus 0x1000 bytes for each of 1024 contexts.
                .cells("reg", &[0, base, 0, 0x60_0000])
                // `VIRT_IRQCHIP_NUM_SOURCES - 1`.
                .cells("riscv,ndev", &[0x5f])
                .cells("numa-node-id", &[socket])
                .cells("phandle", &[phandle]);
            builder.end_node();
        }
        builder.begin_node("serial@10000000");
        builder
            .string("compatible", "ns16550a")
            .cells("reg", &[0, 0x1000_0000, 0, 0x100])
            .cells("clock-frequency", &[3_686_400])
            .cells("interrupt-parent", &[SOCKET0_PLIC_PHANDLE])
            .cells("interrupts", &[10]);
        builder.end_node();
        for (base, source) in [(0x1000_1000, 1), (0x1000_8000, 8)] {
            builder.begin_node(&alloc::format!("virtio_mmio@{base:x}"));
            builder
                .string("compatible", "virtio,mmio")
                .cells("reg", &[0, base, 0, 0x1000])
                .cells("interrupt-parent", &[SOCKET1_PLIC_PHANDLE])
                .cells("interrupts", &[source]);
            builder.end_node();
        }
        builder.end_node();
        builder.end_node();
        builder.finish()
    }

    const QEMU_SOCKET0_PLIC: InterruptController = InterruptController { base: 0x0c00_0000 };
    const QEMU_SOCKET1_PLIC: InterruptController = InterruptController { base: 0x0c60_0000 };

    #[test]
    fn each_device_resolves_to_the_plic_its_interrupt_parent_names() {
        let blob = qemu_virt_two_socket_tree();
        let fdt = Fdt::new(&blob).expect("two-socket tree parses");
        let serial = fdt
            .find_node("/soc/serial@10000000")
            .expect("the tree carries the UART");
        assert_eq!(
            node_interrupt(&fdt, &serial),
            Some(MmioInterrupt {
                controller: QEMU_SOCKET0_PLIC,
                number: 10,
                trigger: None,
            })
        );
        let transports: Vec<_> = mmio_candidates(&fdt)
            .map(|candidate| (candidate.base, candidate.interrupt))
            .collect();
        assert_eq!(
            transports,
            [
                (
                    0x1000_1000,
                    Some(MmioInterrupt {
                        controller: QEMU_SOCKET1_PLIC,
                        number: 1,
                        trigger: None,
                    })
                ),
                (
                    0x1000_8000,
                    Some(MmioInterrupt {
                        controller: QEMU_SOCKET1_PLIC,
                        number: 8,
                        trigger: None,
                    })
                ),
            ]
        );
    }

    /// The bootstrap hart takes its own socket's PLIC; the other
    /// socket's goes to that socket's lowest hart. Every context named
    /// is the second of its hart's pair, the supervisor one.
    #[test]
    fn each_plic_is_taken_by_a_hart_with_a_supervisor_context_in_it() {
        let blob = qemu_virt_two_socket_tree();
        let fdt = Fdt::new(&blob).expect("two-socket tree parses");
        let plics: Vec<_> = plic_descriptions(&fdt, 0).collect();
        assert_eq!(
            plics,
            [
                PlicDescription {
                    controller: QEMU_SOCKET0_PLIC,
                    taking: Some(PlicHartContext {
                        hart: 0,
                        context: 1,
                    }),
                },
                PlicDescription {
                    controller: QEMU_SOCKET1_PLIC,
                    taking: Some(PlicHartContext {
                        hart: 2,
                        context: 1,
                    }),
                },
            ]
        );
    }

    #[test]
    fn a_bootstrap_hart_on_the_second_socket_takes_that_socket_s_plic() {
        let blob = qemu_virt_two_socket_tree();
        let fdt = Fdt::new(&blob).expect("two-socket tree parses");
        let plics: Vec<_> = plic_descriptions(&fdt, 3).collect();
        assert_eq!(
            plics,
            [
                PlicDescription {
                    controller: QEMU_SOCKET0_PLIC,
                    taking: Some(PlicHartContext {
                        hart: 0,
                        context: 1,
                    }),
                },
                PlicDescription {
                    controller: QEMU_SOCKET1_PLIC,
                    taking: Some(PlicHartContext {
                        hart: 3,
                        context: 3,
                    }),
                },
            ]
        );
    }

    /// StarFive's JH7110 (`arch/riscv/boot/dts/starfive/jh7110.dtsi` in
    /// Linux v6.12), cut to the nodes interrupt routing reads.
    ///
    /// `soc` names the PLIC as the interrupt parent of all its children
    /// and the UART declares only its source. Hart 0, the S7 monitor
    /// core, has a machine-external context and no supervisor one, so
    /// the PLIC's context numbers and its supervisor contexts are out of
    /// step. Phandles are dtc's to assign; these are this tree's own.
    fn jh7110_tree() -> Vec<u8> {
        const PLIC_PHANDLE: u32 = 6;
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder
            .cells("#address-cells", &[2])
            .cells("#size-cells", &[2]);
        builder.begin_node("cpus");
        builder
            .cells("#address-cells", &[1])
            .cells("#size-cells", &[0]);
        for id in 0..5 {
            hart(&mut builder, id, &[], id + 1);
        }
        builder.end_node();
        builder.begin_node("soc");
        builder
            .string("compatible", "simple-bus")
            .cells("interrupt-parent", &[PLIC_PHANDLE])
            .cells("#address-cells", &[2])
            .cells("#size-cells", &[2])
            .property("ranges", &[]);
        builder.begin_node("interrupt-controller@c000000");
        let mut contexts = alloc::vec![1, MACHINE_EXTERNAL_CAUSE];
        for intc in 2..=5 {
            contexts.extend_from_slice(&[
                intc,
                MACHINE_EXTERNAL_CAUSE,
                intc,
                SUPERVISOR_EXTERNAL_CAUSE,
            ]);
        }
        builder
            .property("compatible", b"starfive,jh7110-plic\0sifive,plic-1.0.0\0")
            .cells("reg", &[0, 0x0c00_0000, 0, 0x400_0000])
            .cells("interrupts-extended", &contexts)
            .property("interrupt-controller", &[])
            .cells("#interrupt-cells", &[1])
            .cells("#address-cells", &[0])
            .cells("riscv,ndev", &[136])
            .cells("phandle", &[PLIC_PHANDLE]);
        builder.end_node();
        builder.begin_node("serial@10000000");
        builder
            .property("compatible", b"starfive,jh7110-uart\0snps,dw-apb-uart\0")
            .cells("reg", &[0, 0x1000_0000, 0, 0x1_0000])
            .cells("interrupts", &[32]);
        builder.end_node();
        builder.end_node();
        builder.end_node();
        builder.finish()
    }

    const JH7110_PLIC: InterruptController = InterruptController { base: 0x0c00_0000 };

    #[test]
    fn a_device_inherits_the_interrupt_parent_its_bus_names() {
        let blob = jh7110_tree();
        let fdt = Fdt::new(&blob).expect("JH7110 tree parses");
        let serial = fdt
            .find_node("/soc/serial@10000000")
            .expect("the tree carries the UART");
        assert_eq!(
            node_interrupt(&fdt, &serial),
            Some(MmioInterrupt {
                controller: JH7110_PLIC,
                number: 32,
                trigger: None,
            })
        );
    }

    /// Hart 1's supervisor context is the PLIC's third, after hart 0's
    /// lone machine context and hart 1's own; hart 4's is the ninth.
    #[test]
    fn a_hart_without_a_supervisor_context_never_takes_the_plic() {
        let blob = jh7110_tree();
        let fdt = Fdt::new(&blob).expect("JH7110 tree parses");
        let taking = |bootstrap_hart| {
            let plics: Vec<_> = plic_descriptions(&fdt, bootstrap_hart).collect();
            assert_eq!(plics.len(), 1);
            assert_eq!(plics[0].controller, JH7110_PLIC);
            plics[0].taking
        };
        let hart1 = Some(PlicHartContext {
            hart: 1,
            context: 2,
        });
        assert_eq!(taking(1), hart1);
        assert_eq!(taking(0), hart1);
        assert_eq!(
            taking(4),
            Some(PlicHartContext {
                hart: 4,
                context: 8,
            })
        );
    }
}
