//! End-to-end tests for the kernel OOM killer + supervisor wiring.
//!
//! These tests use the kernel's public `InstanceRegistry` /
//! `pick_oom_victim` / `request_kill` / `pending_kill` API and run on
//! the host (no QEMU, no wasmtime) so they verify victim selection
//! semantics in isolation. The kernel's lib-test compilation has
//! pre-existing breakage in unrelated WASI binding paths, so the
//! tests live in the hosted crate where `cargo test -p helios-hosted`
//! will pick them up.

#![cfg(test)]

use helios_hal::vmm::{NoSwap, SwapBackend};
use helios_kernel::{
    DEFAULT_RESTART_COST, InstanceRegistry, KillReason, PLUGIN_RESTART_COST,
    SYSTEM_COMPONENT_RESTART_COST,
};

#[test]
fn pick_skips_instances_with_zero_memory() {
    let registry = InstanceRegistry::new();
    let _idle = registry.register("idle", 0);
    assert!(registry.pick_oom_victim().is_none());
}

#[test]
fn pick_chooses_largest_consumer_when_costs_match() {
    let registry = InstanceRegistry::new();
    let small = registry.register("small", 0);
    let large = registry.register("large", 0);
    small.set_memory_bytes(64 * 1024 * 1024);
    large.set_memory_bytes(512 * 1024 * 1024);

    let victim = registry.pick_oom_victim().expect("a victim is available");
    assert_eq!(victim.id, large.id());
}

#[test]
fn pick_prefers_low_restart_cost_per_byte() {
    let registry = InstanceRegistry::new();
    let user = registry.register_with_cost("user-program", 0, DEFAULT_RESTART_COST);
    let plugin = registry.register_with_cost("compiler-plugin", 0, PLUGIN_RESTART_COST);
    // User holds half as much memory but has one-hundredth the
    // restart cost, so its `memory / restart_cost` score should win.
    user.set_memory_bytes(256 * 1024 * 1024);
    plugin.set_memory_bytes(512 * 1024 * 1024);

    let victim = registry.pick_oom_victim().expect("a victim is available");
    assert_eq!(victim.id, user.id());
}

#[test]
fn system_components_are_picked_only_as_last_resort() {
    let registry = InstanceRegistry::new();
    let system = registry.register_with_cost(
        "debugger",
        0,
        SYSTEM_COMPONENT_RESTART_COST,
    );
    system.set_memory_bytes(1024 * 1024 * 1024);

    // No other candidate: the system component is the only one with
    // memory attributed, so it has to be picked despite the high cost.
    let solo = registry.pick_oom_victim().expect("solo system victim");
    assert_eq!(solo.id, system.id());

    // Add a much smaller user-cost instance with a sliver of memory:
    // even 1 MiB of user-cost memory beats 1 GiB of system-cost.
    let user = registry.register_with_cost("user", 0, DEFAULT_RESTART_COST);
    user.set_memory_bytes(1024 * 1024);

    let victim = registry.pick_oom_victim().expect("a victim is available");
    assert_eq!(
        victim.id,
        user.id(),
        "OOM killer must prefer the cheap-to-restart user program over the system component"
    );
}

#[test]
fn request_kill_flips_flag_observable_to_pending_kill() {
    let registry = InstanceRegistry::new();
    let instance = registry.register("victim", 0);
    instance.set_memory_bytes(128 * 1024 * 1024);
    assert_eq!(instance.pending_kill(), None);

    let did_kill = registry.request_kill(instance.id(), KillReason::OutOfMemory);
    assert!(did_kill);
    assert_eq!(instance.pending_kill(), Some(KillReason::OutOfMemory));
}

#[test]
fn request_kill_is_idempotent() {
    let registry = InstanceRegistry::new();
    let instance = registry.register("victim", 0);
    instance.set_memory_bytes(64 * 1024 * 1024);

    assert!(registry.request_kill(instance.id(), KillReason::OutOfMemory));
    // Second call returns false — the kill is already in progress —
    // and the recorded reason is the original one.
    assert!(!registry.request_kill(instance.id(), KillReason::SupervisorRestart));
    assert_eq!(instance.pending_kill(), Some(KillReason::OutOfMemory));
}

#[test]
fn condemned_instances_are_excluded_from_subsequent_picks() {
    let registry = InstanceRegistry::new();
    let big = registry.register("big", 0);
    let small = registry.register("small", 0);
    big.set_memory_bytes(512 * 1024 * 1024);
    small.set_memory_bytes(64 * 1024 * 1024);

    let first = registry.pick_oom_victim().expect("first victim");
    assert_eq!(first.id, big.id());
    assert!(registry.request_kill(first.id, KillReason::OutOfMemory));

    // big is now condemned; the next pick must move on to small.
    let second = registry.pick_oom_victim().expect("second victim");
    assert_eq!(second.id, small.id());
}

#[test]
fn kill_supervisor_restart_decodes_correctly() {
    let registry = InstanceRegistry::new();
    let instance = registry.register("plugin", 0);
    instance.set_memory_bytes(1);
    assert!(registry.request_kill(instance.id(), KillReason::SupervisorRestart));
    assert_eq!(
        instance.pending_kill(),
        Some(KillReason::SupervisorRestart)
    );
}

#[test]
fn no_swap_reports_unsupported() {
    let backend = NoSwap;
    let result = backend.swap_out(b"some bytes");
    assert!(result.is_err(), "NoSwap must refuse swap_out");
    let mut buffer = [0u8; 16];
    let result = backend.swap_in((), &mut buffer);
    assert!(result.is_err(), "NoSwap must refuse swap_in");
}
