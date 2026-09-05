//! The profile runtime an instrumented kernel links.
//!
//! Everything an object built with `-C profile-generate -Z no-profiler-runtime`
//! still refers to lives here: `__llvm_profile_runtime`, the symbol rustc
//! would otherwise satisfy by injecting compiler-rt, and the section bounds
//! the linker script defines around the instrumentation
//! (`aarch64/profile-generate.ld`, `riscv/profile-generate.x`; the
//! `x86_64-unknown-none` link uses LLD's own layout and its synthesised
//! `__start_`/`__stop_` symbols).
//!
//! Value profiling is off in the instrumented build
//! (`-C llvm-args=-disable-vp=true`), so no object refers to
//! `__llvm_profile_instrument_target` or `__llvm_profile_instrument_memop`
//! and no record carries value sites. `docs/pgo.md` records why: those hooks
//! run on every indirect call and allocate profile nodes as they go, and the
//! value data they produce would make the profile's length depend on what has
//! executed, which is what lets this export hand out a window at a time.

use super::raw::{RawProfile, SectionSpan};
use super::{LlvmProfile, LlvmProfileError};

unsafe extern "C" {
    /// Counter section, written by the instrumentation itself.
    static __start___llvm_prf_cnts: u8;
    static __stop___llvm_prf_cnts: u8;
    /// One `__llvm_profile_data` record per instrumented function.
    static __start___llvm_prf_data: u8;
    static __stop___llvm_prf_data: u8;
    /// Compressed function names.
    static __start___llvm_prf_names: u8;
    static __stop___llvm_prf_names: u8;
    /// The version word LLVM emitted into every instrumented module.
    static __llvm_profile_raw_version: u64;
}

/// The symbol every instrumented object references so that the profile
/// runtime is linked in.
///
/// compiler-rt defines it as `int __llvm_profile_runtime`; the value is never
/// read, only the definition matters, and defining it here is what
/// `-Z no-profiler-runtime` leaves to the program being instrumented.
#[unsafe(no_mangle)]
#[expect(
    non_upper_case_globals,
    reason = "the linker looks this symbol up by the name compiler-rt gives it"
)]
pub static __llvm_profile_runtime: i32 = 0;

/// Exports the raw profile of a kernel that carries LLVM instrumentation.
#[derive(Debug, Clone, Copy, Default)]
pub struct InstrumentedKernelProfile;

impl InstrumentedKernelProfile {
    fn image(&self) -> Result<RawProfile<'static>, LlvmProfileError> {
        // SAFETY: each pair bounds one linked section, so the bytes between
        // them are readable for the life of the kernel image. The counter
        // section is written by instrumented code while it is read, which is
        // why the spans never become shared references.
        let (counters, data, names) = unsafe {
            (
                span(
                    &raw const __start___llvm_prf_cnts,
                    &raw const __stop___llvm_prf_cnts,
                ),
                span(
                    &raw const __start___llvm_prf_data,
                    &raw const __stop___llvm_prf_data,
                ),
                span(
                    &raw const __start___llvm_prf_names,
                    &raw const __stop___llvm_prf_names,
                ),
            )
        };
        // SAFETY: the version word is a link-time constant in the image.
        let version = unsafe { __llvm_profile_raw_version };
        RawProfile::new(data, counters, names, version)
    }
}

impl LlvmProfile for InstrumentedKernelProfile {
    fn size(&self) -> Result<u64, LlvmProfileError> {
        Ok(self.image()?.len())
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> Result<usize, LlvmProfileError> {
        self.image()?.read_at(offset, out)
    }
}

/// Builds the span a `__start_`/`__stop_` symbol pair bounds.
///
/// # Safety
///
/// `start` and `stop` must be the bounds of one linked section, with `stop`
/// at or after `start`.
unsafe fn span(start: *const u8, stop: *const u8) -> SectionSpan<'static> {
    let len = stop.addr().checked_sub(start.addr()).unwrap_or_else(|| {
        panic!(
            "instrumentation section ends at {:#x} before it starts at {:#x}",
            stop.addr(),
            start.addr()
        )
    });
    // SAFETY: the caller guarantees the pair bounds a section of the kernel
    // image, which stays mapped for the life of the kernel.
    unsafe { SectionSpan::new(start, len as u64) }
}
