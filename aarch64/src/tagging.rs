//! Memory tagging on AArch64: decoding a synchronous tag check fault, the
//! canonical form of a fault address, and the fatal report.
//!
//! # Concurrency contract
//!
//! Everything here is a pure function of its arguments, except
//! [`load_allocation_tag`], which reads the executing processor's tag
//! storage for one granule. Nothing allocates, takes a lock or touches
//! another processor, so any of it may run in the synchronous handler.

use core::arch::asm;

use helios_hal::tagging::Tag;

use crate::cfi::{ESR_EC_MASK, ESR_EC_SHIFT};

/// `ESR_ELx.EC` of a data abort taken from a lower and from the same
/// exception level.
const ESR_EC_DATA_ABORT_LOWER: usize = 0x24;
const ESR_EC_DATA_ABORT_SAME: usize = 0x25;
/// `ESR_ELx.ISS[5:0]`, the data fault status code (DFSC).
const ESR_DFSC_MASK: usize = 0x3f;
/// The DFSC of a synchronous tag check fault.
const DFSC_TAG_CHECK_FAULT: usize = 0b010001;
/// `ESR_ELx.ISS.WnR`: the abort was caused by a write.
const ESR_WNR: usize = 1 << 6;

/// The lowest bit of the logical tag in a pointer (and in FAR), and the tag's
/// width. The architecture places the logical tag in bits `[59:56]`.
const LOGICAL_TAG_SHIFT: usize = 56;
const LOGICAL_TAG_MASK: usize = 0xf;
/// Bit that selects the translation table, and so the value every bit of a
/// canonical address's top byte copies.
const ADDRESS_SPACE_SELECT_BIT: u32 = 55;
/// Bits of the pointer's top byte, which a processor with top-byte-ignore
/// does not use to translate.
const TOP_BYTE_BITS: usize = 8;

/// `TCR_EL1.TBI0` and `TCR_EL1.TBI1`: the top byte of an address translated
/// through `TTBR0_EL1` or `TTBR1_EL1` is ignored.
const TCR_TBI0: u64 = 1 << 37;
const TCR_TBI1: u64 = 1 << 38;

/// Bytes one allocation tag covers.
const GRANULE_BYTES: usize = 16;

/// Whether `esr` describes a synchronous tag check fault: a data abort whose
/// fault status is the tag check fault code.
pub(crate) fn is_tag_check_fault(esr: usize) -> bool {
    matches!(
        (esr >> ESR_EC_SHIFT) & ESR_EC_MASK,
        ESR_EC_DATA_ABORT_LOWER | ESR_EC_DATA_ABORT_SAME
    ) && esr & ESR_DFSC_MASK == DFSC_TAG_CHECK_FAULT
}

/// `addr` with its logical tag removed: the top byte, which an address with
/// top-byte-ignore enabled does not translate by, takes the value of bit 55
/// again. This is what `hal::tagging::AllocationTags::strip` is on AArch64.
pub(crate) fn strip(addr: usize) -> usize {
    (((addr << TOP_BYTE_BITS) as isize) >> TOP_BYTE_BITS) as usize
}

/// The address the processor translated when it reported `far` for a data
/// abort: `far` without its top byte where `tcr` (`TCR_EL1`) ignores the top
/// byte of that half of the address space, and `far` itself where it does
/// not, because there a top byte that differs from bit 55 is why the access
/// faulted and no mapping answers for it.
pub(crate) fn translated_address(far: usize, tcr: u64) -> usize {
    let top_byte_ignored = if far & (1 << ADDRESS_SPACE_SELECT_BIT) == 0 {
        TCR_TBI0
    } else {
        TCR_TBI1
    };
    if tcr & top_byte_ignored == 0 {
        far
    } else {
        strip(far)
    }
}

/// The logical tag a faulting pointer carried: FAR bits `[59:56]`, which are
/// valid for a tag check fault whether or not FEAT_MTE_TAGGED_FAR is
/// implemented.
pub(crate) fn logical_tag(far: usize) -> Tag {
    Tag::new(((far >> LOGICAL_TAG_SHIFT) & LOGICAL_TAG_MASK) as u8)
}

/// The start of the granule that holds `canonical`.
pub(crate) fn granule_start(canonical: usize) -> usize {
    canonical & !(GRANULE_BYTES - 1)
}

/// Whether the abort was caused by a write.
fn is_write(esr: usize) -> bool {
    esr & ESR_WNR != 0
}

/// The allocation tag stored with the granule at `granule`, from `LDG`.
///
/// # Safety
///
/// The executing processor implements FEAT_MTE2, so `LDG` is defined, and
/// `granule` is the 16-byte aligned canonical address of memory mapped with a
/// tag-capable attribute, which makes the instruction read tag storage and
/// never fault.
unsafe fn load_allocation_tag(granule: usize) -> Tag {
    let mut tagged = granule;
    // SAFETY: `LDG` replaces `tagged[59:56]` with the allocation tag of the
    // granule and reads no data; the caller guarantees it is defined.
    unsafe {
        asm!(
            ".arch_extension memtag",
            "ldg {tagged}, [{tagged}]",
            tagged = inout(reg) tagged,
            options(nostack, preserves_flags),
        );
    }
    logical_tag(tagged)
}

/// Panics with the report of a synchronous tag check fault: the access that
/// failed, the tag its pointer carried, and the tag the granule holds.
///
/// A guest's linear memory is reached through untagged pointers, so no guest
/// access raises a tag check fault; one is a defect in the kernel or the
/// runtime, and nothing is handed to the runtime.
///
/// # Safety
///
/// The executing processor implements FEAT_MTE2, as a processor that took a
/// tag check fault does, and the faulting address is mapped with a tag-capable
/// attribute, as a tag mismatch implies.
pub(crate) unsafe fn report_tag_check_fault(esr: usize, far: usize, elr: u64) -> ! {
    let canonical = strip(far);
    let logical = logical_tag(far);
    let granule = granule_start(canonical);
    // SAFETY: both are the caller's contract, and `granule` is aligned by
    // construction.
    let allocation = unsafe { load_allocation_tag(granule) };
    let access = if is_write(esr) { "write" } else { "read" };
    panic!(
        "AArch64 synchronous tag check fault: {access} at {canonical:#x} through a pointer \
         tagged {logical:#x}; a faulting granule {granule:#x}..{end:#x} holds tag \
         {allocation:#x}; esr={esr:#x} elr={elr:#x} far={far:#x}",
        logical = logical.raw(),
        end = granule + GRANULE_BYTES,
        allocation = allocation.raw(),
    )
}
