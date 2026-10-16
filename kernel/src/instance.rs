extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use core::time::Duration;

use crossbeam_utils::CachePadded;
use helios_hal::cpu::current_processor;
use slab::Slab;
use spin::Mutex;
use triomphe::Arc;

use crate::memory::{MemoryOwner, UserMemoryOwnerScope, enter_user_memory_owner};
use crate::user_mapping_kernel_heap_bytes;

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

/// One of the kernel's two memory ownership domains.
///
/// They are separate pools with separate accounting (AGENTS §3): a wasm
/// instance's pages come from the user pool, while the page tables,
/// reservation records and store state that describe them come from the
/// kernel heap. Killing an instance returns bytes to both, but *how
/// many* it returns to each is a different number, and an instance that
/// dominates one pool need not dominate the other — a program holding
/// one large linear memory is the biggest user-pool consumer in the
/// system while a program holding forty guest threads over a small
/// memory is the biggest kernel-heap consumer. So the ranking has to be
/// told which pool ran out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryPool {
    /// The user-memory pool wasm linear memories are served from.
    User,
    /// The kernel heap.
    Kernel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstanceId(u64);

impl InstanceId {
    /// The identifier an inspector or a guest names an instance by.
    ///
    /// Nothing about the number is meaningful outside the registry that
    /// issued it: an identifier that names no live instance is answered
    /// with a "no such instance", never with a different one.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

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
    /// Asked for by an operator, through the inspector.
    Operator,
}

/// What came of a request to stop an instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KillOutcome {
    /// The instance is flagged and unwinds at its next yield point.
    Requested,
    /// It was already condemned, by the OOM killer or an earlier
    /// request, and the reason it is going stays the first one.
    AlreadyStopping,
    /// No live instance carries that identifier.
    NoSuchInstance,
}

const KILL_FLAG_NONE: u8 = 0;
const KILL_FLAG_OOM: u8 = 1;
const KILL_FLAG_SUPERVISOR: u8 = 2;
const KILL_FLAG_OPERATOR: u8 = 3;

const fn encode_kill_reason(reason: KillReason) -> u8 {
    match reason {
        KillReason::OutOfMemory => KILL_FLAG_OOM,
        KillReason::SupervisorRestart => KILL_FLAG_SUPERVISOR,
        KillReason::Operator => KILL_FLAG_OPERATOR,
    }
}

const fn decode_kill_flag(value: u8) -> Option<KillReason> {
    match value {
        KILL_FLAG_OOM => Some(KillReason::OutOfMemory),
        KILL_FLAG_SUPERVISOR => Some(KillReason::SupervisorRestart),
        KILL_FLAG_OPERATOR => Some(KillReason::Operator),
        _ => None,
    }
}

/// How long the OOM killer waits for a condemned instance's memory
/// before it will condemn another one to serve the same shortfall.
///
/// Condemning an instance does not free anything by itself: the kill
/// flag is observed at the victim's next interrupt check, and the memory
/// returns when its store is torn down and its registry entry drops.
/// Whoever condemns the instance also interrupts its stores, so a victim
/// executing wasm traps at its next function entry or loop header; five
/// idle interrupt intervals (`IDLE_INTERRUPT_INTERVAL`) leave room for the
/// trap to unwind and the store to drop.
///
/// A victim parked in a host future that never resolves — a socket
/// read with no peer, a host-fs request whose reply never comes — never
/// returns to wasm and so never reaches an interrupt check, so the
/// grace period has to be bounded.
/// When the window expires the condemnation goes stale: it stops
/// counting as coverage, so the next request condemns a fresh victim,
/// while the stale condemnation stays on the books and stays out of
/// victim selection until the instance is actually torn down.
pub const OOM_RECLAIM_GRACE: Duration = Duration::from_millis(500);

/// Memory the OOM killer has condemned and not yet seen returned,
/// counted in one [`MemoryPool`].
///
/// Reclaim is recorded by the victim's registry entry disappearing when
/// its last handle drops, so this ledger is derived from the live
/// instances rather than kept as a counter that could drift away from
/// them. A condemnation records what the victim held in *both* pools,
/// so a shortfall on either one is answered against the bytes that
/// shortfall is actually waiting for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CondemnedMemory {
    /// Condemned within [`OOM_RECLAIM_GRACE`]: memory the killer still
    /// expects back, and the amount that covers a pending grow request.
    pub pending_bytes: u64,
    /// Condemned longer ago than [`OOM_RECLAIM_GRACE`]. Still condemned
    /// and still not a candidate for a second condemnation, but no
    /// longer counted as coverage: the instance may never return to
    /// wasm, and a shortfall cannot wait on it forever.
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
/// `score` is the ranking metric (the bytes this victim holds in `pool`
/// divided by its restart cost) — the higher the score, the more
/// attractive the victim. Callers do not need to interpret it; it is
/// exposed so the kernel can log victim selection decisions. Both pools
/// travel with the victim so a log line says what the kill actually
/// returns, not only the pool it was ranked on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OomVictim {
    pub id: InstanceId,
    pub name: String,
    /// The pool that ran out, and therefore the one `score` ranks.
    pub pool: MemoryPool,
    /// User-pool bytes: the instance's wasm linear memory.
    pub memory_bytes: u64,
    /// Kernel-heap bytes attributed to the instance.
    pub kernel_bytes: u64,
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

struct InstanceRegistryInner {
    next_id: AtomicU64,
    entries: Mutex<Slab<Box<InstanceEntry>>>,
    sampling: Mutex<SamplingState>,
}

struct InstanceEntry {
    id: InstanceId,
    name: String,
    started_at: u64,
    memory_bytes: AtomicU64,
    /// Kernel heap charged to this instance by its owners: one charge
    /// per store it holds, released when that store drops. The
    /// page-table and reservation-record cost of its user memory is
    /// derived from `memory_bytes` instead of charged, so the two can
    /// never drift apart — see [`InstanceEntry::kernel_heap_bytes`].
    kernel_bytes: AtomicU64,
    /// The processor time of every store this instance holds. The swap
    /// policy reads it to decide whether an instance has been idle long
    /// enough to evict; stats and profiles read the total.
    stores: Mutex<StoreClocks>,
    /// What the OOM killer may do with this instance.
    policy: OomPolicy,
    /// Set by the OOM killer / supervisor. The runtime's interrupt
    /// callback reads it and traps the guest with `InstanceKilled { reason }`,
    /// and a guest call that completes while it is set reports the
    /// kill instead of its result.
    kill_flag: AtomicU8,
    /// Monotonic nanoseconds at which the kill flag was set, and what
    /// the instance held in each pool at that moment. Together they are
    /// this instance's entry in the condemned-memory ledger: what the
    /// killer expects back, and when it started expecting it. Both
    /// pools are recorded because either one can be the pool a later
    /// shortfall is waiting on.
    ///
    /// All three are written and read under the registry's `entries`
    /// lock, alongside the flag flip itself, so no reader can see a
    /// condemnation whose bytes have not been recorded yet.
    condemned_at: AtomicU64,
    condemned_user_bytes: AtomicU64,
    condemned_kernel_bytes: AtomicU64,
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
            }),
        }
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
            kernel_bytes: AtomicU64::new(0),
            stores: Mutex::new(StoreClocks::new(started_at)),
            policy,
            kill_flag: AtomicU8::new(KILL_FLAG_NONE),
            condemned_at: AtomicU64::new(0),
            condemned_user_bytes: AtomicU64::new(0),
            condemned_kernel_bytes: AtomicU64::new(0),
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
    /// heuristic against the pool that ran out: large consumers *of
    /// that pool* with low restart cost are chosen first, and system
    /// components never. Returns `None` when no candidate instance
    /// holds anything in `pool`.
    ///
    /// `pool` is what makes the ranking measure the shortfall it is
    /// answering. Ranking a kernel-heap shortfall by wasm linear-memory
    /// size picks the instance holding the most of the pool that is not
    /// full; killing it does return kernel-side state, but only
    /// incidentally, and the instance that would have returned the most
    /// keeps running.
    ///
    /// Instances that are already condemned are not candidates: their
    /// memory is on the condemned ledger ([`Self::condemned_memory`])
    /// rather than available to condemn again.
    ///
    /// This is victim selection on its own. The OOM killer's entry point
    /// is [`Self::condemn_for_oom`], which weighs the ledger against the
    /// request before it selects anything.
    pub fn pick_oom_victim(&self, pool: MemoryPool) -> Option<OomVictim> {
        best_victim(&self.inner.entries.lock(), pool)
    }

    /// Memory condemned out of `pool` and not yet reclaimed, as of
    /// `now_nanos`.
    ///
    /// Reclaim is the victim's registry entry dropping with its last
    /// handle, so an instance stops contributing here exactly when its
    /// memory is back.
    pub fn condemned_memory(&self, pool: MemoryPool, now_nanos: u64) -> CondemnedMemory {
        condemned_ledger(&self.inner.entries.lock(), pool, now_nanos)
    }

    /// The OOM killer's decision for one refused grow by `requester`,
    /// where `requested_bytes` is what has to come back to `pool`
    /// before the same request can be admitted.
    ///
    /// Condemning a victim only sets its kill flag; the memory returns
    /// when that instance traps at its next interrupt check and is torn
    /// down. Until it does, those bytes are already spoken for, so
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
        pool: MemoryPool,
        requested_bytes: u64,
        now_nanos: u64,
    ) -> OomKillDecision {
        let entries = self.inner.entries.lock();
        let mut condemned = condemned_ledger(&entries, pool, now_nanos);
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
        let victim = best_victim(&entries, pool).filter(|victim| victim.id != requester);
        let Some(victim) = victim else {
            return OomKillDecision {
                outcome: OomKillOutcome::NoVictim,
                condemned,
            };
        };
        let condemned_bytes =
            condemn_entry(&entries, victim.id, KillReason::OutOfMemory, now_nanos)
                .expect("victim selected under the registry lock must still be condemnable");
        condemned.pending_bytes = condemned
            .pending_bytes
            .saturating_add(condemned_bytes.of(pool));
        drop(entries);
        OomKillDecision {
            outcome: OomKillOutcome::Condemned(victim),
            condemned,
        }
    }

    /// Mark `id` for termination as of `now_nanos`. The instance's next
    /// interrupt check traps with the recorded `KillReason`, and a guest
    /// call of it that completes first reports the kill instead of its
    /// result.
    ///
    /// A successful flip puts the instance's memory on the condemned
    /// ledger, timestamped with `now_nanos`: whatever the reason for the
    /// kill, those bytes are on their way back and the OOM killer must
    /// not condemn a second instance for memory it is already getting.
    ///
    /// The registry only records the condemnation. The runtime adapter
    /// that asked for it then interrupts every store of the instance, so
    /// a CPU-bound victim traps at its next function entry or loop
    /// header instead of running on, and a guest suspended in a host
    /// call meets the same check as soon as it is back in wasm. The same
    /// holds for [`Self::condemn_for_oom`], and it is what bounds the
    /// ledger's grace period: see [`OOM_RECLAIM_GRACE`].
    pub fn request_kill(&self, id: InstanceId, reason: KillReason, now_nanos: u64) -> KillOutcome {
        let entries = self.inner.entries.lock();
        let outcome = if entries.iter().any(|(_, entry)| entry.id == id) {
            match condemn_entry(&entries, id, reason, now_nanos) {
                Some(_) => KillOutcome::Requested,
                None => KillOutcome::AlreadyStopping,
            }
        } else {
            KillOutcome::NoSuchInstance
        };
        drop(entries);
        outcome
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
                if entry.kill_flag.load(Ordering::Acquire) != KILL_FLAG_NONE {
                    return None;
                }
                let time = entry.stores.lock().processor_time(now_nanos);
                if time.on_processor {
                    return None;
                }
                let last_active = time.last_active_at;
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
            let total_active_nanos = entry.stores.lock().processor_time(now_nanos).active_nanos;
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
                active_nanos: entry.stores.lock().processor_time(now_nanos).active_nanos,
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

/// Highest-scoring OOM candidate in `entries` for the pool that ran
/// out.
///
/// Called with the registry's `entries` lock held.
fn best_victim(entries: &Slab<Box<InstanceEntry>>, pool: MemoryPool) -> Option<OomVictim> {
    let mut best: Option<OomVictim> = None;
    for (_, entry) in entries.iter() {
        let memory_bytes = entry.memory_bytes.load(Ordering::Acquire);
        let kernel_bytes = entry.kernel_heap_bytes();
        let pool_bytes = match pool {
            MemoryPool::User => memory_bytes,
            MemoryPool::Kernel => kernel_bytes,
        };
        if pool_bytes == 0 {
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
        let score = pool_bytes / u64::from(cost.max(1));
        match &best {
            Some(current) if current.score >= score => {}
            _ => {
                best = Some(OomVictim {
                    id: entry.id,
                    name: entry.name.clone(),
                    pool,
                    memory_bytes,
                    kernel_bytes,
                    policy: entry.policy,
                    score,
                });
            }
        }
    }
    best
}

/// Sum the `pool` memory of every condemned instance in `entries`,
/// split at the [`OOM_RECLAIM_GRACE`] deadline.
///
/// Called with the registry's `entries` lock held.
fn condemned_ledger(
    entries: &Slab<Box<InstanceEntry>>,
    pool: MemoryPool,
    now_nanos: u64,
) -> CondemnedMemory {
    let grace_nanos = OOM_RECLAIM_GRACE.as_nanos() as u64;
    let mut ledger = CondemnedMemory::default();
    for (_, entry) in entries.iter() {
        if entry.kill_flag.load(Ordering::Acquire) == KILL_FLAG_NONE {
            continue;
        }
        let bytes = match pool {
            MemoryPool::User => entry.condemned_user_bytes.load(Ordering::Acquire),
            MemoryPool::Kernel => entry.condemned_kernel_bytes.load(Ordering::Acquire),
        };
        let waited = now_nanos.saturating_sub(entry.condemned_at.load(Ordering::Acquire));
        if waited < grace_nanos {
            ledger.pending_bytes = ledger.pending_bytes.saturating_add(bytes);
        } else {
            ledger.stale_bytes = ledger.stale_bytes.saturating_add(bytes);
        }
    }
    ledger
}

/// What one condemnation put on the ledger, in both pools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CondemnedBytes {
    user: u64,
    kernel: u64,
}

impl CondemnedBytes {
    const fn of(self, pool: MemoryPool) -> u64 {
        match pool {
            MemoryPool::User => self.user,
            MemoryPool::Kernel => self.kernel,
        }
    }
}

/// Flip `id`'s kill flag and record its ledger entry, returning the
/// bytes now condemned in each pool, or `None` when the instance is
/// gone or was already condemned.
///
/// Called with the registry's `entries` lock held, which is what keeps
/// the flag and its ledger entry from being observed out of step.
fn condemn_entry(
    entries: &Slab<Box<InstanceEntry>>,
    id: InstanceId,
    reason: KillReason,
    now_nanos: u64,
) -> Option<CondemnedBytes> {
    let entry = entries
        .iter()
        .find_map(|(_, entry)| (entry.id == id).then_some(entry))?;
    let encoded = encode_kill_reason(reason);
    entry
        .kill_flag
        .compare_exchange(KILL_FLAG_NONE, encoded, Ordering::AcqRel, Ordering::Acquire)
        .ok()?;
    let condemned = CondemnedBytes {
        user: entry.memory_bytes.load(Ordering::Acquire),
        kernel: entry.kernel_heap_bytes(),
    };
    entry.condemned_at.store(now_nanos, Ordering::Release);
    entry
        .condemned_user_bytes
        .store(condemned.user, Ordering::Release);
    entry
        .condemned_kernel_bytes
        .store(condemned.kernel, Ordering::Release);
    Some(condemned)
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

    /// Kernel heap attributed to this instance: every live
    /// [`KernelHeapCharge`] against it, plus the page tables and
    /// reservation records its user memory needs.
    pub fn kernel_heap_bytes(&self) -> u64 {
        self.entry().kernel_heap_bytes()
    }

    /// Charge `bytes` of kernel heap to this instance for as long as
    /// the returned charge is alive.
    pub fn charge_kernel_heap(&self, bytes: u64) -> KernelHeapCharge {
        self.entry().kernel_bytes.fetch_add(bytes, Ordering::AcqRel);
        KernelHeapCharge {
            instance: self.clone(),
            bytes,
        }
    }

    /// Returns `Some(reason)` when the instance has been condemned by
    /// the OOM killer or a supervisor and the runtime should trap at
    /// the guest's next interrupt check.
    pub fn pending_kill(&self) -> Option<KillReason> {
        decode_kill_flag(self.entry().kill_flag.load(Ordering::Acquire))
    }
}

/// Kernel heap charged to an instance for as long as the structure that
/// allocated it is alive.
///
/// The kernel allocator cannot be asked who an allocation was for, so
/// attribution travels with ownership instead: whoever allocates kernel
/// heap on an instance's behalf holds one of these, and the bytes leave
/// that instance's footprint exactly when the owner drops — which is
/// also when the allocation itself goes back. One charge per store is
/// what makes an instance's kernel-heap footprint track the stores it
/// holds rather than the linear memory it grew.
pub struct KernelHeapCharge {
    instance: RegisteredInstance,
    bytes: u64,
}

impl KernelHeapCharge {
    /// The bytes this charge holds against its instance.
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for KernelHeapCharge {
    fn drop(&mut self) {
        let bytes = self.bytes;
        let _ = self.instance.entry().kernel_bytes.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |charged| Some(charged.saturating_sub(bytes)),
        );
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

/// A store's processor time, in the one word its driver writes.
///
/// While the store is on a processor the word holds [`ON_PROCESSOR`]
/// over `resumed_at - accumulated`, so the total at any later `now` is
/// `now - base` with no second field to keep consistent with the
/// first; off a processor it holds the accumulated nanoseconds.
///
/// # Concurrency
///
/// One writer: the turn that holds the store, which is one task on one
/// processor at a time, because a store is driven through `&mut`. It
/// writes with plain stores, never a read-modify-write. Readers are the
/// registry's stats, profile and swap passes on any processor, under the
/// owning entry's [`StoreClocks`] lock; the word is a single atomic, so
/// they need no retry loop. The clock is cache-padded, so the line a
/// store's turns write is that store's alone: guest threads of one
/// instance, each on its own store and processor, never share it.
struct StoreClock {
    word: AtomicU64,
    /// When the store last left a processor. Stored before the word
    /// that marks it off-processor, so a reader that sees it off sees
    /// this stretch's end too.
    last_active_at: AtomicU64,
}

/// The bit of [`StoreClock::word`] that says the store is on a
/// processor.
const ON_PROCESSOR: u64 = 1 << 63;

impl StoreClock {
    const fn new() -> Self {
        Self {
            word: AtomicU64::new(0),
            last_active_at: AtomicU64::new(0),
        }
    }

    /// Processor nanoseconds this store has used as of `now_nanos`, and
    /// whether it is on a processor right now.
    fn read(&self, now_nanos: u64) -> (u64, bool) {
        let word = self.word.load(Ordering::Acquire);
        if word & ON_PROCESSOR == 0 {
            (word, false)
        } else {
            (now_nanos.saturating_sub(word & !ON_PROCESSOR), true)
        }
    }
}

/// The processor time of one instance, summed over its stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessorTime {
    active_nanos: u64,
    on_processor: bool,
    last_active_at: u64,
}

/// The clocks of an instance's live stores, and what its dropped ones
/// left behind.
///
/// Locked when a store is opened or dropped and when the registry reads
/// an instance's time, never on a turn: a turn writes only its own
/// [`StoreClock`].
struct StoreClocks {
    live: Vec<Arc<CachePadded<StoreClock>>>,
    retired_active_nanos: u64,
    retired_last_active_at: u64,
}

impl StoreClocks {
    const fn new(started_at: u64) -> Self {
        Self {
            live: Vec::new(),
            retired_active_nanos: 0,
            retired_last_active_at: started_at,
        }
    }

    fn processor_time(&self, now_nanos: u64) -> ProcessorTime {
        let mut time = ProcessorTime {
            active_nanos: self.retired_active_nanos,
            on_processor: false,
            last_active_at: self.retired_last_active_at,
        };
        for clock in &self.live {
            let (active_nanos, on_processor) = clock.read(now_nanos);
            time.active_nanos = time.active_nanos.saturating_add(active_nanos);
            time.on_processor |= on_processor;
            time.last_active_at = time
                .last_active_at
                .max(clock.last_active_at.load(Ordering::Acquire));
        }
        time
    }
}

/// One store's accounting for the instance it runs: the kernel heap
/// the store costs, and the processor time its turns use.
///
/// [`RegisteredInstance`] is a handle: cloneable, shared, readable by
/// anyone. A store's clock is not. `InstanceActivity` is created with
/// the store, registers the store's clock on the instance, and folds
/// that clock into the instance's retired total when the store drops.
/// The turns themselves are taken through [`StoreTurns`], the handle
/// the code driving the store holds while the store is mutably
/// borrowed.
pub struct InstanceActivity {
    turns: StoreTurns,
    /// The kernel heap this store costs, charged to the instance for
    /// exactly as long as the store exists. The activity owner is the
    /// one thing there is precisely one of per store (that is what
    /// makes it the owner), so it is where the per-store charge
    /// belongs.
    _store_charge: KernelHeapCharge,
}

impl InstanceActivity {
    /// Open a store's accounting over `instance`, charging it
    /// `store_kernel_heap_bytes` of kernel heap until this owner drops.
    pub fn new(instance: RegisteredInstance, store_kernel_heap_bytes: u64) -> Self {
        let store_charge = instance.charge_kernel_heap(store_kernel_heap_bytes);
        let clock = Arc::new(CachePadded::new(StoreClock::new()));
        instance.entry().stores.lock().live.push(clock.clone());
        Self {
            turns: StoreTurns { instance, clock },
            _store_charge: store_charge,
        }
    }

    pub fn instance(&self) -> &RegisteredInstance {
        &self.turns.instance
    }

    /// The condemnation recorded for this instance, if any.
    pub fn pending_kill(&self) -> Option<KillReason> {
        self.turns.instance.pending_kill()
    }

    /// A handle for the code about to drive this store.
    pub fn turns(&self) -> StoreTurns {
        self.turns.clone()
    }
}

impl Drop for InstanceActivity {
    fn drop(&mut self) {
        let clock = &self.turns.clock;
        let mut stores = self.turns.instance.entry().stores.lock();
        let position = stores
            .live
            .iter()
            .position(|live| Arc::ptr_eq(live, clock))
            .expect("a store's clock is registered on its instance until the store drops");
        stores.live.swap_remove(position);
        let word = clock.word.load(Ordering::Acquire);
        // A turn borrows the store, so none can be open while the store
        // drops. Kernel-owned invariant.
        assert!(
            word & ON_PROCESSOR == 0,
            "store of instance {} dropped during a processor turn",
            self.turns.instance.name()
        );
        stores.retired_active_nanos = stores.retired_active_nanos.saturating_add(word);
        stores.retired_last_active_at = stores
            .retired_last_active_at
            .max(clock.last_active_at.load(Ordering::Acquire));
    }
}

/// What the code driving a store needs to account the processor turns
/// it gives that store.
///
/// A turn is one stretch in which the store holds a processor: one poll
/// of the future that drives its guest code. It begins when the
/// executor polls that future, inside which the Wasmtime fiber resumes,
/// and ends when the poll returns, which is when the fiber has
/// suspended or finished. A synchronous host call that never suspends
/// happens inside one turn and costs nothing of its own.
#[derive(Clone)]
pub struct StoreTurns {
    instance: RegisteredInstance,
    clock: Arc<CachePadded<StoreClock>>,
}

impl StoreTurns {
    pub fn instance(&self) -> &RegisteredInstance {
        &self.instance
    }

    /// The condemnation recorded for this instance, if any.
    pub fn pending_kill(&self) -> Option<KillReason> {
        self.instance.pending_kill()
    }

    /// Put the store on the executing processor as of `now_nanos`.
    ///
    /// Until the turn ends, user memory this processor commits is the
    /// instance's: see [`ProcessorTurn`].
    pub fn begin(&self, now_nanos: u64) -> ProcessorTurn<'_> {
        let word = self.clock.word.load(Ordering::Relaxed);
        // Turns of one store cannot nest: the driver holds the store by
        // `&mut` for the length of the poll. A nested begin means a
        // second driver was wrapped around a call already inside one.
        assert!(
            word & ON_PROCESSOR == 0,
            "store of instance {} began a processor turn inside another",
            self.instance.name()
        );
        // The owner scope is entered before the clock is marked, so a
        // processor without identity fails before the store is on it.
        let owner = enter_user_memory_owner(
            current_processor(),
            MemoryOwner::new(self.instance.id().raw()),
        );
        self.clock.word.store(
            ON_PROCESSOR | now_nanos.saturating_sub(word),
            Ordering::Release,
        );
        ProcessorTurn {
            turns: self,
            resumed_at: now_nanos,
            _owner: owner,
        }
    }
}

/// One open turn of a store on the executing processor.
///
/// It names the instance as the owner of the user memory the processor
/// commits for as long as it is open, and restores whatever owner was
/// named before when it ends. That covers the guest's synchronous host
/// calls too: what such a call commits in the user address space is the
/// guest's, while what it allocates from the kernel heap is attributed
/// by [`KernelHeapCharge`] ownership and never by this name. A host
/// call that commits memory for a different instance — a spawn
/// instantiating its child — names that owner in its own nested turn.
#[must_use = "a processor turn is ended with the time it ended"]
pub struct ProcessorTurn<'a> {
    turns: &'a StoreTurns,
    resumed_at: u64,
    _owner: UserMemoryOwnerScope<'static>,
}

impl ProcessorTurn<'_> {
    /// Take the store off the processor as of `now_nanos`, answering
    /// how long this turn held it.
    pub fn end(self, now_nanos: u64) -> u64 {
        let clock = &self.turns.clock;
        let base = clock.word.load(Ordering::Relaxed) & !ON_PROCESSOR;
        clock.last_active_at.store(now_nanos, Ordering::Release);
        clock
            .word
            .store(now_nanos.saturating_sub(base), Ordering::Release);
        now_nanos.saturating_sub(self.resumed_at)
    }
}

impl InstanceEntry {
    /// Kernel heap attributed to this instance.
    ///
    /// Two terms, because the kernel knows about its own allocations in
    /// two different ways. What its owners charged it explicitly — one
    /// [`KernelHeapCharge`] per store it holds — is accumulated in
    /// `kernel_bytes`. What its *user* memory costs the kernel heap is
    /// derived from `memory_bytes` through the same model the grow
    /// admission path uses, rather than charged separately: the two
    /// then cannot drift apart, and an instance is never admitted
    /// against one number and ranked on another.
    fn kernel_heap_bytes(&self) -> u64 {
        let memory_bytes = self.memory_bytes.load(Ordering::Acquire);
        let mapped_bytes = usize::try_from(memory_bytes).unwrap_or(usize::MAX);
        let mapping_bytes =
            u64::try_from(user_mapping_kernel_heap_bytes(mapped_bytes)).unwrap_or(u64::MAX);
        self.kernel_bytes
            .load(Ordering::Acquire)
            .saturating_add(mapping_bytes)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::{InstanceActivity, InstanceRegistry};

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

        let activity = InstanceActivity::new(instance.clone(), 0);
        let turns = activity.turns();
        let turn = turns.begin(220);
        let second = registry.snapshot(260);
        assert_eq!(second[0].cpu_busy, 666);

        assert_eq!(turn.end(280), 60);
        let third = registry.snapshot(320);
        assert_eq!(third[0].cpu_busy, 333);
    }

    /// Guest threads each drive their own store. Their turns overlap,
    /// and the instance's time is the sum of the processors it held.
    #[test]
    fn processor_time_sums_the_turns_of_every_store() {
        let registry = InstanceRegistry::new();
        let instance = registry.register("threads", 0);
        let main = InstanceActivity::new(instance.clone(), 0);
        let thread = InstanceActivity::new(instance.clone(), 0);
        let (main_turns, thread_turns) = (main.turns(), thread.turns());

        let main_turn = main_turns.begin(100);
        let thread_turn = thread_turns.begin(150);
        assert_eq!(registry.active_totals(200)[0].active_nanos, 150);
        assert!(registry.idle_instances(200, 0).is_empty());
        assert_eq!(thread_turn.end(200), 50);
        assert_eq!(main_turn.end(300), 200);
        assert_eq!(registry.active_totals(1_000)[0].active_nanos, 250);
        assert_eq!(registry.idle_instances(1_000, 700), vec![instance.id()]);
        assert!(registry.idle_instances(1_000, 701).is_empty());

        // A dropped store's time stays on the instance.
        drop(thread_turns);
        drop(thread);
        assert_eq!(registry.active_totals(1_000)[0].active_nanos, 250);
        let turn = main_turns.begin(2_000);
        assert_eq!(registry.active_totals(2_010)[0].active_nanos, 260);
        assert_eq!(turn.end(2_010), 10);
        drop(main_turns);
        drop(main);
        assert_eq!(registry.active_totals(5_000)[0].active_nanos, 260);
        assert_eq!(registry.idle_instances(5_000, 2_990), vec![instance.id()]);
    }

    #[test]
    #[should_panic(expected = "began a processor turn inside another")]
    fn a_store_cannot_begin_a_turn_inside_another() {
        let registry = InstanceRegistry::new();
        let instance = registry.register("nested", 0);
        let activity = InstanceActivity::new(instance, 0);
        let turns = activity.turns();
        // The store would refuse to drop mid-turn while this unwinds.
        core::mem::forget(activity);
        let _outer = turns.begin(1);
        let _inner = turns.begin(2);
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
