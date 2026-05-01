//! Compactor unit tests.
//!
//! The compactor sits above the backend address space, so we test it
//! against a fake `AddressSpace` that records `relocate` calls and
//! returns scripted outcomes. This pins the dispatcher's policy
//! handling — budget enforcement, unsupported short-circuit,
//! `OutOfFrames` early-stop — without depending on a real bare-metal
//! VM.

#![cfg(test)]

use std::sync::Mutex;

use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{
    AddressSpace, AddressSpaceError, PageFlags, Translation, VirtAddr, VirtRange,
};
use helios_kernel::{
    CompactionBudget, CompactionPolicy, CompactionTarget, Compactor, PressureLevel,
};

/// Fake AS that records `relocate` calls and returns scripted
/// outcomes. The inner `Mutex` is only needed for the test harness;
/// the real backend impls are lock-free per-page.
struct FakeAddressSpace {
    relocate_log: Mutex<Vec<VirtRange>>,
    relocate_outcomes: Mutex<Vec<Result<(), AddressSpaceError>>>,
}

impl FakeAddressSpace {
    fn with_outcomes(outcomes: Vec<Result<(), AddressSpaceError>>) -> Self {
        Self {
            relocate_log: Mutex::new(Vec::new()),
            relocate_outcomes: Mutex::new(outcomes),
        }
    }

    fn relocate_log(&self) -> Vec<VirtRange> {
        self.relocate_log.lock().unwrap().clone()
    }
}

impl AddressSpace for FakeAddressSpace {
    fn reserve(&self, _byte_len: usize) -> Result<VirtRange, AddressSpaceError> {
        unimplemented!("compactor never reserves")
    }
    fn release(&self, _virt: VirtRange) -> Result<(), AddressSpaceError> {
        unimplemented!("compactor never releases")
    }
    fn commit(&self, _virt: VirtRange, _flags: PageFlags) -> Result<(), AddressSpaceError> {
        unimplemented!("compactor never commits")
    }
    fn decommit(&self, _virt: VirtRange) -> Result<(), AddressSpaceError> {
        unimplemented!("compactor never decommits")
    }
    fn protect(&self, _virt: VirtRange, _flags: PageFlags) -> Result<(), AddressSpaceError> {
        unimplemented!("compactor never protects")
    }
    fn translate(&self, _addr: VirtAddr) -> Translation {
        Translation::Unmapped
    }
    fn relocate(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        self.relocate_log.lock().unwrap().push(virt);
        let mut outcomes = self.relocate_outcomes.lock().unwrap();
        if outcomes.is_empty() {
            Ok(())
        } else {
            outcomes.remove(0)
        }
    }
}

struct ScriptedPolicy {
    targets: Mutex<Vec<CompactionTarget>>,
}

impl ScriptedPolicy {
    fn new(targets: Vec<CompactionTarget>) -> Self {
        Self {
            targets: Mutex::new(targets),
        }
    }
}

impl CompactionPolicy for ScriptedPolicy {
    fn next_target(&mut self) -> Option<CompactionTarget> {
        let mut targets = self.targets.lock().unwrap();
        if targets.is_empty() {
            None
        } else {
            Some(targets.remove(0))
        }
    }
}

fn page_aligned(pages: usize) -> usize {
    pages * PhysFrame::SIZE
}

fn target(start: usize, pages: usize) -> CompactionTarget {
    CompactionTarget {
        range: VirtRange::new(VirtAddr::new(start), page_aligned(pages)),
    }
}

#[test]
fn green_pressure_does_not_run_compactor() {
    // The compactor is invoked by an outer policy that gates on
    // pressure; this test pins the gate's truth value, not the
    // compactor itself.
    assert_eq!(PressureLevel::from_free_fraction(0.9), PressureLevel::Green);
}

#[test]
fn pass_runs_until_policy_drains() {
    let address_space: &'static _ = Box::leak(Box::new(FakeAddressSpace::with_outcomes(vec![])));
    let policy = ScriptedPolicy::new(vec![
        target(0x1_0000, 1),
        target(0x2_0000, 2),
        target(0x3_0000, 4),
    ]);
    let mut compactor = Compactor::new(
        address_space,
        policy,
        CompactionBudget {
            max_pages_per_pass: 1024,
        },
    );

    let report = compactor.run_pass();
    assert_eq!(report.pages_relocated, 1 + 2 + 4);
    assert_eq!(report.pages_failed, 0);
    assert_eq!(report.pages_skipped_unsupported, 0);
    assert_eq!(address_space.relocate_log().len(), 3);
}

#[test]
fn pass_respects_page_budget() {
    let address_space: &'static _ = Box::leak(Box::new(FakeAddressSpace::with_outcomes(vec![])));
    let policy = ScriptedPolicy::new(vec![
        target(0x1_0000, 4),
        target(0x2_0000, 4),
        target(0x3_0000, 4),
    ]);
    let mut compactor = Compactor::new(
        address_space,
        policy,
        CompactionBudget {
            max_pages_per_pass: 6,
        },
    );

    let report = compactor.run_pass();
    // First 4-page target fits, second target pushes past 6 but is
    // accepted whole (budget is a soft cap on entry, not a hard
    // mid-target chop) — the compactor stops *after* the second
    // target because pages_done >= budget.
    assert_eq!(report.pages_relocated, 8);
    assert_eq!(address_space.relocate_log().len(), 2);
}

#[test]
fn unsupported_short_circuits_pass() {
    let address_space: &'static _ =
        Box::leak(Box::new(FakeAddressSpace::with_outcomes(vec![Err(
            AddressSpaceError::InvalidFlags,
        )])));
    let policy = ScriptedPolicy::new(vec![target(0x1_0000, 1), target(0x2_0000, 1)]);
    let mut compactor = Compactor::new(
        address_space,
        policy,
        CompactionBudget {
            max_pages_per_pass: 1024,
        },
    );

    let report = compactor.run_pass();
    assert_eq!(report.pages_relocated, 0);
    assert_eq!(report.pages_skipped_unsupported, 1);
    // Compactor must abandon the rest of the queue once a backend
    // reports unsupported — looping forever costs CPU with no
    // chance of progress.
    assert_eq!(address_space.relocate_log().len(), 1);
}

#[test]
fn out_of_frames_stops_pass_and_reports_failure() {
    let address_space: &'static _ = Box::leak(Box::new(FakeAddressSpace::with_outcomes(vec![
        Ok(()),
        Err(AddressSpaceError::OutOfFrames),
    ])));
    let policy = ScriptedPolicy::new(vec![
        target(0x1_0000, 2),
        target(0x2_0000, 2),
        target(0x3_0000, 2),
    ]);
    let mut compactor = Compactor::new(
        address_space,
        policy,
        CompactionBudget {
            max_pages_per_pass: 1024,
        },
    );

    let report = compactor.run_pass();
    assert_eq!(report.pages_relocated, 2);
    assert_eq!(report.pages_failed, 2);
    // Failed second target stops the pass — third never reached.
    assert_eq!(address_space.relocate_log().len(), 2);
}

#[test]
fn other_errors_continue_pass_but_count_failures() {
    let address_space: &'static _ = Box::leak(Box::new(FakeAddressSpace::with_outcomes(vec![
        Err(AddressSpaceError::Misaligned),
        Ok(()),
    ])));
    let policy = ScriptedPolicy::new(vec![target(0x1_0000, 1), target(0x2_0000, 1)]);
    let mut compactor = Compactor::new(
        address_space,
        policy,
        CompactionBudget {
            max_pages_per_pass: 1024,
        },
    );

    let report = compactor.run_pass();
    assert_eq!(report.pages_relocated, 1);
    assert_eq!(report.pages_failed, 1);
    assert_eq!(address_space.relocate_log().len(), 2);
}
