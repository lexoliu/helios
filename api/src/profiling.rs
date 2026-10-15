//! Profiling syscalls.

use crate::bindings::helios::system::profiling as raw;
pub use crate::bindings::helios::system::profiling::{
    Filter, FoldedSample, InstructionProfile, InstructionProfileError, InstructionSample,
    MetricFilter, MetricSample, MonoNanos, ProfileSection, RawProfileError, SampledCode, Scope,
    WasmFunction,
};

/// Turns kernel-side profile sampling on or off for this instance.
pub fn set_enabled(enabled: bool) {
    raw::set_enabled(enabled);
}

/// Discards all accumulated profile and metric samples.
pub fn clear() {
    raw::clear();
}

/// Returns up to `limit` folded stack samples matching `filter`,
/// suitable for flamegraph rendering.
pub fn folded(filter: &Filter, limit: u32) -> Vec<FoldedSample> {
    raw::folded(filter, limit)
}

/// Returns up to `limit` performance-metric samples matching `filter`.
pub fn metrics(filter: &MetricFilter, limit: u32) -> Vec<MetricSample> {
    raw::metrics(filter, limit)
}

/// Starts or stops the kernel's instruction-address sampling, independently
/// of [`set_enabled`]. Sampling shortens every processor's timer period, so
/// it perturbs timings; starting discards an earlier run's samples.
///
/// A backend that does not sample instruction addresses says so.
pub fn set_instruction_sampling(enabled: bool) -> Result<(), InstructionProfileError> {
    raw::set_instruction_sampling(enabled)
}

/// Returns the instruction addresses the kernel sampled since sampling last
/// started, attributed to the code they executed.
///
/// A backend that does not sample instruction addresses says so.
pub fn instruction_samples() -> Result<InstructionProfile, InstructionProfileError> {
    raw::instruction_samples()
}

/// Returns the length in bytes of the kernel's LLVM raw profile.
///
/// A kernel that was not built with the `profile-generate` profile carries
/// no instrumentation and says so.
pub fn raw_profile_size() -> Result<u64, RawProfileError> {
    raw::raw_profile_size()
}

/// Returns the window of the kernel's LLVM raw profile that starts at
/// `offset`, at most `length` bytes of it.
pub fn raw_profile_read(offset: u64, length: u32) -> Result<Vec<u8>, RawProfileError> {
    raw::raw_profile_read(offset, length)
}
