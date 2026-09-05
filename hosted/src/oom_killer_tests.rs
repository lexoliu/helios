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
use helios_kernel::{
    ActivityChange, InstanceActivity, InstanceExecutionTransition, InstanceRegistry,
    KernelHeapHeadroom, KillReason, MemoryPool, OOM_RECLAIM_GRACE, OomKillOutcome, OomPolicy,
    store_kernel_heap_bytes, user_mapping_kernel_heap_bytes,
};

/// A store value's own size, standing in for the wasmtime store the
/// kernel really allocates. The exact number does not matter to these
/// tests — only that every store carries the charge the kernel gives
/// it, through the same function the runtime adapter calls.
const TEST_STORE_BYTES: usize = 4096;

/// What one store costs the kernel heap.
fn test_store_charge() -> u64 {
    store_kernel_heap_bytes(TEST_STORE_BYTES)
}

#[test]
fn pick_skips_instances_with_zero_memory() {
    let registry = InstanceRegistry::new();
    let _idle = registry.register("idle", 0);
    assert!(registry.pick_oom_victim(MemoryPool::User).is_none());
}

#[test]
fn pick_chooses_largest_consumer_when_costs_match() {
    let registry = InstanceRegistry::new();
    let small = registry.register("small", 0);
    let large = registry.register("large", 0);
    small.set_memory_bytes(64 * 1024 * 1024);
    large.set_memory_bytes(512 * 1024 * 1024);

    let victim = registry
        .pick_oom_victim(MemoryPool::User)
        .expect("a victim is available");
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

    let victim = registry
        .pick_oom_victim(MemoryPool::User)
        .expect("a victim is available");
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
        registry.pick_oom_victim(MemoryPool::User).is_none(),
        "a system component holding every byte is still not a victim"
    );

    // A user program with a sliver of memory is, and stays, the pick.
    let user = registry.register_with_policy("user", 0, OomPolicy::UserProgram);
    user.set_memory_bytes(1024 * 1024);

    let victim = registry
        .pick_oom_victim(MemoryPool::User)
        .expect("a victim is available");
    assert_eq!(victim.id, user.id());

    // Once that user program is condemned there is no one left to
    // condemn: the requester takes the grow failure instead.
    assert!(registry.request_kill(victim.id, KillReason::OutOfMemory, 0));
    assert!(
        registry.pick_oom_victim(MemoryPool::User).is_none(),
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

    let first = registry
        .pick_oom_victim(MemoryPool::User)
        .expect("first victim");
    assert_eq!(first.id, big.id());
    assert!(registry.request_kill(first.id, KillReason::OutOfMemory, 0));

    // big is now condemned; the next pick must move on to small.
    let second = registry
        .pick_oom_victim(MemoryPool::User)
        .expect("second victim");
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
            .condemn_for_oom(requester.id(), MemoryPool::User, REQUESTED_BYTES, 0)
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

    let ledger = registry.condemned_memory(MemoryPool::User, 0);
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

    let decision = registry.condemn_for_oom(requester.id(), MemoryPool::User, 1024 * 1024, 0);
    assert!(matches!(decision.outcome, OomKillOutcome::Condemned(_)));
    assert_eq!(decision.condemned.pending_bytes, 8 * 1024 * 1024);

    // Teardown: the victim's last handle drops and its memory is back.
    drop(victim);
    assert_eq!(
        registry.condemned_memory(MemoryPool::User, 0).pending_bytes,
        0
    );

    // A shortfall after the reclaim is a fresh decision again.
    let decision = registry.condemn_for_oom(requester.id(), MemoryPool::User, 1024 * 1024, 0);
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

    let decision = registry.condemn_for_oom(requester.id(), MemoryPool::User, REQUESTED_BYTES, 0);
    assert!(matches!(
        decision.outcome,
        OomKillOutcome::Condemned(victim) if victim.id == stuck.id()
    ));

    // Inside the window the condemned bytes still cover the request.
    let decision =
        registry.condemn_for_oom(requester.id(), MemoryPool::User, REQUESTED_BYTES, grace - 1);
    assert_eq!(decision.outcome, OomKillOutcome::AwaitingReclaim);
    assert_eq!(decision.condemned.pending_bytes, 8 * 1024 * 1024);
    assert_eq!(decision.condemned.stale_bytes, 0);

    // The window expires and the victim is still holding its memory:
    // the next victim is condemned rather than waiting forever.
    let decision =
        registry.condemn_for_oom(requester.id(), MemoryPool::User, REQUESTED_BYTES, grace);
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
    assert!(registry.pick_oom_victim(MemoryPool::User).is_none());
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

    let decision = registry.condemn_for_oom(requester.id(), MemoryPool::User, 1024 * 1024, 0);
    assert_eq!(decision.outcome, OomKillOutcome::NoVictim);
    assert_eq!(requester.pending_kill(), None);
    assert_eq!(small.pending_kill(), None);
}

/// Issue #114. The spawner holding a hundred children is condemned by
/// the OOM killer while it is on a processor, and the call hook that
/// observes the condemnation has already recorded the transition it is
/// about to abort.
///
/// Wasmtime brackets a host call with `CallingHost`/`ReturningFromHost`
/// and a wasm entry with `CallingWasm`/`ReturningFromWasm`, and it fires
/// `ReturningFromWasm` even when the guest trapped. A hook that records
/// the `CallingHost` pause and then returns `Err(InstanceKilled)`
/// cancels the host call, so `ReturningFromHost` never arrives — while
/// the trap still unwinds through `ReturningFromWasm`. That second pause
/// has no resume behind it, and before the activation became an owned
/// state machine it drove the entry's counter below zero and panicked
/// the kernel on a user-mode victim.
#[test]
fn a_condemned_spawner_survives_the_hooks_that_unwind_its_kill_trap() {
    /// What the spawner held when the bench lane condemned it.
    const SPAWNER_BYTES: u64 = 3_932_160;
    /// The grow each child was refused.
    const REQUESTED_BYTES: u64 = 1_114_112;
    const CHILDREN: usize = 100;

    let registry = InstanceRegistry::new();
    let spawner = registry.register("/bin/procbench", 0);
    spawner.set_memory_bytes(SPAWNER_BYTES);
    let children: Vec<_> = (0..CHILDREN)
        .map(|_| {
            let child = registry.register("procbench-child", 0);
            child.set_memory_bytes(64 * 1024);
            child
        })
        .collect();

    // The spawner is inside guest code: `CallingWasm`.
    let mut activity = InstanceActivity::new(spawner.clone(), test_store_charge());
    let step = activity.record(InstanceExecutionTransition::Resume, 10);
    assert_eq!(step.change, ActivityChange::Entered);
    assert_eq!(step.killed, None);

    // Two children's grows are refused. The first condemns the spawner,
    // the second is covered by the ledger and condemns nothing further.
    let first = registry.condemn_for_oom(children[0].id(), MemoryPool::User, REQUESTED_BYTES, 20);
    assert!(
        matches!(first.outcome, OomKillOutcome::Condemned(victim) if victim.id == spawner.id()),
        "the spawner is the highest-scoring victim"
    );
    let second = registry.condemn_for_oom(children[1].id(), MemoryPool::User, REQUESTED_BYTES, 21);
    assert_eq!(second.outcome, OomKillOutcome::AwaitingReclaim);

    // The spawner reaches a host call. The hook records the pause and
    // gets the condemnation back with the activation already ended, so
    // the trap it raises cancels the host call safely.
    let step = activity.record(InstanceExecutionTransition::Pause, 30);
    assert_eq!(step.killed, Some(KillReason::OutOfMemory));
    assert_eq!(
        step.change,
        ActivityChange::Left {
            instance_elapsed: Some(20)
        }
    );

    // The trap unwinds and wasmtime reports the wasm frame leaving too.
    // There is no activation left for it to close.
    let step = activity.record(InstanceExecutionTransition::Pause, 31);
    assert_eq!(step.change, ActivityChange::Unchanged);
    assert_eq!(step.killed, Some(KillReason::OutOfMemory));

    // The victim is still a live registry entry on its way out, and the
    // kernel is still running.
    assert_eq!(
        registry
            .condemned_memory(MemoryPool::User, 31)
            .pending_bytes,
        SPAWNER_BYTES
    );
}

/// The mirror image of the same defect. When the condemnation is first
/// seen on a *resume* hook — `CallingWasm`, or `ReturningFromHost` — the
/// call the hook aborts is the one whose `ReturningFromWasm` would have
/// closed the activation, so nothing closes it. The instance would read
/// as permanently on a processor: never idle, never swappable, and
/// billed for CPU it is not using.
#[test]
fn a_condemnation_seen_on_a_resume_hook_still_closes_the_activation() {
    let registry = InstanceRegistry::new();
    let victim = registry.register("/bin/procbench", 0);
    victim.set_memory_bytes(4 * 1024 * 1024);
    let requester = registry.register("procbench-child", 0);
    requester.set_memory_bytes(64 * 1024);

    assert!(registry.request_kill(victim.id(), KillReason::OutOfMemory, 5));

    // The victim was in a host call when it was condemned, and the hook
    // that sees the flag is the one returning to wasm.
    let mut activity = InstanceActivity::new(victim.clone(), test_store_charge());
    let step = activity.record(InstanceExecutionTransition::Resume, 10);
    assert_eq!(step.killed, Some(KillReason::OutOfMemory));
    assert_eq!(
        step.change,
        ActivityChange::Left {
            instance_elapsed: Some(0)
        },
        "the resume the hook just recorded is closed again before it traps"
    );

    // Nothing accrues after the trap: the instance is off the processor.
    let _ = registry.snapshot(1_000);
    let busy = registry
        .snapshot(2_000)
        .into_iter()
        .find(|snapshot| snapshot.id == victim.id())
        .expect("the victim is still registered")
        .cpu_busy;
    assert_eq!(busy, 0, "a killed instance must not keep billing CPU");
    drop(requester);
}

/// The activation is released when its owner is dropped, not only when
/// wasmtime reports the pause. A store torn down mid-guest — a
/// cancelled task, a future dropped while its fiber is suspended —
/// never delivers `ReturningFromWasm`.
#[test]
fn dropping_the_owner_releases_its_activation() {
    let registry = InstanceRegistry::new();
    let instance = registry.register("cancelled", 0);

    let mut activity = InstanceActivity::new(instance.clone(), test_store_charge());
    activity.record(InstanceExecutionTransition::Resume, 100);
    drop(activity);

    let _ = registry.snapshot(1_000);
    let busy = registry
        .snapshot(2_000)
        .into_iter()
        .find(|snapshot| snapshot.id == instance.id())
        .expect("the instance is still registered")
        .cpu_busy;
    assert_eq!(busy, 0);
}

/// Issue #120, first defect. The kernel-heap branch of the grow
/// admission path subtracted the whole growth from the kernel heap's
/// availability — after the user-pool branch above it had already
/// established that the growth comes out of the user pool. What a grow
/// costs the *kernel* heap is the page tables and reservation records
/// that address the new pages, around a five-hundredth of it, so grows
/// the kernel heap could fund hundreds of times over were refused.
#[test]
fn a_grow_is_charged_its_page_tables_not_its_pages() {
    const GROWTH: usize = 256 * 1024 * 1024;
    const RESERVE: usize = 128 * 1024 * 1024;
    const SPARE: usize = 4 * 1024 * 1024;

    let cost = user_mapping_kernel_heap_bytes(GROWTH);
    assert!(
        cost < SPARE,
        "a {GROWTH}-byte grow must cost the kernel heap less than {SPARE} bytes, not {cost}"
    );

    // The kernel heap has 4 MiB above its reserve: far less than the
    // growth, and far more than the page tables the growth needs. The
    // old check refused this; the pages are not the kernel heap's to
    // fund.
    let headroom = KernelHeapHeadroom {
        available_bytes: RESERVE + SPARE,
        reserve_bytes: RESERVE,
    };
    assert_eq!(headroom.growth_shortfall_bytes(GROWTH), None);

    // The reserve is still a reserve. With nothing free above it, the
    // grow is refused — and the shortfall it asks the OOM killer to
    // reclaim is the kernel-side cost, not the growth.
    let exhausted = KernelHeapHeadroom {
        available_bytes: RESERVE,
        reserve_bytes: RESERVE,
    };
    assert_eq!(exhausted.growth_shortfall_bytes(GROWTH), Some(cost));

    // The #114 shape: free was already under the reserve, so the
    // refusal never depended on the size of the grow. It still does
    // not, and the kernel heap is still defended.
    let breached = KernelHeapHeadroom {
        available_bytes: RESERVE - SPARE,
        reserve_bytes: RESERVE,
    };
    assert_eq!(
        breached.growth_shortfall_bytes(GROWTH),
        Some(cost + SPARE),
        "a heap under its reserve owes the page tables plus the breach"
    );

    // A grow of nothing costs nothing, even with the reserve breached.
    assert_eq!(breached.growth_shortfall_bytes(0), None);
}

/// Issue #120, second defect. `pick_oom_victim` ranked by wasm linear
/// memory whatever ran out, so a kernel-heap shortfall condemned the
/// biggest holder of the pool that was *not* full. The two rankings
/// genuinely disagree: one instance can hold the largest linear memory
/// in the system while another, with a small memory and a store per
/// guest thread, holds the most kernel heap.
#[test]
fn a_kernel_heap_shortfall_condemns_the_largest_kernel_footprint() {
    const GUEST_THREADS: usize = 64;

    let registry = InstanceRegistry::new();

    // One large linear memory on a single store.
    let hog = registry.register("/bin/big-memory", 0);
    hog.set_memory_bytes(512 * 1024 * 1024);
    let _hog_store = InstanceActivity::new(hog.clone(), test_store_charge());

    // A small memory, but a store per `wasi:thread-spawn` thread — and
    // a store is kernel heap: the store value itself plus the page
    // tables for the fiber stack it runs on.
    let threaded = registry.register("/bin/many-threads", 0);
    threaded.set_memory_bytes(4 * 1024 * 1024);
    let _threaded_stores: Vec<_> = (0..GUEST_THREADS)
        .map(|_| InstanceActivity::new(threaded.clone(), test_store_charge()))
        .collect();

    assert!(hog.memory_bytes() > threaded.memory_bytes());
    assert!(threaded.kernel_heap_bytes() > hog.kernel_heap_bytes());

    let user_victim = registry
        .pick_oom_victim(MemoryPool::User)
        .expect("a user-pool victim is available");
    assert_eq!(
        user_victim.id,
        hog.id(),
        "a user-pool shortfall still ranks by linear memory"
    );

    let kernel_victim = registry
        .pick_oom_victim(MemoryPool::Kernel)
        .expect("a kernel-heap victim is available");
    assert_eq!(
        kernel_victim.id,
        threaded.id(),
        "a kernel-heap shortfall must rank by kernel-heap footprint"
    );
    assert_eq!(kernel_victim.pool, MemoryPool::Kernel);
    assert_eq!(kernel_victim.score, threaded.kernel_heap_bytes());
}

/// A store's kernel heap leaves the instance's footprint when the store
/// does. Attribution travels with ownership because the kernel
/// allocator cannot be asked who an allocation was for.
#[test]
fn a_dropped_store_returns_its_kernel_heap_to_the_instance() {
    let registry = InstanceRegistry::new();
    let instance = registry.register("/bin/threaded", 0);
    let base = instance.kernel_heap_bytes();

    let first = InstanceActivity::new(instance.clone(), test_store_charge());
    let second = InstanceActivity::new(instance.clone(), test_store_charge());
    assert_eq!(instance.kernel_heap_bytes(), base + 2 * test_store_charge());

    drop(second);
    assert_eq!(instance.kernel_heap_bytes(), base + test_store_charge());
    drop(first);
    assert_eq!(instance.kernel_heap_bytes(), base);
}

/// #105's ledger has to count the pool the shortfall is in. A
/// condemnation records what the victim held in both pools, so a
/// kernel-heap shortfall is answered against kernel-heap bytes and a
/// second shortfall those bytes cover condemns nothing further.
#[test]
fn the_ledger_counts_the_pool_the_shortfall_is_in() {
    const MEMORY_BYTES: u64 = 64 * 1024 * 1024;

    let registry = InstanceRegistry::new();
    let requester = registry.register("requester", 0);
    let victim = registry.register("/bin/procbench", 0);
    victim.set_memory_bytes(MEMORY_BYTES);
    let _victim_store = InstanceActivity::new(victim.clone(), test_store_charge());
    let kernel_bytes = victim.kernel_heap_bytes();
    assert!(
        kernel_bytes < MEMORY_BYTES,
        "the two pools are different numbers"
    );

    let decision =
        registry.condemn_for_oom(requester.id(), MemoryPool::Kernel, kernel_bytes / 2, 0);
    assert!(
        matches!(decision.outcome, OomKillOutcome::Condemned(ref chosen) if chosen.id == victim.id())
    );
    assert_eq!(decision.condemned.pending_bytes, kernel_bytes);

    assert_eq!(
        registry
            .condemned_memory(MemoryPool::Kernel, 0)
            .pending_bytes,
        kernel_bytes
    );
    assert_eq!(
        registry.condemned_memory(MemoryPool::User, 0).pending_bytes,
        MEMORY_BYTES,
        "the same condemnation is on both ledgers, at each pool's own size"
    );

    // The kernel-heap bytes already condemned cover a second shortfall
    // of the same size, so no second live instance is condemned.
    let second = registry.condemn_for_oom(requester.id(), MemoryPool::Kernel, kernel_bytes / 2, 0);
    assert_eq!(second.outcome, OomKillOutcome::AwaitingReclaim);
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
