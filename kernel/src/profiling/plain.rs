//! The export a kernel that carries no instrumentation answers with.
//!
//! A plain kernel has no `__llvm_prf_*` sections and no profile runtime, so
//! the call it can still receive is answered by naming that, rather than by
//! handing back a profile with no counters in it — a merge would take an
//! empty profile for a workload that never ran anything.

use super::{LlvmProfile, LlvmProfileError};

/// Reports that this kernel image carries no LLVM instrumentation.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlainKernelProfile;

impl LlvmProfile for PlainKernelProfile {
    fn size(&self) -> Result<u64, LlvmProfileError> {
        Err(LlvmProfileError::NotInstrumented)
    }

    fn read(&self, _offset: u64, _out: &mut [u8]) -> Result<usize, LlvmProfileError> {
        Err(LlvmProfileError::NotInstrumented)
    }
}
