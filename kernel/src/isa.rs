//! The instruction-set level the in-kernel compiler emits guest code for.

use core::fmt;

use helios_artifact::{
    CwasmTarget, IsaLevel, IsaLevelError, NativeFeatureProbe, cwasm_target_isa_levels,
};

/// The target the kernel image was built for and the highest of its ISA
/// levels the processor satisfies.
///
/// The level is selected once, from the backend's feature probe, when the
/// program service is installed on the bootstrap processor, before any
/// secondary processor runs guest code. The backends check that every
/// processor answers the probe alike, so the one level holds machine-wide.
///
/// Concurrency contract: the value is immutable and `Copy`. It is stored in
/// the program service and read lock-free from any processor; nothing writes
/// it after selection.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GuestIsa {
    triple: &'static str,
    processor: &'static IsaLevel,
}

impl GuestIsa {
    /// Selects the highest level of `triple`'s chain whose features `probe`
    /// reports the processor has.
    pub(crate) fn select(
        triple: &'static str,
        probe: Option<NativeFeatureProbe>,
    ) -> Result<Self, IsaLevelError> {
        let processor = cwasm_target_isa_levels(triple).select(probe)?;
        Ok(Self { triple, processor })
    }

    /// The selection a kernel image made for its own build target.
    ///
    /// # Panics
    ///
    /// Panics naming the error when the probe does not answer a feature of
    /// the target's levels or the processor does not satisfy the floor level:
    /// the kernel cannot compile for a processor it cannot describe.
    pub(crate) fn for_build_target(probe: Option<NativeFeatureProbe>) -> Self {
        let triple = env!("HELIOS_BUILD_TARGET");
        Self::select(triple, probe).unwrap_or_else(|error| {
            panic!("cannot select the guest ISA level for {triple}: {error}")
        })
    }

    /// The compile target: the build target at the processor's level.
    pub(crate) fn compile_target(&self) -> CwasmTarget<'static> {
        CwasmTarget::new(self.triple, self.processor.name())
            .unwrap_or_else(|error| panic!("the selected level is not the target's own: {error}"))
    }

    /// Writes the boot line naming the selection.
    pub(crate) fn log(&self) {
        tracing::info!(
            triple = self.triple,
            processor = self.processor.name(),
            flags = %CraneliftFlags(self.processor),
            "guest code ISA level selected"
        );
    }
}

/// A level's Cranelift flags, comma separated, floor's first.
struct CraneliftFlags(&'static IsaLevel);

impl fmt::Display for CraneliftFlags {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, feature) in self.0.features().enumerate() {
            if index > 0 {
                formatter.write_str(",")?;
            }
            formatter.write_str(feature.cranelift_flag())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::floor_only_probe;

    #[test]
    fn a_floor_only_probe_selects_the_floor() {
        let guest = GuestIsa::for_build_target(Some(floor_only_probe));
        let levels = cwasm_target_isa_levels(env!("HELIOS_BUILD_TARGET"));
        assert_eq!(guest.processor().name(), levels.floor().name());
        assert_eq!(
            guest.compile_target().level().name(),
            guest.processor().name()
        );
    }

    #[test]
    fn a_probe_that_does_not_answer_fails_the_selection() {
        let triple = env!("HELIOS_BUILD_TARGET");
        let featureless = cwasm_target_isa_levels(triple)
            .iter()
            .all(|level| level.features().next().is_none());
        // A featureless target asks the probe nothing, so none can fail it.
        assert_eq!(
            GuestIsa::select(triple, Some(|_| None)).is_err(),
            !featureless
        );
    }

    #[test]
    fn the_flags_line_lists_every_flag_of_the_level() {
        let guest = GuestIsa::for_build_target(Some(floor_only_probe));
        let flags = std::format!("{}", CraneliftFlags(guest.processor()));
        assert_eq!(
            flags.split(',').filter(|flag| !flag.is_empty()).count(),
            guest.processor().features().count()
        );
    }
}
