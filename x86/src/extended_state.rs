//! x86 extended-state bring-up and the state-preservation path.
//!
//! The kernel uses a soft-float ABI with SSE/SSE2 enabled: the
//! `x86_64-unknown-none` target spec says `rustc-abi: softfloat` and
//! `+soft-float`. `.cargo/config.toml`'s `-soft-float` is rejected by rustc
//! ("cannot be disabled"), so `target_feature="soft-float"` stays enabled.
//! LLVM uses XMM only for moves and memcpy; no AVX target feature is enabled.
//! The inspector image audit enforces that the kernel contains no VEX/EVEX
//! instructions. Runtime CPU dispatch in dependencies is why the audit exists
//! and must stay on the release image: `encoding_rs`'s UTF-8 validation selects
//! `simdutf8`'s AVX2 path through `core_detect`, which runs CPUID and XGETBV
//! itself on `no_std`, so once OSXSAVE is on, any kernel path that reached it
//! would run VEX code.
//!
//! `exceptions.S` saves x87/SSE with FXSAVE64 and returns via IRETQ.
//! Legacy SSE/x87 and FXRSTOR leave upper YMM/ZMM and opmask state untouched,
//! and because the kernel contains no VEX/EVEX code, nothing in the kernel
//! writes those components, so they survive interrupt entry/exit without
//! XSAVE.
//! This is deliberate: #410 measured eager AVX-512 XSAVE on every interrupt
//! as +15–38% on interrupt/IPI-heavy rows. Timer/epoch, wake, TLB-shootdown,
//! and device handlers do not switch context. Wasmtime trap delivery resumes
//! at a call boundary where SysV vector registers are dead, and the FXSAVE
//! area remains on the IST stack.
//!
//! Fiber switches are cooperative calls; SysV makes vector registers and
//! k-masks caller-saved, so only callee-saved GPRs switch. MXCSR and the x87
//! control word are callee-saved but unchanged across a switch. Preemption is
//! cooperative through a Cranelift epoch check and call boundary; x86 has no
//! asynchronous preemption. The aarch64 swap trampoline does not exist here:
//! x86 disables swap with `SwapDisabled::NoSwapHooks`. If swap is added (#25),
//! arbitrary interruption requires full XSAVE of the enabled XCR0 state.
//!
//! XCR0 is every user state component CPUID.(0DH,0):EDX:EAX reports, not
//! only the vector components guest code uses. Under KVM, which is the
//! production target, entering the guest loads the guest's XCR0 and every
//! exit restores the host's, unless the two are equal. KVM reports only
//! components the host itself enabled, so the reported set is a subset of
//! the host's mask and equals it when the VMM exposes every component
//! (QEMU's `-cpu host` does); a guest that matches it pays no XSETBV on each
//! exit. The components beyond x87/SSE/AVX/AVX-512 (PKRU, AMX tiles) are
//! written by neither the kernel nor Cranelift-compiled guest code, and
//! CR4.PKE stays clear, so they stay in their initial configuration.

use core::arch::x86_64::{__cpuid, __cpuid_count};
use x86_64::registers::control::{Cr4, Cr4Flags};

const X87: u64 = 1 << 0;
const SSE: u64 = 1 << 1;
const AVX: u64 = 1 << 2;
const OPMASK: u64 = 1 << 5;
const ZMM_HI256: u64 = 1 << 6;
const HI16_ZMM: u64 = 1 << 7;
pub(super) const AVX512_STATE: u64 = OPMASK | ZMM_HI256 | HI16_ZMM;

/// The XCR0 mask the bootstrap processor computed and programmed. It reaches
/// each secondary through `smp::BootContext`, and every secondary must
/// program exactly this mask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Xcr0(u64);

impl Xcr0 {
    pub(crate) fn bits(self) -> u64 {
        self.0
    }
}

/// Validates CPUID, enables OSXSAVE and programs XCR0 on the bootstrap
/// processor, returning the mask the secondaries must match.
pub(crate) fn enable_on_bootstrap_processor() -> Xcr0 {
    let xcr0 = supported_xcr0();
    program(xcr0);
    Xcr0(xcr0)
}

/// Enables OSXSAVE and programs the bootstrap processor's XCR0 on a
/// secondary, after checking this processor reports the same components.
pub(crate) fn enable_on_secondary_processor(bootstrap: Xcr0) {
    let xcr0 = supported_xcr0();
    let initial_apic_id = __cpuid(1).ebx >> 24;
    assert_eq!(
        xcr0,
        bootstrap.0,
        "x86 processor with initial APIC ID {initial_apic_id} XCR0 {xcr0:#x} differs from \
         bootstrap XCR0 {bootstrap:#x}; Helios requires every processor to run the same XSAVE \
         state components",
        bootstrap = bootstrap.0
    );
    program(xcr0);
}

/// Every user state component this processor's CPUID reports, after the
/// checks the kernel depends on.
fn supported_xcr0() -> u64 {
    let max_basic = __cpuid(0).eax;
    assert!(
        max_basic >= 0xD,
        "x86 processor lacks CPUID basic leaf 0xD; Helios runs AVX-compiled guest code and \
         manages vector state through XSAVE/XCR0"
    );
    let leaf1 = __cpuid(1);
    assert!(
        leaf1.ecx & (1 << 26) != 0,
        "x86 processor lacks XSAVE (CPUID.01H:ECX[26] clear); Helios runs AVX-compiled guest \
         code and manages vector state through XSAVE/XCR0"
    );
    let leaf0 = __cpuid_count(0xD, 0);
    let supported = u64::from(leaf0.eax) | (u64::from(leaf0.edx) << 32);
    assert!(
        supported & (X87 | SSE) == (X87 | SSE),
        "x86 processor CPUID.(0DH,0):EDX:EAX {supported:#x} lacks required XCR0 mask \
         {required:#x} (x87|SSE)",
        required = X87 | SSE
    );
    let mut xcr0 = supported;
    if xcr0 & AVX == 0 || leaf1.ecx & (1 << 28) == 0 {
        xcr0 &= !(AVX | AVX512_STATE);
    }
    if xcr0 & AVX512_STATE != 0 && xcr0 & AVX512_STATE != AVX512_STATE {
        panic!(
            "x86 processor reported incomplete AVX-512 XCR0 state {reported:#x}; \
             XSETBV requires the complete AVX512_STATE mask {required:#x}",
            reported = xcr0 & AVX512_STATE,
            required = AVX512_STATE
        );
    }
    xcr0
}

/// Sets CR4.OSXSAVE and XCR0 on the executing processor and checks the
/// write took.
fn program(xcr0: u64) {
    unsafe {
        Cr4::update(|flags| flags.insert(Cr4Flags::OSXSAVE));
    }
    let low = xcr0 as u32;
    let high = (xcr0 >> 32) as u32;
    unsafe {
        core::arch::asm!(
            "xsetbv",
            in("ecx") 0_u32,
            in("eax") low,
            in("edx") high,
            options(nostack, preserves_flags),
        );
    }
    let read_back = unsafe {
        let eax: u32;
        let edx: u32;
        core::arch::asm!(
            "xgetbv",
            in("ecx") 0_u32,
            lateout("eax") eax,
            lateout("edx") edx,
            options(nomem, nostack, preserves_flags),
        );
        u64::from(eax) | (u64::from(edx) << 32)
    };
    assert_eq!(
        xcr0, read_back,
        "x86 XSETBV/XGETBV mismatch: wrote {xcr0:#x}, read back {read_back:#x}"
    );
}

pub(crate) fn xsave_sizes() -> (u32, u32) {
    let leaf = __cpuid_count(0xD, 0);
    (leaf.ebx, leaf.ecx)
}
