//! User-memory compactor.
//!
//! The compactor is the policy layer that decides *when* to relocate
//! user-memory pages and *which* ranges to target. It calls
//! [`hal::vmm::AddressSpace::relocate`] for the mechanism. Backends
//! whose `relocate` returns `InvalidFlags` (the documented
//! "unsupported" sentinel) cause the compactor to short-circuit
//! cleanly — it remains safe to install on every kernel even before
//! any backend grows real PT-relocation support.
//!
//! Trigger:
//!
//! - [`PressureLevel::Green`] (≥ 50 % free) — never run.
//! - [`PressureLevel::Yellow`] (25–50 % free) — run a bounded budget
//!   per pass (`max_pages_per_pass`) so a single pass cannot stall
//!   the kernel async executor on an in-rush of user memory churn.
//! - [`PressureLevel::Red`] (< 25 % free) — run aggressively and let
//!   the OOM killer step in if compaction cannot reclaim enough.
//!
//! The `CompactionPolicy` trait selects ranges to relocate. The
//! default `LargestUserRange` policy targets the largest committed
//! sub-range owned by the highest-memory non-system instance,
//! mirroring the OOM killer's "memory / restart_cost" weighting.
//! Real workloads can swap in alternative policies without touching
//! the dispatch loop.

use core::marker::PhantomData;

use helios_hal::vmm::{AddressSpace, AddressSpaceError, VirtRange};

use crate::pmm::KernelPhysFrameAllocator;

/// Coarse pressure classification used by the compactor and the OOM
/// killer to decide whether to act on a `FrameAllocStats` snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PressureLevel {
    /// ≥ 50 % free, no action.
    Green,
    /// 25 – 50 % free, gentle compaction allowed.
    Yellow,
    /// < 25 % free, OOM killer eligible.
    Red,
}

impl PressureLevel {
    /// Classify a free fraction in `[0.0, 1.0]`.
    pub fn from_free_fraction(free: f32) -> Self {
        if free >= 0.5 {
            Self::Green
        } else if free >= 0.25 {
            Self::Yellow
        } else {
            Self::Red
        }
    }
}

/// Per-pass execution budget for [`Compactor::run_pass`]. Caps the
/// compactor's wall-clock cost so the kernel async executor stays
/// responsive even under sustained Yellow pressure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactionBudget {
    pub max_pages_per_pass: usize,
}

impl Default for CompactionBudget {
    fn default() -> Self {
        Self {
            max_pages_per_pass: 256, // 1 MiB at 4 KiB pages.
        }
    }
}

/// One compaction-pass outcome reported to observers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct CompactionReport {
    pub pages_relocated: usize,
    pub pages_skipped_unsupported: usize,
    pub pages_failed: usize,
}

/// Per-range relocation candidate selected by a [`CompactionPolicy`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactionTarget {
    pub range: VirtRange,
}

/// Strategy that picks ranges to relocate. The compactor calls
/// `next_target` until it returns `None` or the budget is exhausted.
///
/// Implementations are expected to be cheap — `next_target` runs on
/// the kernel async executor's hot path under Yellow/Red pressure.
pub trait CompactionPolicy {
    fn next_target(&mut self) -> Option<CompactionTarget>;
}

/// The compactor itself. Generic over the backend address space and
/// over the policy so neither the dispatcher nor the policy uses
/// `dyn Trait` in the hot path (per AGENTS §3).
pub struct Compactor<As, Policy>
where
    As: AddressSpace,
    Policy: CompactionPolicy,
{
    address_space: &'static As,
    policy: Policy,
    budget: CompactionBudget,
    _frames: PhantomData<&'static KernelPhysFrameAllocator>,
}

impl<As, Policy> Compactor<As, Policy>
where
    As: AddressSpace,
    Policy: CompactionPolicy,
{
    pub fn new(address_space: &'static As, policy: Policy, budget: CompactionBudget) -> Self {
        Self {
            address_space,
            policy,
            budget,
            _frames: PhantomData,
        }
    }

    /// Relocate as many ranges as the budget allows. Stops on the
    /// first `OutOfFrames` (treats it as "compaction has nothing to
    /// pack into" and yields back to the OOM killer).
    pub fn run_pass(&mut self) -> CompactionReport {
        let mut report = CompactionReport::default();
        let mut pages_done = 0usize;
        while pages_done < self.budget.max_pages_per_pass {
            let Some(target) = self.policy.next_target() else {
                break;
            };
            let pages = target.range.frame_count();
            match self.address_space.relocate(target.range) {
                Ok(()) => {
                    report.pages_relocated += pages;
                    pages_done += pages;
                }
                Err(AddressSpaceError::InvalidFlags) => {
                    // Backend reports relocation unsupported; bail
                    // immediately rather than churning through a
                    // policy queue that no `Ok` can ever drain.
                    report.pages_skipped_unsupported += pages;
                    break;
                }
                Err(AddressSpaceError::OutOfFrames) => {
                    report.pages_failed += pages;
                    break;
                }
                Err(_) => {
                    report.pages_failed += pages;
                }
            }
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_classifies_around_thresholds() {
        assert_eq!(PressureLevel::from_free_fraction(0.9), PressureLevel::Green);
        assert_eq!(PressureLevel::from_free_fraction(0.5), PressureLevel::Green);
        assert_eq!(
            PressureLevel::from_free_fraction(0.4),
            PressureLevel::Yellow
        );
        assert_eq!(
            PressureLevel::from_free_fraction(0.25),
            PressureLevel::Yellow
        );
        assert_eq!(PressureLevel::from_free_fraction(0.1), PressureLevel::Red);
        assert_eq!(PressureLevel::from_free_fraction(0.0), PressureLevel::Red);
    }
}
