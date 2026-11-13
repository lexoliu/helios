//! The bootstrap processor's memory type range registers, carried to every
//! processor the kernel starts.
//!
//! Firmware programs the MTRRs of the processors it runs on. A processor
//! the kernel starts itself — an application processor at boot, a
//! hot-added or re-added vCPU (#497), a slot not present at boot (#493) —
//! leaves reset with every range register cleared, and where the host
//! honours guest MTRRs it would run with memory uncached. The bootstrap
//! processor captures its configuration once ([`Mtrrs::capture`]) into the
//! boot context, and every started processor loads it ([`Mtrrs::apply`])
//! before it runs kernel code.

use core::arch::x86_64::__cpuid;

use x86::msr::{
    IA32_MTRR_DEF_TYPE, IA32_MTRR_FIX4K_C0000, IA32_MTRR_FIX4K_C8000, IA32_MTRR_FIX4K_D0000,
    IA32_MTRR_FIX4K_D8000, IA32_MTRR_FIX4K_E0000, IA32_MTRR_FIX4K_E8000, IA32_MTRR_FIX4K_F0000,
    IA32_MTRR_FIX4K_F8000, IA32_MTRR_FIX16K_80000, IA32_MTRR_FIX16K_A0000, IA32_MTRR_FIX64K_00000,
    IA32_MTRR_PHYSBASE0, IA32_MTRR_PHYSMASK0, IA32_MTRRCAP, rdmsr, wrmsr,
};
use x86_64::registers::control::{Cr0, Cr0Flags, Cr3, Cr4, Cr4Flags};

/// `CPUID.01H:EDX` bit 12, "MTRR: Memory Type Range Registers" (Intel SDM
/// Vol. 2A, Table 3-11 "Feature Information Returned in the EDX Register").
const CPUID_01_EDX_MTRR: u32 = 1 << 12;

/// `IA32_MTRRCAP` bits 7:0, `VCNT`: the number of variable range
/// registers (Intel SDM Vol. 3A §12.11.1 "MTRR Feature Identification",
/// Figure 12-5 "IA32_MTRRCAP Register").
const MTRRCAP_VCNT_MASK: u64 = 0xff;

/// `IA32_MTRRCAP` bit 8, `FIX`: the fixed range registers are supported
/// (Intel SDM Vol. 3A §12.11.1, Figure 12-5).
const MTRRCAP_FIX: u64 = 1 << 8;

/// `IA32_MTRR_DEF_TYPE` bit 11, `E`: MTRRs enabled. Clear, every
/// physical address is uncacheable (Intel SDM Vol. 3A §12.11.2.1
/// "IA32_MTRR_DEF_TYPE MSR", Figure 12-8).
const DEF_TYPE_ENABLE: u64 = 1 << 11;

/// The variable range registers occupy `IA32_MTRR_PHYSBASEn` /
/// `IA32_MTRR_PHYSMASKn` pairs from MSR `0x200` and end before the first
/// fixed range register at `0x250` (Intel SDM Vol. 4, Table 2-2, MSRs
/// `200H`–`26FH`), which bounds `VCNT`.
const MAX_VARIABLE_RANGES: usize = (IA32_MTRR_FIX64K_00000 - IA32_MTRR_PHYSBASE0) as usize / 2;

/// The eleven fixed range registers, in address order: one 64 KiB-granular
/// (`00000`–`7FFFF`), two 16 KiB-granular (`80000`–`BFFFF`) and eight
/// 4 KiB-granular (`C0000`–`FFFFF`) (Intel SDM Vol. 3A §12.11.2.2 "Fixed
/// Range MTRRs", Table 12-9 "Address Mapping for Fixed-Range MTRRs").
const FIXED_RANGE_MSRS: [u32; 11] = [
    IA32_MTRR_FIX64K_00000,
    IA32_MTRR_FIX16K_80000,
    IA32_MTRR_FIX16K_A0000,
    IA32_MTRR_FIX4K_C0000,
    IA32_MTRR_FIX4K_C8000,
    IA32_MTRR_FIX4K_D0000,
    IA32_MTRR_FIX4K_D8000,
    IA32_MTRR_FIX4K_E0000,
    IA32_MTRR_FIX4K_E8000,
    IA32_MTRR_FIX4K_F0000,
    IA32_MTRR_FIX4K_F8000,
];

/// What `IA32_MTRRCAP` reports about the range registers present.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Capability {
    variable_ranges: usize,
    fixed_ranges: bool,
}

impl Capability {
    /// Decodes an `IA32_MTRRCAP` value. Write-combining (bit 10) and SMRR
    /// (bit 11) are not read: write-combining is a memory type a range
    /// register may hold, and the SMRRs belong to system management mode,
    /// which the kernel does not run in and cannot write (Intel SDM Vol. 3A
    /// §12.11.1; §32.4.2.1 "SMRAM Caching").
    fn decode(mtrrcap: u64) -> Self {
        let variable_ranges = (mtrrcap & MTRRCAP_VCNT_MASK) as usize;
        assert!(
            variable_ranges <= MAX_VARIABLE_RANGES,
            "x86 IA32_MTRRCAP reports {variable_ranges} variable range registers; the MSR \
             space before the fixed ranges holds {MAX_VARIABLE_RANGES} pairs"
        );
        Self {
            variable_ranges,
            fixed_ranges: mtrrcap & MTRRCAP_FIX != 0,
        }
    }
}

/// One variable range: the `IA32_MTRR_PHYSBASEn` and `IA32_MTRR_PHYSMASKn`
/// values (Intel SDM Vol. 3A §12.11.2.3 "Variable Range MTRRs").
#[derive(Clone, Copy)]
struct VariableRange {
    base: u64,
    mask: u64,
}

/// The bootstrap processor's MTRR configuration: every register
/// `IA32_MTRRCAP` reports, read once. Immutable once captured.
pub(crate) struct MtrrConfiguration {
    capability: Capability,
    def_type: u64,
    fixed: [u64; FIXED_RANGE_MSRS.len()],
    variable: [VariableRange; MAX_VARIABLE_RANGES],
}

/// The bootstrap processor's MTRRs. `None` when `CPUID.01H:EDX[12]` is
/// clear: there are no range registers to carry and every processor is in
/// the same state, so [`Mtrrs::apply`] has nothing to do.
pub(crate) struct Mtrrs(Option<MtrrConfiguration>);

impl Mtrrs {
    /// Reads the calling processor's MTRRs. Called once, on the
    /// bootstrap processor, while the registers still hold what firmware
    /// programmed.
    pub(crate) fn capture() -> Self {
        if __cpuid(1).edx & CPUID_01_EDX_MTRR == 0 {
            return Self(None);
        }
        // SAFETY: `IA32_MTRRCAP` exists because `CPUID.01H:EDX[12]` is set.
        let capability = Capability::decode(unsafe { rdmsr(IA32_MTRRCAP) });
        let mut configuration = MtrrConfiguration {
            capability,
            // SAFETY: `IA32_MTRR_DEF_TYPE` exists with the MTRRs.
            def_type: unsafe { rdmsr(IA32_MTRR_DEF_TYPE) },
            fixed: [0; FIXED_RANGE_MSRS.len()],
            variable: [VariableRange { base: 0, mask: 0 }; MAX_VARIABLE_RANGES],
        };
        if capability.fixed_ranges {
            for (value, msr) in configuration.fixed.iter_mut().zip(FIXED_RANGE_MSRS) {
                // SAFETY: `IA32_MTRRCAP.FIX` is set, so the fixed ranges exist.
                *value = unsafe { rdmsr(msr) };
            }
        }
        for (index, range) in configuration.variable[..capability.variable_ranges]
            .iter_mut()
            .enumerate()
        {
            let (base_msr, mask_msr) = variable_range_msrs(index);
            // SAFETY: `index` is below `IA32_MTRRCAP.VCNT`.
            unsafe {
                range.base = rdmsr(base_msr);
                range.mask = rdmsr(mask_msr);
            }
        }
        Self(Some(configuration))
    }

    /// Loads the captured configuration into the calling processor, which
    /// must not yet have run anything that depends on its memory types.
    ///
    /// Follows the procedure of Intel SDM Vol. 3A §12.11.8 "MTRR
    /// Considerations in MP Systems" for the one processor that changes:
    /// the others already hold this configuration, so the broadcast and
    /// rendezvous steps that keep processors from disagreeing while
    /// another updates have nothing to wait for. The steps, in the
    /// manual's order: disable interrupts; enter the no-fill cache mode
    /// (`CR0.CD` = 1, `CR0.NW` = 0); `WBINVD`; clear `CR4.PGE`; flush the
    /// TLB; disable the MTRRs (`IA32_MTRR_DEF_TYPE.E` = 0); write the
    /// fixed and variable ranges and the default type; `WBINVD` and flush
    /// the TLB again; restore `CR0`, `CR4.PGE` and the interrupt flag.
    pub(crate) fn apply(&self) {
        let Self(Some(configuration)) = self else {
            return;
        };
        let capability = Capability::decode(
            // SAFETY: the bootstrap processor reported MTRR support, and
            // every processor of the machine answers the same (the ISA
            // answers are compared on the same path).
            unsafe { rdmsr(IA32_MTRRCAP) },
        );
        assert_eq!(
            capability, configuration.capability,
            "x86 processor reports MTRR capabilities different from the bootstrap processor's"
        );
        x86_64::instructions::interrupts::without_interrupts(|| {
            let cr0 = Cr0::read();
            let cr4 = Cr4::read();
            // SAFETY: the sequence is the SDM's; it ends with the caches
            // flushed, the TLB flushed and `CR0`/`CR4` as they were, and
            // runs with interrupts masked, so nothing observes the
            // intermediate memory types.
            unsafe {
                // `CD` = 1, `NW` = 0: no-fill cache mode.
                Cr0::write((cr0 | Cr0Flags::CACHE_DISABLE) - Cr0Flags::NOT_WRITE_THROUGH);
                flush_caches();
                if cr4.contains(Cr4Flags::PAGE_GLOBAL) {
                    Cr4::write(cr4 - Cr4Flags::PAGE_GLOBAL);
                }
                flush_tlb();
                wrmsr(
                    IA32_MTRR_DEF_TYPE,
                    configuration.def_type & !DEF_TYPE_ENABLE,
                );
                if capability.fixed_ranges {
                    for (value, msr) in configuration.fixed.iter().zip(FIXED_RANGE_MSRS) {
                        wrmsr(msr, *value);
                    }
                }
                for (index, range) in configuration.variable[..capability.variable_ranges]
                    .iter()
                    .enumerate()
                {
                    let (base_msr, mask_msr) = variable_range_msrs(index);
                    wrmsr(base_msr, range.base);
                    wrmsr(mask_msr, range.mask);
                }
                wrmsr(IA32_MTRR_DEF_TYPE, configuration.def_type);
                flush_caches();
                flush_tlb();
                Cr0::write(cr0);
                if cr4.contains(Cr4Flags::PAGE_GLOBAL) {
                    Cr4::write(cr4);
                }
            }
        });
    }
}

/// The `IA32_MTRR_PHYSBASEn` and `IA32_MTRR_PHYSMASKn` MSR numbers of
/// variable range `index`: consecutive pairs from `0x200` (Intel SDM
/// Vol. 3A §12.11.2.3).
fn variable_range_msrs(index: usize) -> (u32, u32) {
    let base = IA32_MTRR_PHYSBASE0 + 2 * index as u32;
    (base, base + (IA32_MTRR_PHYSMASK0 - IA32_MTRR_PHYSBASE0))
}

/// Writes back and invalidates every cache of this processor (`WBINVD`,
/// Intel SDM Vol. 2B).
fn flush_caches() {
    // SAFETY: `WBINVD` is privileged and has no operand; the kernel runs
    // in ring 0.
    unsafe { core::arch::asm!("wbinvd", options(nostack, preserves_flags)) };
}

/// Flushes every TLB entry of this processor, global entries included
/// when `CR4.PGE` is clear, by rewriting `CR3` (Intel SDM Vol. 3A §5.10.4.1
/// "Operations that Invalidate TLBs and Paging-Structure Caches").
fn flush_tlb() {
    let (frame, flags) = Cr3::read();
    // SAFETY: writing back the value just read changes no translation.
    unsafe { Cr3::write(frame, flags) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_decodes_variable_count_and_fixed_support() {
        // VCNT = 8, FIX = 1, WC = 1, SMRR = 1: the common Intel value.
        let capability = Capability::decode(0xd08);
        assert_eq!(capability.variable_ranges, 8);
        assert!(capability.fixed_ranges);
        // VCNT = 10, FIX = 0.
        let capability = Capability::decode(0x0a);
        assert_eq!(capability.variable_ranges, 10);
        assert!(!capability.fixed_ranges);
    }

    #[test]
    fn variable_range_msrs_are_consecutive_pairs() {
        assert_eq!(variable_range_msrs(0), (0x200, 0x201));
        assert_eq!(variable_range_msrs(9), (0x212, 0x213));
    }

    #[test]
    #[should_panic(expected = "variable range registers")]
    fn capability_rejects_a_count_beyond_the_msr_space() {
        Capability::decode(0xff);
    }
}
