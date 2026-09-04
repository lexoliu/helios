//! End-to-end tests for the kernel OOM killer + supervisor wiring.
//!
//! These tests use the kernel's public `InstanceRegistry` /
//! `pick_oom_victim` / `condemn_for_oom` / `request_kill` /
//! `pending_kill` API and run on the host (no QEMU, no wasmtime) so they
//! verify victim selection and condemned-memory accounting in
//! isolation. The kernel's lib-test compilation has
//! pre-existing breakage in unrelated WASI binding paths, so the
//! tests live in the hosted crate where `cargo test -p helios-hosted`
//! will pick them up.

#![cfg(test)]

use helios_hal::vmm::{NoSwap, SwapBackend};
use helios_kernel::{InstanceRegistry, KillReason, OOM_RECLAIM_GRACE, OomKillOutcome, OomPolicy};

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
    assert!(registry.request_kill(victim.id, KillReason::OutOfMemory, 0));
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

    let did_kill = registry.request_kill(instance.id(), KillReason::OutOfMemory, 0);
    assert!(did_kill);
    assert_eq!(instance.pending_kill(), Some(KillReason::OutOfMemory));
}

#[test]
fn request_kill_is_idempotent() {
    let registry = InstanceRegistry::new();
    let instance = registry.register("victim", 0);
    instance.set_memory_bytes(64 * 1024 * 1024);

    assert!(registry.request_kill(instance.id(), KillReason::OutOfMemory, 0));
    // Second call returns false — the kill is already in progress —
    // and the recorded reason is the original one.
    assert!(!registry.request_kill(instance.id(), KillReason::SupervisorRestart, 10));
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
    assert!(registry.request_kill(first.id, KillReason::OutOfMemory, 0));

    // big is now condemned; the next pick must move on to small.
    let second = registry.pick_oom_victim().expect("second victim");
    assert_eq!(second.id, small.id());
}

#[test]
fn kill_supervisor_restart_decodes_correctly() {
    let registry = InstanceRegistry::new();
    let instance = registry.register("plugin", 0);
    instance.set_memory_bytes(1);
    assert!(registry.request_kill(instance.id(), KillReason::SupervisorRestart, 0));
    assert_eq!(instance.pending_kill(), Some(KillReason::SupervisorRestart));
}

/// Issue #100: condemning a victim does not free its memory — the kill
/// flag is observed at that instance's next call-hook boundary. The
/// killer used to pick a fresh live instance on every refused grow, so
/// one 1MiB shortfall walked a whole workload: ~20 instances condemned
/// at ~45KB reclaimed each. The condemned-memory ledger is what stops
/// it: once the bytes already condemned cover the request, the
/// requester takes the grow failure and retries.
#[test]
fn one_shortfall_condemns_only_the_victims_that_cover_it() {
    const VICTIM_BYTES: u64 = 64 * 1024;
    const REQUESTED_BYTES: u64 = 1024 * 1024;
    const INSTANCES: usize = 32;

    let registry = InstanceRegistry::new();
    // The requester is the instance whose grow was refused; it holds no
    // memory of its own yet.
    let requester = registry.register("requester", 0);
    let victims: Vec<_> = (0..INSTANCES)
        .map(|_| {
            let instance = registry.register("/bin/procbench", 0);
            instance.set_memory_bytes(VICTIM_BYTES);
            instance
        })
        .collect();

    // Every instance retries its refused grow; none of them is torn down
    // in between, which is exactly the state the storm ran in.
    let mut condemned = 0_usize;
    let mut awaiting = 0_usize;
    for _ in 0..INSTANCES {
        match registry
            .condemn_for_oom(requester.id(), REQUESTED_BYTES, 0)
            .outcome
        {
            OomKillOutcome::Condemned(_) => condemned += 1,
            OomKillOutcome::AwaitingReclaim => awaiting += 1,
            OomKillOutcome::NoVictim => panic!("live instances remained to condemn"),
        }
    }

    let needed = REQUESTED_BYTES.div_ceil(VICTIM_BYTES) as usize;
    assert_eq!(
        condemned, needed,
        "one refused grow must condemn only the victims that cover it"
    );
    assert_eq!(awaiting, INSTANCES - needed);

    let ledger = registry.condemned_memory(0);
    assert_eq!(ledger.pending_bytes, REQUESTED_BYTES);
    assert_eq!(ledger.stale_bytes, 0);
    assert_eq!(
        victims
            .iter()
            .filter(|victim| victim.pending_kill().is_some())
            .count(),
        needed
    );
}

/// The ledger is derived from the live instances, so a victim that
/// actually reaches its call hook and is torn down stops covering
/// anything the moment its memory is back.
#[test]
fn reclaimed_memory_leaves_the_ledger() {
    let registry = InstanceRegistry::new();
    let requester = registry.register("requester", 0);
    let victim = registry.register("victim", 0);
    victim.set_memory_bytes(8 * 1024 * 1024);
    let survivor = registry.register("survivor", 0);
    survivor.set_memory_bytes(4 * 1024 * 1024);

    let decision = registry.condemn_for_oom(requester.id(), 1024 * 1024, 0);
    assert!(matches!(decision.outcome, OomKillOutcome::Condemned(_)));
    assert_eq!(decision.condemned.pending_bytes, 8 * 1024 * 1024);

    // Teardown: the victim's last handle drops and its memory is back.
    drop(victim);
    assert_eq!(registry.condemned_memory(0).pending_bytes, 0);

    // A shortfall after the reclaim is a fresh decision again.
    let decision = registry.condemn_for_oom(requester.id(), 1024 * 1024, 0);
    assert!(matches!(
        decision.outcome,
        OomKillOutcome::Condemned(victim) if victim.id == survivor.id()
    ));
}

/// The bound on the ledger's honesty. A victim parked in a host future
/// may never reach a call hook, and the epoch bump cannot reach it, so
/// its condemnation stops covering requests after `OOM_RECLAIM_GRACE`.
/// The next victim is then condemned, while the stale condemnation stays
/// on the books — and out of victim selection — until it is torn down.
#[test]
fn a_condemnation_stops_covering_when_its_grace_expires() {
    const REQUESTED_BYTES: u64 = 1024 * 1024;
    let grace = OOM_RECLAIM_GRACE.as_nanos() as u64;

    let registry = InstanceRegistry::new();
    let requester = registry.register("requester", 0);
    let stuck = registry.register("stuck-in-a-host-call", 0);
    stuck.set_memory_bytes(8 * 1024 * 1024);
    let next = registry.register("next", 0);
    next.set_memory_bytes(4 * 1024 * 1024);

    let decision = registry.condemn_for_oom(requester.id(), REQUESTED_BYTES, 0);
    assert!(matches!(
        decision.outcome,
        OomKillOutcome::Condemned(victim) if victim.id == stuck.id()
    ));

    // Inside the window the condemned bytes still cover the request.
    let decision = registry.condemn_for_oom(requester.id(), REQUESTED_BYTES, grace - 1);
    assert_eq!(decision.outcome, OomKillOutcome::AwaitingReclaim);
    assert_eq!(decision.condemned.pending_bytes, 8 * 1024 * 1024);
    assert_eq!(decision.condemned.stale_bytes, 0);

    // The window expires and the victim is still holding its memory:
    // the next victim is condemned rather than waiting forever.
    let decision = registry.condemn_for_oom(requester.id(), REQUESTED_BYTES, grace);
    assert!(matches!(
        decision.outcome,
        OomKillOutcome::Condemned(victim) if victim.id == next.id()
    ));
    assert_eq!(decision.condemned.pending_bytes, 4 * 1024 * 1024);
    assert_eq!(
        decision.condemned.stale_bytes,
        8 * 1024 * 1024,
        "a stale condemnation stays counted until the instance is torn down"
    );

    // Stale is not forgotten: the instance stays condemned and is never
    // picked a second time.
    assert_eq!(stuck.pending_kill(), Some(KillReason::OutOfMemory));
    assert!(registry.pick_oom_victim().is_none());
}

/// The requester is not condemned to serve its own grow, and no smaller
/// instance is condemned in its place: the instance already holding the
/// most memory takes the failure itself.
#[test]
fn the_requester_is_never_its_own_victim() {
    let registry = InstanceRegistry::new();
    let requester = registry.register("hog", 0);
    requester.set_memory_bytes(512 * 1024 * 1024);
    let small = registry.register("small", 0);
    small.set_memory_bytes(1024 * 1024);

    let decision = registry.condemn_for_oom(requester.id(), 1024 * 1024, 0);
    assert_eq!(decision.outcome, OomKillOutcome::NoVictim);
    assert_eq!(requester.pending_kill(), None);
    assert_eq!(small.pending_kill(), None);
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
