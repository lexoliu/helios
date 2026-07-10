//! FDT-driven discovery of `virtio,mmio` platform devices.
//!
//! Every MMIO-bus backend walks the same device-tree shape; only the
//! address translation in front of the probe differs per architecture.
//! The walk lives here and backends probe the candidates at whatever
//! virtual address their MMIO mapping provides.

use core::num::NonZeroU32;

use fdt::Fdt;

use crate::DeviceType;

const REG_MAGIC_VALUE: usize = 0x000;
const REG_VERSION: usize = 0x004;
const REG_DEVICE_ID: usize = 0x008;
const MAGIC_VALUE: u32 = 0x7472_6976;
const MODERN_VERSION: u32 = 2;

/// One `virtio,mmio` node: physical base address, MMIO window size, and
/// the first declared interrupt source, if any.
#[derive(Clone, Copy, Debug)]
pub struct MmioCandidate {
    pub base: usize,
    pub size: usize,
    pub irq: Option<NonZeroU32>,
}

/// Iterates every `virtio,mmio` node in `fdt`.
///
/// Reg cells are parsed raw so the walk is independent of the parent
/// bus `#address-cells`; a malformed cell width on a matching node
/// panics rather than silently skipping the device.
pub fn mmio_candidates<'f>(fdt: &'f Fdt<'f>) -> impl Iterator<Item = MmioCandidate> + 'f {
    fdt.all_nodes().filter_map(|node| {
        if !node
            .compatible()
            .is_some_and(|compatible| compatible.all().any(|entry| entry == "virtio,mmio"))
        {
            return None;
        }
        let region = node.raw_reg().and_then(|mut regions| regions.next())?;
        let base = reg_cells_to_usize(region.address, "virtio,mmio reg address");
        let size = reg_cells_to_usize(region.size, "virtio,mmio reg size");
        let irq = node
            .interrupts()
            .and_then(|mut interrupts| interrupts.next())
            .and_then(|irq| u32::try_from(irq).ok())
            .and_then(NonZeroU32::new);
        Some(MmioCandidate { base, size, irq })
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
