//! The boot report of which return addresses the machine authenticates.
//!
//! # Concurrency contract
//!
//! The report is read-only and runs once, on the bootstrap processor, after
//! every other processor has been started. A backend enables its mechanisms
//! on a processor before [`Cpu::start_processor`] returns for it, so the line
//! is emitted after every processor's enable. The inputs are two immutable
//! values: the backend's [`ControlFlowIntegrity`], fixed before the `Cpu`
//! reached the kernel, and the code protection the engine was configured to
//! require. Nothing here takes a lock or reaches another processor.

use helios_artifact::{CwasmCodeProtection, cwasm_target_code_protection};
use helios_hal::cpu::{ControlFlowIntegrity, Cpu, ReturnAddressSigning};

/// What the machine authenticates, from the two sources that decide it: what
/// the backend enabled, and what the runtime code the engine loads carries.
///
/// The runtime-code half is the value the engine was configured to require,
/// so the claim and the enforcement share one source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReturnAddressCoverage {
    backend: ControlFlowIntegrity,
    runtime_code: CwasmCodeProtection,
}

/// The four combinations of the two sources, each reported in its own words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Report {
    /// The backend enabled signing on every processor.
    Enabled {
        signing: ReturnAddressSigning,
        runtime_code_signed: bool,
    },
    /// The backend enabled nothing, and the runtime code carries signing
    /// instructions that therefore do nothing.
    NotEnabledRuntimeCodeSigned,
    /// The backend enabled nothing, and the runtime code carries none.
    NotEnabled,
}

impl ReturnAddressCoverage {
    pub(crate) const fn new(
        backend: ControlFlowIntegrity,
        runtime_code: CwasmCodeProtection,
    ) -> Self {
        Self {
            backend,
            runtime_code,
        }
    }

    /// The coverage of `cpu` running the artifacts compiled for `target`.
    pub(crate) fn of<CpuImpl: Cpu>(cpu: &CpuImpl, target: &str) -> Self {
        Self::new(
            cpu.control_flow_integrity(),
            cwasm_target_code_protection(target),
        )
    }

    fn report(self) -> Report {
        match self.backend.return_addresses {
            Some(signing) => Report::Enabled {
                signing,
                runtime_code_signed: self.runtime_code.return_addresses_signed,
            },
            None if self.runtime_code.return_addresses_signed => {
                Report::NotEnabledRuntimeCodeSigned
            }
            None => Report::NotEnabled,
        }
    }

    /// Emits the boot line for a machine of `processors` processors.
    pub(crate) fn log(self, processors: usize) {
        match self.report() {
            Report::Enabled {
                signing,
                runtime_code_signed,
            } => tracing::info!(
                processors,
                key = "IA, one per boot",
                fpac = signing.faults_at_authentication,
                kernel_text = "non-leaf functions sign their return addresses",
                runtime_code = if runtime_code_signed {
                    "every loaded artifact signs its return addresses (zero modifier)"
                } else {
                    "not signed"
                },
                not_covered = "exception frames, swap-fault resume frames, suspended fiber switch frames, hand-written assembly, bring-up before the enable, forward edges",
                "return-address signing enabled on every processor"
            ),
            Report::NotEnabledRuntimeCodeSigned => tracing::info!(
                runtime_code =
                    "carries return-address signing instructions this backend does not enable",
                "return-address signing not enabled by this backend"
            ),
            Report::NotEnabled => {
                tracing::info!("return-address signing not enabled by this backend")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGNED: CwasmCodeProtection = CwasmCodeProtection {
        return_addresses_signed: true,
    };
    const SIGNING: ReturnAddressSigning = ReturnAddressSigning {
        faults_at_authentication: true,
    };
    const ENABLED: ControlFlowIntegrity = ControlFlowIntegrity {
        return_addresses: Some(SIGNING),
    };

    #[test]
    fn backend_signing_with_signed_runtime_code_is_reported_enabled() {
        assert_eq!(
            ReturnAddressCoverage::new(ENABLED, SIGNED).report(),
            Report::Enabled {
                signing: SIGNING,
                runtime_code_signed: true,
            }
        );
    }

    #[test]
    fn backend_signing_with_unsigned_runtime_code_says_so() {
        assert_eq!(
            ReturnAddressCoverage::new(ENABLED, CwasmCodeProtection::NONE).report(),
            Report::Enabled {
                signing: SIGNING,
                runtime_code_signed: false,
            }
        );
    }

    #[test]
    fn signed_runtime_code_without_the_backend_is_reported_not_enabled() {
        assert_eq!(
            ReturnAddressCoverage::new(ControlFlowIntegrity::NONE, SIGNED).report(),
            Report::NotEnabledRuntimeCodeSigned
        );
    }

    #[test]
    fn nothing_is_reported_not_enabled() {
        assert_eq!(
            ReturnAddressCoverage::new(ControlFlowIntegrity::NONE, CwasmCodeProtection::NONE)
                .report(),
            Report::NotEnabled
        );
    }

    /// The test processor enables nothing, so it takes the default, and the
    /// coverage of an aarch64 target with it is the second state.
    #[test]
    fn a_backend_that_overrides_nothing_enables_nothing() {
        let cpu = crate::test_support::TestCpu::without_entropy();
        assert_eq!(cpu.control_flow_integrity(), ControlFlowIntegrity::NONE);
        assert_eq!(
            ReturnAddressCoverage::of(&cpu, "aarch64-unknown-none").report(),
            Report::NotEnabledRuntimeCodeSigned
        );
        assert_eq!(
            ReturnAddressCoverage::of(&cpu, "x86_64-unknown-none").report(),
            Report::NotEnabled
        );
    }
}
