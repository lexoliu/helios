//! XSAVE-managed extended register state.
//!
//! Wasmtime-generated code uses every vector extension the artifact's
//! Cranelift flags name (`helios_artifact::cwasm_target_cranelift_flags`),
//! so the kernel has to keep the whole extended register file — AVX's
//! upper YMM halves, AVX-512's opmask and ZMM state — alive across every
//! exception and interrupt that returns to the interrupted code. The
//! entry stubs in `exceptions.S` do that with `xsave64`/`xrstor64`, which
//! only execute once the processor has been told which components it
//! manages: `CR4.OSXSAVE` set and XCR0 programmed, per processor, before
//! its IDT is installed.

use core::arch::asm;
use core::arch::x86_64::{__cpuid, __cpuid_count};

/// Bytes `exceptions.S` reserves on the stack for one XSAVE area; the
/// assembly's `HELIOS_X86_XSAVE_AREA_BYTES` is the same number.
const XSAVE_AREA_BYTES: usize = 4096;

/// XCR0 bits: x87 state, SSE state, AVX upper halves, and the three
/// AVX-512 components (opmask, ZMM_Hi256, Hi16_ZMM).
const XCR0_X87: u64 = 1 << 0;
const XCR0_SSE: u64 = 1 << 1;
const XCR0_AVX: u64 = 1 << 2;
const XCR0_OPMASK: u64 = 1 << 5;
const XCR0_ZMM_HI256: u64 = 1 << 6;
const XCR0_HI16_ZMM: u64 = 1 << 7;
const XCR0_AVX512: u64 = XCR0_OPMASK | XCR0_ZMM_HI256 | XCR0_HI16_ZMM;

const CPUID_1_ECX_XSAVE: u32 = 1 << 26;
const CPUID_1_ECX_AVX: u32 = 1 << 28;
const CR4_OSXSAVE: u64 = 1 << 18;

/// Enables XSAVE on the executing processor and programs XCR0 with the
/// x87, SSE and AVX components, plus the AVX-512 components when the
/// processor has them.
///
/// Runs on every processor before its IDT is installed, because the
/// entry stubs' `xsave64` raises `#UD` until `CR4.OSXSAVE` is set. A
/// processor without XSAVE or AVX cannot run the artifacts this kernel
/// compiles (their Cranelift flags require AVX2), so it is refused here,
/// at bring-up, rather than at the first `#UD` inside guest code.
pub(crate) fn enable_for_current_processor() {
    let leaf1 = __cpuid(1);
    assert!(
        leaf1.ecx & CPUID_1_ECX_XSAVE != 0,
        "x86 processor has no XSAVE: the kernel's exception entry saves extended state with it"
    );
    assert!(
        leaf1.ecx & CPUID_1_ECX_AVX != 0,
        "x86 processor has no AVX: compiled artifacts require AVX2 (has_avx2 in the Cranelift \
         flags)"
    );
    let supported = {
        let leaf = __cpuid_count(0xD, 0);
        (u64::from(leaf.edx) << 32) | u64::from(leaf.eax)
    };
    assert!(
        supported & (XCR0_X87 | XCR0_SSE | XCR0_AVX) == (XCR0_X87 | XCR0_SSE | XCR0_AVX),
        "x86 processor XSAVE cannot manage x87/SSE/AVX state (supported XCR0 bits {supported:#x})"
    );
    // AVX-512 state is enabled only as a whole: a processor that reports
    // one of its three components without the others is not one this
    // kernel has met, and enabling a subset would leave `xrstor64`
    // initialising registers the guest still owns.
    let avx512 = if supported & XCR0_AVX512 == XCR0_AVX512 {
        XCR0_AVX512
    } else {
        0
    };
    let xcr0 = XCR0_X87 | XCR0_SSE | XCR0_AVX | avx512;

    // SAFETY: setting CR4.OSXSAVE and XCR0 only widens the register state
    // the processor tracks; nothing has used AVX registers on this
    // processor yet, and every consumer of the extended state (the entry
    // stubs, guest code) runs after this function returns.
    unsafe {
        asm!(
            "mov {tmp}, cr4",
            "or {tmp}, {osxsave}",
            "mov cr4, {tmp}",
            tmp = out(reg) _,
            osxsave = const CR4_OSXSAVE,
            options(nomem, nostack, preserves_flags),
        );
        asm!(
            "xsetbv",
            in("ecx") 0_u32,
            in("eax") xcr0 as u32,
            in("edx") (xcr0 >> 32) as u32,
            options(nomem, nostack, preserves_flags),
        );
    }

    // CPUID.(EAX=0DH,ECX=0):EBX is the XSAVE area size for the XCR0 now
    // in effect; the entry stubs reserve a fixed area, so the processor
    // has to fit in it.
    let area_bytes = __cpuid_count(0xD, 0).ebx as usize;
    assert!(
        area_bytes <= XSAVE_AREA_BYTES,
        "x86 XSAVE area for XCR0 {xcr0:#x} is {area_bytes} bytes; the exception entry reserves \
         {XSAVE_AREA_BYTES}"
    );
}
