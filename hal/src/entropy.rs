//! Platform entropy contracts.
//!
//! `Cpu::fill_entropy` covers the source a processor carries itself —
//! `RNDR`, `RDRAND`, or the host OS on a hosted backend. Firmware is the
//! other pre-boot source: a device tree's `/chosen/rng-seed` is filled
//! in by the bootloader from its own entropy pool, and is the only
//! cryptographic material some targets have before their entropy device
//! comes up.

use fdt::Fdt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntropyQuality {
    Cryptographic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntropyUnavailable;

/// The seed firmware left in the device tree's `/chosen/rng-seed`.
///
/// The property is optional; a tree without it simply contributes no
/// firmware material. Every backend that boots with a device tree reads
/// it through here so the property name and the "absent is fine, empty
/// is a broken tree" rule live in one place.
pub fn device_tree_seed<'a>(fdt: &Fdt<'a>) -> Option<&'a [u8]> {
    let seed = fdt.find_node("/chosen")?.property("rng-seed")?.value;
    assert!(
        !seed.is_empty(),
        "device tree /chosen/rng-seed is present but empty"
    );
    Some(seed)
}
