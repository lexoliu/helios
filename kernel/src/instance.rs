extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroU32;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use core::time::Duration;

use slab::Slab;
use spin::Mutex;
use triomphe::Arc;

const INACTIVE_RESUME_AT: u64 = u64::MAX;

/// What the OOM killer is allowed to do with an instance.
///
/// The kernel's own components are not on the menu. A cost score, however
/// high, only postpones the moment a user program's demand for memory
/// condemns the debugger or the compiler — and losing one of those is
/// fatal, so the ranking is between user-mode instances and the kernel's
/// infrastructure is outside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OomPolicy {
    /// An ordinary user-mode program: the OOM killer's first choice,
    /// ranked by the memory it holds.
    UserProgram,
    /// A kernel plugin (compiler, http client). Restartable, but
    /// restarting costs a runtime cache rebuild and every in-flight
    /// request, so it is picked only when it dominates memory.
    KernelPlugin,
    /// A component the kernel provisions and depends on. Never a
    /// victim: user memory pressure must not be able to take down the
    /// kernel's own services.
    SystemComponent,
}

impl OomPolicy {
    /// The weight a victim's memory is divided by when ranking, or
    /// `None` when the instance is not a candidate at all.
    pub const fn restart_cost(self) -> Option<u32> {
        match self {
            Self::UserProgram => Some(1),
            Self::KernelPlugin => Some(100),
            Self::SystemComponent => None,
        }
    }

    /// Whether the OOM killer may condemn an instance under this policy.
    pub const fn is_victim_candidate(self) -> bool {
        self.restart_cost().is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstanceId(u64);

impl InstanceId {
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Reason an instance was marked for termination by the OOM killer
/// or the kernel-plugin supervisor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KillReason {
    /// Picked by the OOM killer to free user memory for the system.
    OutOfMemory,
    /// Supervisor restart after a fault or quarantine breach.
    SupervisorRestart,
}

const KILL_FLAG_NONE: u8 = 0;
const KILL_FLAG_OOM: u8 = 1;
const KILL_FLAG_SUPERVISOR: u8 = 2;

const fn encode_kill_reason(reason: KillReason) -> u8 {
    match reason {
        KillReason::OutOfMemory => KILL_FLAG_OOM,
        KillReason::SupervisorRestart => KILL_FLAG_SUPERVISOR,
    }
}

const fn decode_kill_flag(value: u8) -> Option<KillReason> {
    match value {
        KILL_FLAG_OOM => Some(KillReason::OutOfMemory),
        KILL_FLAG_SUPERVISOR => Some(KillReason::SupervisorRestart),
        _ => None,
    }
}

/// How long the OOM killer waits for a condemned instance's memory
/// before it will condemn another one to serve the same shortfall.
///
/// Condemning an instance does not free anything by itself: the kill
/// flag is observed at the victim's next call-hook boundary, and the
/// memory returns when its store is torn down and its registry entry
/// drops. [`InstanceRegistry::request_kill`] bumps the runtime engine
/// epoch, so a victim executing wasm reaches that boundary at its next
/// epoch check, and the scheduler tick that drives those runs every
/// 100ms (`SCHEDULER_INTERRUPT_INTERVAL`). Five ticks leaves room for
/// the trap to unwind and the store to drop.
///
/// A victim parked in a host future that the epoch cannot reach — a
/// socket read with no peer, a host-fs request whose reply never comes
/// — may never reach a call hook at all, so the wait has to be bounded.
/// When the window expires the condemnation goes stale: it stops
/// counting as coverage, so the next request condemns a fresh victim,
/// while the stale condemnation stays on the books and stays out of
/// victim selection until the instance is actually torn down.
pub const OOM_RECLAIM_GRACE: Duration = Duration::from_millis(500);

/// User memory the OOM killer has condemned and not yet seen returned.
///
/// Reclaim is recorded by the victim's registry entry disappearing when
/// its last handle drops, so this ledger is derived from the live
/// instances rather than kept as a counter that could drift away from
/// them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CondemnedMemory {
    /// Condemned within [`OOM_RECLAIM_GRACE`]: memory the killer still
    /// expects back, and the amount that covers a pending grow request.
    pub pending_bytes: u64,
    /// Condemned longer ago than [`OOM_RECLAIM_GRACE`]. Still condemned
    /// and still not a candidate for a second condemnation, but no
    /// longer counted as coverage: the instance may never reach a call
    /// hook, and a shortfall cannot wait on it forever.
    pub stale_bytes: u64,
}

impl CondemnedMemory {
    /// Whether the memory already condemned and still expected back
    /// covers a request for `requested_bytes`.
    pub const fn covers(&self, requested_bytes: u64) -> bool {
        self.pending_bytes >= requested_bytes
    }
}

/// What [`InstanceRegistry::condemn_for_oom`] did with a grow request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OomKillOutcome {
    /// This request condemned `victim`; its memory is expected back
    /// within [`OOM_RECLAIM_GRACE`].
    Condemned(OomVictim),
    /// Memory already condemned covers the request. The requester takes
    /// the typed grow failure and retries once the reclaim lands, rather
    /// than condemning another live instance for memory that is already
    /// on its way back.
    AwaitingReclaim,
    /// Nothing eligible is left to condemn: the requester takes the grow
    /// failure with no victim.
    NoVictim,
}

/// The outcome of a grow request together with the ledger it was
/// decided against, as it stands after the decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OomKillDecision {
    pub outcome: OomKillOutcome,
    pub condemned: CondemnedMemory,
}

/// Snapshot of an OOM victim selected by [`InstanceRegistry::pick_oom_victim`].
///
/// `score` is the ranking metric (`memory_bytes / restart cost`) — the
/// higher the score, the more attractive the victim. Callers do not
/// need to interpret it; it is exposed so the kernel can log victim
/// selection decisions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OomVictim {
    pub id: InstanceId,
    pub name: String,
    pub memory_bytes: u64,
    pub policy: OomPolicy,
    pub score: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstanceSnapshot {
    pub id: InstanceId,
    pub name: String,
    pub started_at: u64,
    pub uptime: u64,
    pub memory_bytes: u64,
    pub cpu_busy: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstanceProfileTotal {
    pub name: String,
    pub active_nanos: u64,
}

#[derive(Clone)]
pub struct InstanceRegistry {
    inner: Arc<InstanceRegistryInner>,
}

pub struct RegisteredInstance {
    registry: InstanceRegistry,
    entry_slot: usize,
    entry: NonNull<InstanceEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstanceExecutionTransition {
    Resume,
    Pause,
}

struct InstanceRegistryInner {
    next_id: AtomicU64,
    entries: Mutex<Slab<Box<InstanceEntry>>>,
    sampling: Mutex<SamplingState>,
    kill_notifier: Mutex<fn()>,
}

struct InstanceEntry {
    id: InstanceId,
    name: String,
    started_at: u64,
    memory_bytes: AtomicU64,
    active_nanos: AtomicU64,
    active_depth: AtomicU32,
    last_resume_at: AtomicU64,
    /// When this instance was last on a processor. Set on every pause,
    /// so an instance that is running right now carries the timestamp of
    /// its previous stretch and `active_depth` tells the two apart. The
    /// swap policy reads both to decide whether an instance has been
    /// idle long enough to evict.
    last_active_at: AtomicU64,
    /// What the OOM killer may do with this instance.
    policy: OomPolicy,
    /// Set by the OOM killer / supervisor; checked on each call_hook
    /// transition. When set, the next host-call boundary returns
    /// `Killed { reason }` instead of resuming the guest.
    kill_flag: AtomicU8,
    /// Monotonic nanoseconds at which the kill flag was set, and the
    /// memory attributed to the instance at that moment. Together they
    /// are this instance's entry in the condemned-memory ledger: what
    /// the killer expects back, and when it started expecting it.
    ///
    /// Both are written and read under the registry's `entries` lock,
    /// alongside the flag flip itself, so no reader can see a
    /// condemnation whose bytes have not been recorded yet.
    condemned_at: AtomicU64,
    condemned_bytes: AtomicU64,
    handle_count: AtomicUsize,
}

#[derive(Default)]
struct SamplingState {
    last_at: Option<u64>,
    totals: Vec<SampleTotal>,
}

struct SampleTotal {
    id: InstanceId,
    active_nanos: u64,
}

impl InstanceRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(InstanceRegistryInner {
                next_id: AtomicU64::new(1),
                entries: Mutex::new(Slab::new()),
                sampling: Mutex::new(SamplingState::default()),
                kill_notifier: Mutex::new(noop_kill_notifier),
            }),
        }
    }

    pub fn set_kill_notifier(&self, notifier: fn()) {
        *self.inner.kill_notifier.lock() = notifier;
    }

    pub fn register(&self, name: impl Into<String>, started_at: u64) -> RegisteredInstance {
        self.register_with_policy(name, started_at, OomPolicy::UserProgram)
    }

    /// Register under an explicit OOM policy. Kernel plugins and system
    /// components say so here; everything else is a user program.
    pub fn register_with_policy(
        &self,
        name: impl Into<String>,
        started_at: u64,
        policy: OomPolicy,
    ) -> RegisteredInstance {
        let id = InstanceId(self.inner.next_id.fetch_add(1, Ordering::AcqRel));
        let entry = Box::new(InstanceEntry {
            id,
            name: name.into(),
            started_at,
            memory_bytes: AtomicU64::new(0),
            active_nanos: AtomicU64::new(0),
            active_depth: AtomicU32::new(0),
            last_resume_at: AtomicU64::new(INACTIVE_RESUME_AT),
            last_active_at: AtomicU64::new(started_at),
            policy,
            kill_flag: AtomicU8::new(KILL_FLAG_NONE),
            condemned_at: AtomicU64::new(0),
            condemned_bytes: AtomicU64::new(0),
            handle_count: AtomicUsize::new(1),
        });
        let entry_ptr = NonNull::from(entry.as_ref());
        let entry_slot = self.inner.entries.lock().insert(entry);
        RegisteredInstance {
            registry: self.clone(),
            entry_slot,
            entry: entry_ptr,
        }
    }

    /// Pick a victim instance using the standard `memory / restart cost`
    /// heuristic: large memory consumers with low restart cost are
    /// chosen first, and system components never. Returns `None` when
    /// no candidate instance has memory attributed to it.
    ///
    /// Instances that are already condemned are not candidates: their
    /// memory is on the condemned ledger ([`Self::condemned_memory`])
    /// rather than available to condemn again.
    ///
    /// This is victim selection on its own. The OOM killer's entry point
    /// is [`Self::condemn_for_oom`], which weighs the ledger against the
    /// request before it selects anything.
    pub fn pick_oom_victim(&self) -> Option<OomVictim> {
        best_victim(&self.inner.entries.lock())
    }

    /// User memory condemned and not yet reclaimed, as of `now_nanos`.
    ///
    /// Reclaim is the victim's registry entry dropping with its last
    /// handle, so an instance stops contributing here exactly when its
    /// memory is back.
    pub fn condemned_memory(&self, now_nanos: u64) -> CondemnedMemory {
        condemned_ledger(&self.inner.entries.lock(), now_nanos)
    }

    /// The OOM killer's decision for one refused grow of
    /// `requested_bytes` by `requester`.
    ///
    /// Condemning a victim only sets its kill flag; the memory returns
    /// when that instance reaches its next call-hook boundary and is
    /// torn down. Until it does, those bytes are already spoken for, so
    /// a second refused grow that they would satisfy must not condemn a
    /// second live instance — otherwise one 1MiB shortfall walks the
    /// whole workload, condemning an instance per attempt while the
    /// first reclaim is still in flight.
    ///
    /// So: while the memory condemned within [`OOM_RECLAIM_GRACE`]
    /// covers the request, the answer is [`OomKillOutcome::AwaitingReclaim`]
    /// and the requester takes its typed grow failure and retries.
    /// Once that window expires on a victim that never reached a call
    /// hook, its bytes stop counting as coverage and the next request
    /// condemns a fresh victim; the stale condemnation stays on the
    /// ledger as [`CondemnedMemory::stale_bytes`], and stays out of
    /// victim selection, until the instance is torn down.
    ///
    /// The requester is never its own victim: when it is the
    /// highest-scoring candidate the answer is
    /// [`OomKillOutcome::NoVictim`] and it takes the failure itself,
    /// rather than a smaller instance dying to feed it.
    ///
    /// Selection and condemnation happen under one lock, so two
    /// processors refusing a grow at the same moment cannot both
    /// condemn against the same empty ledger.
    pub fn condemn_for_oom(
        &self,
        requester: InstanceId,
        requested_bytes: u64,
        now_nanos: u64,
    ) -> OomKillDecision {
        let entries = self.inner.entries.lock();
        let mut condemned = condemned_ledger(&entries, now_nanos);
        if condemned.covers(requested_bytes) {
            return OomKillDecision {
                outcome: OomKillOutcome::AwaitingReclaim,
                condemned,
            };
        }
        // Avoid suiciding: when the highest-scoring victim is the
        // requester itself, the killer hands the grow failure back to it
        // rather than marking it for termination — and does not fall
        // through to a smaller instance, which would condemn a modest
        // program to feed the one already holding the most memory.
        // Other instances become the pick on later attempts as they
        // accumulate memory.
        let victim = best_victim(&entries).filter(|victim| victim.id != requester);
        let Some(victim) = victim else {
            return OomKillDecision {
                outcome: OomKillOutcome::NoVictim,
                condemned,
            };
        };
        let condemned_bytes =
            condemn_entry(&entries, victim.id, KillReason::OutOfMemory, now_nanos)
                .expect("victim selected under the registry lock must still be condemnable");
        condemned.pending_bytes = condemned.pending_bytes.saturating_add(condemned_bytes);
        drop(entries);
        self.notify_kill();
        OomKillDecision {
            outcome: OomKillOutcome::Condemned(victim),
            condemned,
        }
    }

    /// Mark `id` for termination as of `now_nanos`. The next host-call
    /// boundary in that instance returns the recorded `KillReason`
    /// instead of resuming the guest. Returns true if the kill flag was
    /// actually flipped (i.e. the instance existed and was not already
    /// condemned).
    ///
    /// A successful flip puts the instance's memory on the condemned
    /// ledger, timestamped with `now_nanos`: whatever the reason for the
    /// kill, those bytes are on their way back and the OOM killer must
    /// not condemn a second instance for memory it is already getting.
    ///
    /// On a successful flip, the runtime engine epoch is bumped so
    /// any guest currently running without host calls hits its
    /// next `epoch_deadline_async_yield` boundary quickly and exposes
    /// the kill flag to `call_hook`. Without this kick a CPU-bound
    /// victim could run until its next host call, which on adversarial
    /// workloads is "indefinitely". It is also what bounds the ledger's
    /// wait: see [`OOM_RECLAIM_GRACE`].
    pub fn request_kill(&self, id: InstanceId, reason: KillReason, now_nanos: u64) -> bool {
        let entries = self.inner.entries.lock();
        let flipped = condemn_entry(&entries, id, reason, now_nanos).is_some();
        drop(entries);
        if flipped {
            self.notify_kill();
        }
        flipped
    }

    fn notify_kill(&self) {
        let notify = *self.inner.kill_notifier.lock();
        notify();
    }

    /// Instances that have not run on any processor for at least
    /// `idle_after_nanos`, newest activity last.
    ///
    /// An instance that is on a processor right now is never idle no
    /// matter how long its previous stretch was, and an instance
    /// condemned by the OOM killer is skipped: its memory is about to
    /// come back for free, and writing it to disk first would be work
    /// thrown away.
    pub fn idle_instances(&self, now_nanos: u64, idle_after_nanos: u64) -> Vec<InstanceId> {
        let entries = self.inner.entries.lock();
        let mut idle: Vec<(u64, InstanceId)> = entries
            .iter()
            .filter_map(|(_, entry)| {
                if entry.active_depth.load(Ordering::Acquire) != 0 {
                    return None;
                }
                if entry.kill_flag.load(Ordering::Acquire) != KILL_FLAG_NONE {
                    return None;
                }
                let last_active = entry.last_active_at.load(Ordering::Acquire);
                let idle_for = now_nanos.saturating_sub(last_active);
                (idle_for >= idle_after_nanos).then_some((last_active, entry.id))
            })
            .collect();
        idle.sort_unstable();
        idle.into_iter().map(|(_, id)| id).collect()
    }

    pub fn snapshot(&self, now_nanos: u64) -> Vec<InstanceSnapshot> {
        let entries = self.inner.entries.lock();
        let mut sampling = self.inner.sampling.lock();
        let elapsed_nanos = sampling
            .last_at
            .map(|last_at| now_nanos.saturating_sub(last_at))
            .unwrap_or(0);
        sampling.last_at = Some(now_nanos);

        let mut snapshots = Vec::with_capacity(entries.len());
        let mut next_totals = Vec::with_capacity(entries.len());

        for (_, entry) in entries.iter() {
            let total_active_nanos = entry.total_active_nanos(now_nanos);
            let previous_total = sampling
                .totals
                .iter()
                .find(|sample| sample.id == entry.id)
                .map(|sample| sample.active_nanos)
                .unwrap_or(total_active_nanos);
            let delta_active_nanos = total_active_nanos.saturating_sub(previous_total);
            let cpu_busy = delta_active_nanos
                .saturating_mul(1_000)
                .checked_div(elapsed_nanos)
                .unwrap_or(0)
                .min(1_000) as u16;

            next_totals.push(SampleTotal {
                id: entry.id,
                active_nanos: total_active_nanos,
            });
            snapshots.push(InstanceSnapshot {
                id: entry.id,
                name: entry.name.clone(),
                started_at: entry.started_at,
                uptime: now_nanos.saturating_sub(entry.started_at),
                memory_bytes: entry.memory_bytes.load(Ordering::Acquire),
                cpu_busy,
            });
        }

        snapshots.sort_by_key(|snapshot| snapshot.id);
        sampling.totals = next_totals;
        snapshots
    }

    pub fn active_totals(&self, now_nanos: u64) -> Vec<InstanceProfileTotal> {
        let mut totals = self
            .inner
            .entries
            .lock()
            .iter()
            .map(|(_, entry)| InstanceProfileTotal {
                name: entry.name.clone(),
                active_nanos: entry.total_active_nanos(now_nanos),
            })
            .collect::<Vec<_>>();
        totals.sort_by(|left, right| left.name.cmp(&right.name));
        totals
    }
}

impl Default for InstanceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn noop_kill_notifier() {}

/// Highest-scoring OOM candidate in `entries`.
///
/// Called with the registry's `entries` lock held.
fn best_victim(entries: &Slab<Box<InstanceEntry>>) -> Option<OomVictim> {
    let mut best: Option<OomVictim> = None;
    for (_, entry) in entries.iter() {
        let memory_bytes = entry.memory_bytes.load(Ordering::Acquire);
        if memory_bytes == 0 {
            continue;
        }
        if entry.kill_flag.load(Ordering::Acquire) != KILL_FLAG_NONE {
            // Already condemned: its memory is on the ledger, not on the menu.
            continue;
        }
        // Kernel infrastructure is not a candidate at any score.
        let Some(cost) = entry.policy.restart_cost() else {
            continue;
        };
        let score = memory_bytes / u64::from(cost.max(1));
        match &best {
            Some(current) if current.score >= score => {}
            _ => {
                best = Some(OomVictim {
                    id: entry.id,
                    name: entry.name.clone(),
                    memory_bytes,
                    policy: entry.policy,
                    score,
                });
            }
        }
    }
    best
}

/// Sum the memory of every condemned instance in `entries`, split at the
/// [`OOM_RECLAIM_GRACE`] deadline.
///
/// Called with the registry's `entries` lock held.
fn condemned_ledger(entries: &Slab<Box<InstanceEntry>>, now_nanos: u64) -> CondemnedMemory {
    let grace_nanos = OOM_RECLAIM_GRACE.as_nanos() as u64;
    let mut ledger = CondemnedMemory::default();
    for (_, entry) in entries.iter() {
        if entry.kill_flag.load(Ordering::Acquire) == KILL_FLAG_NONE {
            continue;
        }
        let bytes = entry.condemned_bytes.load(Ordering::Acquire);
        let waited = now_nanos.saturating_sub(entry.condemned_at.load(Ordering::Acquire));
        if waited < grace_nanos {
            ledger.pending_bytes = ledger.pending_bytes.saturating_add(bytes);
        } else {
            ledger.stale_bytes = ledger.stale_bytes.saturating_add(bytes);
        }
    }
    ledger
}

/// Flip `id`'s kill flag and record its ledger entry, returning the
/// bytes now condemned, or `None` when the instance is gone or was
/// already condemned.
///
/// Called with the registry's `entries` lock held, which is what keeps
/// the flag and its ledger entry from being observed out of step.
fn condemn_entry(
    entries: &Slab<Box<InstanceEntry>>,
    id: InstanceId,
    reason: KillReason,
    now_nanos: u64,
) -> Option<u64> {
    let entry = entries
        .iter()
        .find_map(|(_, entry)| (entry.id == id).then_some(entry))?;
    let encoded = encode_kill_reason(reason);
    entry
        .kill_flag
        .compare_exchange(KILL_FLAG_NONE, encoded, Ordering::AcqRel, Ordering::Acquire)
        .ok()?;
    let condemned_bytes = entry.memory_bytes.load(Ordering::Acquire);
    entry.condemned_at.store(now_nanos, Ordering::Release);
    entry
        .condemned_bytes
        .store(condemned_bytes, Ordering::Release);
    Some(condemned_bytes)
}

pub fn allow_instance_resource_growth(
    instance: &RegisteredInstance,
    desired: usize,
    maximum: Option<usize>,
) -> bool {
    let allow = maximum.is_none_or(|maximum| desired <= maximum);
    if allow {
        instance.set_memory_bytes(u64::try_from(desired).expect("desired memory exceeds u64"));
    }
    allow
}

impl RegisteredInstance {
    fn entry(&self) -> &InstanceEntry {
        // SAFETY: each live handle increments `handle_count`; the registry
        // removes the boxed entry only when the final handle drops.
        unsafe { self.entry.as_ref() }
    }

    pub fn id(&self) -> InstanceId {
        self.entry().id
    }

    pub fn name(&self) -> &str {
        &self.entry().name
    }

    pub fn started_at(&self) -> u64 {
        self.entry().started_at
    }

    pub fn oom_policy(&self) -> OomPolicy {
        self.entry().policy
    }

    pub fn memory_bytes(&self) -> u64 {
        self.entry().memory_bytes.load(Ordering::Acquire)
    }

    pub fn set_memory_bytes(&self, memory_bytes: u64) {
        self.entry()
            .memory_bytes
            .store(memory_bytes, Ordering::Release);
    }

    /// Returns `Some(reason)` when the instance has been condemned by
    /// the OOM killer or a supervisor and the runtime should trap on
    /// the next host-call boundary instead of resuming the guest.
    pub fn pending_kill(&self) -> Option<KillReason> {
        decode_kill_flag(self.entry().kill_flag.load(Ordering::Acquire))
    }
}

impl Clone for RegisteredInstance {
    fn clone(&self) -> Self {
        self.entry().handle_count.fetch_add(1, Ordering::AcqRel);
        Self {
            registry: self.registry.clone(),
            entry_slot: self.entry_slot,
            entry: self.entry,
        }
    }
}

unsafe impl Send for RegisteredInstance {}
unsafe impl Sync for RegisteredInstance {}

impl Drop for RegisteredInstance {
    fn drop(&mut self) {
        let entry = self.entry();
        let previous_handles = entry.handle_count.fetch_sub(1, Ordering::AcqRel);
        assert!(
            previous_handles != 0,
            "registered instance handle count underflow"
        );
        if previous_handles != 1 {
            return;
        }
        let id = entry.id;
        let mut entries = self.registry.inner.entries.lock();
        if entries
            .get(self.entry_slot)
            .is_some_and(|entry| entry.id == id)
        {
            let _ = entries.remove(self.entry_slot);
        } else {
            panic!("registered instance entry disappeared before final handle drop");
        }
        drop(entries);
        self.registry
            .inner
            .sampling
            .lock()
            .totals
            .retain(|sample| sample.id != id);
    }
}

/// What one recorded transition did to the processor the reporting
/// store is running on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityChange {
    /// The store entered the instance's guest code: what this processor
    /// commits from here on belongs to that instance.
    Entered,
    /// The store left the instance's guest code. `instance_elapsed` is
    /// `Some` when no activation is left anywhere, carrying the
    /// nanoseconds the instance held a processor.
    Left { instance_elapsed: Option<u64> },
    /// Nothing changed hands: a nested transition, or one reported
    /// after the activation had already ended.
    Unchanged,
}

impl ActivityChange {
    /// Fold the change made by ending an activation into the change the
    /// transition itself made. Releasing the activation always wins:
    /// once it is gone the processor is not the instance's any more.
    fn then(self, ending: Self) -> Self {
        match ending {
            Self::Left { .. } => ending,
            Self::Entered | Self::Unchanged => self,
        }
    }
}

/// The result of reporting one call-hook transition to an
/// [`InstanceActivity`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActivityStep {
    /// What the transition did to this processor's ownership.
    pub change: ActivityChange,
    /// Set when the instance has been condemned. The activation is
    /// already ended when this is `Some`, so the caller's only
    /// remaining job is to raise the trap.
    pub killed: Option<KillReason>,
}

/// One store's activity accounting for the instance it runs.
///
/// [`RegisteredInstance`] is a handle: cloneable, shared, readable by
/// anyone. Activity is not. The registry entry counts how many stores
/// are inside an instance's guest code — a `wasi:thread-spawn` thread
/// gets its own store over the same instance — but the resume/pause
/// pairing behind each of those counts belongs to exactly one owner.
/// `InstanceActivity` is that owner: it is not `Clone`, it holds at
/// most one activation on the entry, and it is the only thing that
/// opens or closes one.
///
/// The kill path is part of the same ownership. Wasmtime pairs its call
/// hooks around the call it is making, so a hook that records a
/// transition and *then* returns an error cancels the call whose
/// matching hook would have closed that transition — while the
/// `ReturningFromWasm` that unwinds the resulting trap still arrives.
/// [`Self::record`] therefore looks for the condemnation itself and
/// ends the activation in the same step. That ended state is
/// terminal, so the transitions wasmtime reports while the trap unwinds
/// find no activation to close rather than a counter to drive below
/// zero.
pub struct InstanceActivity {
    instance: RegisteredInstance,
    state: ActivityState,
    /// The last timestamp this owner was given, so a store dropped
    /// while its guest is suspended — a cancelled task, a future
    /// dropped on its fiber — still closes its activation instead of
    /// leaving the instance on a processor forever.
    last_nanos: u64,
}

enum ActivityState {
    /// Not inside guest code; no activation held.
    Idle,
    /// Inside guest code, `depth` call-hook levels deep, holding one
    /// activation on the registry entry.
    Running(NonZeroU32),
    /// A call hook raised a trap the guest never returns from — an OOM
    /// or supervisor kill, or a signal exit. Any activation was
    /// released then, and this state is terminal.
    Ended,
}

impl InstanceActivity {
    pub fn new(instance: RegisteredInstance) -> Self {
        let last_nanos = instance.started_at();
        Self {
            instance,
            state: ActivityState::Idle,
            last_nanos,
        }
    }

    pub fn instance(&self) -> &RegisteredInstance {
        &self.instance
    }

    /// The condemnation recorded for this instance, if any.
    pub fn pending_kill(&self) -> Option<KillReason> {
        self.instance.pending_kill()
    }

    /// Report the transition a call hook just delivered.
    ///
    /// The condemnation check lives here rather than in the caller
    /// because the two cannot be separated safely: a hook that records
    /// a transition and then refuses to let the guest continue has told
    /// the registry about an activation whose closing hook will never
    /// fire. Ending the activation in the same step is what makes the
    /// trailing unwind transitions harmless.
    pub fn record(
        &mut self,
        transition: InstanceExecutionTransition,
        now_nanos: u64,
    ) -> ActivityStep {
        self.last_nanos = now_nanos;
        let change = match transition {
            InstanceExecutionTransition::Resume => self.enter(now_nanos),
            InstanceExecutionTransition::Pause => self.leave(now_nanos),
        };
        match self.instance.pending_kill() {
            Some(reason) => {
                let ending = self.end(now_nanos);
                ActivityStep {
                    change: change.then(ending),
                    killed: Some(reason),
                }
            }
            None => ActivityStep {
                change,
                killed: None,
            },
        }
    }

    /// End this store's activation because a trap the guest never
    /// returns from is being raised. Terminal and idempotent.
    pub fn end(&mut self, now_nanos: u64) -> ActivityChange {
        self.last_nanos = now_nanos;
        let change = match self.state {
            ActivityState::Running(_) => ActivityChange::Left {
                instance_elapsed: self.instance.entry().close_activation(now_nanos),
            },
            ActivityState::Idle | ActivityState::Ended => ActivityChange::Unchanged,
        };
        self.state = ActivityState::Ended;
        change
    }

    fn enter(&mut self, now_nanos: u64) -> ActivityChange {
        match self.state {
            ActivityState::Idle => {
                self.instance.entry().open_activation(now_nanos);
                self.state = ActivityState::Running(NonZeroU32::MIN);
                ActivityChange::Entered
            }
            ActivityState::Running(depth) => {
                self.state = ActivityState::Running(depth.saturating_add(1));
                ActivityChange::Unchanged
            }
            ActivityState::Ended => ActivityChange::Unchanged,
        }
    }

    fn leave(&mut self, now_nanos: u64) -> ActivityChange {
        match self.state {
            ActivityState::Running(depth) => match NonZeroU32::new(depth.get() - 1) {
                Some(remaining) => {
                    self.state = ActivityState::Running(remaining);
                    ActivityChange::Unchanged
                }
                None => {
                    self.state = ActivityState::Idle;
                    ActivityChange::Left {
                        instance_elapsed: self.instance.entry().close_activation(now_nanos),
                    }
                }
            },
            // A store whose pauses outnumber its resumes while its
            // activation is still live is a kernel bookkeeping bug: the
            // owner that would have opened the activation is this one.
            // Kernel-owned invariant, so it stays fatal.
            ActivityState::Idle => panic!(
                "instance {} paused while already inactive",
                self.instance.name()
            ),
            ActivityState::Ended => ActivityChange::Unchanged,
        }
    }
}

impl Drop for InstanceActivity {
    fn drop(&mut self) {
        // A store dropped mid-guest — a cancelled task, a future
        // dropped while its fiber is suspended — never reports the
        // pause wasmtime would have. Close the activation at the last
        // timestamp this owner saw, so the instance does not read as
        // permanently on a processor.
        let _ = self.end(self.last_nanos);
    }
}

impl InstanceEntry {
    fn total_active_nanos(&self, now_nanos: u64) -> u64 {
        let total = self.active_nanos.load(Ordering::Acquire);
        if self.active_depth.load(Ordering::Acquire) == 0 {
            return total;
        }

        let resumed_at = self.last_resume_at.load(Ordering::Acquire);
        if resumed_at == INACTIVE_RESUME_AT {
            return total;
        }

        total.saturating_add(now_nanos.saturating_sub(resumed_at))
    }

    /// Open one activation on this instance. Called only by the
    /// [`InstanceActivity`] that opened it, which is the single owner
    /// of the pairing behind this count.
    fn open_activation(&self, now_nanos: u64) {
        if self.active_depth.fetch_add(1, Ordering::AcqRel) == 0 {
            self.last_resume_at.store(now_nanos, Ordering::Release);
        }
    }

    /// Close one activation, returning how long the instance held a
    /// processor when this was the last one.
    fn close_activation(&self, now_nanos: u64) -> Option<u64> {
        let previous_depth = self.active_depth.fetch_sub(1, Ordering::AcqRel);
        // Every close is made by the owner that made the matching open,
        // so a count that has already reached zero here is the
        // registry's own bookkeeping going wrong, not a user-load
        // condition. Kernel-owned invariant: panic.
        assert!(
            previous_depth != 0,
            "instance {} closed an activation it never opened",
            self.name
        );

        if previous_depth == 1 {
            let resumed_at = self
                .last_resume_at
                .swap(INACTIVE_RESUME_AT, Ordering::AcqRel);
            assert!(
                resumed_at != INACTIVE_RESUME_AT,
                "instance {} lost its resume timestamp",
                self.name
            );
            let elapsed = now_nanos.saturating_sub(resumed_at);
            self.active_nanos.fetch_add(elapsed, Ordering::AcqRel);
            self.last_active_at.store(now_nanos, Ordering::Release);
            return Some(elapsed);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{InstanceActivity, InstanceExecutionTransition, InstanceRegistry};

    #[test]
    fn assigns_stable_ids() {
        let registry = InstanceRegistry::new();
        let first = registry.register("init", 10);
        let second = registry.register("worker", 20);

        assert_eq!(first.id().raw(), 1);
        assert_eq!(second.id().raw(), 2);
    }

    #[test]
    fn snapshot_reports_recent_cpu_and_memory() {
        let registry = InstanceRegistry::new();
        let instance = registry.register("worker", 100);
        instance.set_memory_bytes(4096);

        let first = registry.snapshot(200);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].cpu_busy, 0);
        assert_eq!(first[0].memory_bytes, 4096);

        let mut activity = InstanceActivity::new(instance.clone());
        activity.record(InstanceExecutionTransition::Resume, 220);
        let second = registry.snapshot(260);
        assert_eq!(second[0].cpu_busy, 666);

        activity.record(InstanceExecutionTransition::Pause, 280);
        let third = registry.snapshot(320);
        assert_eq!(third[0].cpu_busy, 333);
    }

    #[test]
    fn dropping_instance_removes_it_from_future_snapshots() {
        let registry = InstanceRegistry::new();
        let instance = registry.register("init", 0);
        assert_eq!(registry.snapshot(1).len(), 1);
        drop(instance);
        assert!(registry.snapshot(2).is_empty());
    }

    #[test]
    fn cloned_instance_handle_keeps_registry_entry_alive() {
        let registry = InstanceRegistry::new();
        let instance = registry.register("worker", 0);
        let clone = instance.clone();
        drop(instance);

        let snapshots = registry.snapshot(1);
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].name, "worker");

        drop(clone);
        assert!(registry.snapshot(2).is_empty());
    }
}
