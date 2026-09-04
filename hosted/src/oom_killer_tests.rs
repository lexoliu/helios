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
use helios_kernel::{InstanceRegistry, KillReason, OomPolicy};

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
    let user = registry.register_with_policy("user-program", 0, OomPolicy::UserProgram);
    let plugin = registry.register_with_policy("compiler-plugin", 0, OomPolicy::KernelPlugin);
    // User holds half as much memory but has one-hundredth the
    // restart cost, so its `memory / restart cost` score should win.
    user.set_memory_bytes(256 * 1024 * 1024);
    plugin.set_memory_bytes(512 * 1024 * 1024);

    let victim = registry.pick_oom_victim().expect("a victim is available");
    assert_eq!(victim.id, user.id());
}

/// Issue #94's second signature: a spawn storm walked the user pool
/// empty, every user instance was already condemned, and the OOM killer
/// picked the embedded debugger — which is fatal. A system component is
/// not a candidate at any score.
#[test]
fn system_components_are_never_oom_victims() {
    let registry = InstanceRegistry::new();
    let system = registry.register_with_policy("debugger", 0, OomPolicy::SystemComponent);
    system.set_memory_bytes(1024 * 1024 * 1024);

    assert!(
        registry.pick_oom_victim().is_none(),
        "a system component holding every byte is still not a victim"
    );

    // A user program with a sliver of memory is, and stays, the pick.
    let user = registry.register_with_policy("user", 0, OomPolicy::UserProgram);
    user.set_memory_bytes(1024 * 1024);

    let victim = registry.pick_oom_victim().expect("a victim is available");
    assert_eq!(victim.id, user.id());

    // Once that user program is condemned there is no one left to
    // condemn: the requester takes the grow failure instead.
    assert!(registry.request_kill(victim.id, KillReason::OutOfMemory));
    assert!(
        registry.pick_oom_victim().is_none(),
        "with every user instance condemned the killer must run out of victims, \
         not fall through to the kernel's own components"
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
    assert_eq!(instance.pending_kill(), Some(KillReason::SupervisorRestart));
}

#[test]
fn no_swap_reports_unsupported() {
    let backend = NoSwap;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("tokio runtime");
    let result = runtime.block_on(backend.swap_out(b"some bytes"));
    assert!(result.is_err(), "NoSwap must refuse swap_out");
    let mut buffer = [0u8; 16];
    let result = runtime.block_on(backend.swap_in((), &mut buffer));
    assert!(result.is_err(), "NoSwap must refuse swap_in");
}
