extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use slab::Slab;
use spin::Mutex;
use triomphe::Arc;

const INACTIVE_RESUME_AT: u64 = u64::MAX;

/// Default OOM-killer cost score for plain user-mode programs.
pub const DEFAULT_RESTART_COST: u32 = 1;
/// Cost score for kernel plugins (e.g. compiler). Higher than user
/// programs because restarting is expensive (plugin runtime cache
/// rebuild, in-flight compile request loss), but finite so the OOM
/// killer can still pick them when they are the dominant memory
/// consumer.
pub const PLUGIN_RESTART_COST: u32 = 100;
/// Cost score for embedded system components (debugger). Highest of
/// all, so the OOM killer only picks them when there is no other
/// viable victim — an absolute last resort that nonetheless does
/// terminate when memory pressure forces it, since system components
/// are restartable.
pub const SYSTEM_COMPONENT_RESTART_COST: u32 = 1_000_000;

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

/// Snapshot of an OOM victim selected by [`InstanceRegistry::pick_oom_victim`].
///
/// `score` is the ranking metric (`memory_bytes / restart_cost`) — the
/// higher the score, the more attractive the victim. Callers do not
/// need to interpret it; it is exposed so the kernel can log victim
/// selection decisions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OomVictim {
    pub id: InstanceId,
    pub name: String,
    pub memory_bytes: u64,
    pub restart_cost: u32,
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
    /// Cost weight for OOM victim selection. Higher = harder to kill.
    /// See `DEFAULT_RESTART_COST` and friends.
    restart_cost: u32,
    /// Set by the OOM killer / supervisor; checked on each call_hook
    /// transition. When set, the next host-call boundary returns
    /// `Killed { reason }` instead of resuming the guest.
    kill_flag: AtomicU8,
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
        self.register_with_cost(name, started_at, DEFAULT_RESTART_COST)
    }

    /// Register with an explicit OOM-killer restart cost. Callers that
    /// run system components or kernel plugins use the higher
    /// constants (`PLUGIN_RESTART_COST`, `SYSTEM_COMPONENT_RESTART_COST`)
    /// so the OOM killer prefers cheaper victims.
    pub fn register_with_cost(
        &self,
        name: impl Into<String>,
        started_at: u64,
        restart_cost: u32,
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
            restart_cost,
            kill_flag: AtomicU8::new(KILL_FLAG_NONE),
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

    /// Pick a victim instance using the standard `memory / restart_cost`
    /// heuristic: large memory consumers with low restart cost are
    /// chosen first, system components last. Returns `None` when no
    /// instance has any memory attributed to it.
    ///
    /// The caller must follow up with [`RegisteredInstance::request_kill`]
    /// or use the registry-level helper that does both.
    pub fn pick_oom_victim(&self) -> Option<OomVictim> {
        let entries = self.inner.entries.lock();
        let mut best: Option<OomVictim> = None;
        for (_, entry) in entries.iter() {
            let memory_bytes = entry.memory_bytes.load(Ordering::Acquire);
            if memory_bytes == 0 {
                continue;
            }
            if entry.kill_flag.load(Ordering::Acquire) != KILL_FLAG_NONE {
                // Already condemned; do not re-pick.
                continue;
            }
            let cost = entry.restart_cost.max(1) as u64;
            let score = memory_bytes / cost;
            match &best {
                Some(current) if current.score >= score => {}
                _ => {
                    best = Some(OomVictim {
                        id: entry.id,
                        name: entry.name.clone(),
                        memory_bytes,
                        restart_cost: entry.restart_cost,
                        score,
                    });
                }
            }
        }
        best
    }

    /// Mark `id` for termination. The next host-call boundary in that
    /// instance returns the recorded `KillReason` instead of resuming
    /// the guest. Returns true if the kill flag was actually flipped
    /// (i.e. the instance existed and was not already condemned).
    ///
    /// On a successful flip, the runtime engine epoch is bumped so
    /// any guest currently running without host calls hits its
    /// next `epoch_deadline_async_yield` boundary quickly and exposes
    /// the kill flag to `call_hook`. Without this kick a CPU-bound
    /// victim could run until its next host call, which on adversarial
    /// workloads is "indefinitely".
    pub fn request_kill(&self, id: InstanceId, reason: KillReason) -> bool {
        let entries = self.inner.entries.lock();
        let Some(entry) = entries
            .iter()
            .find_map(|(_, entry)| if entry.id == id { Some(entry) } else { None })
        else {
            return false;
        };
        let encoded = encode_kill_reason(reason);
        let flipped = entry
            .kill_flag
            .compare_exchange(KILL_FLAG_NONE, encoded, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        drop(entries);
        if flipped {
            let notify = *self.inner.kill_notifier.lock();
            notify();
        }
        flipped
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

    /// Every registered instance that has not been condemned, most
    /// recently active last.
    pub fn live_instances(&self) -> Vec<InstanceId> {
        let entries = self.inner.entries.lock();
        let mut live: Vec<(u64, InstanceId)> = entries
            .iter()
            .filter(|(_, entry)| entry.kill_flag.load(Ordering::Acquire) == KILL_FLAG_NONE)
            .map(|(_, entry)| (entry.last_active_at.load(Ordering::Acquire), entry.id))
            .collect();
        live.sort_unstable();
        live.into_iter().map(|(_, id)| id).collect()
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

pub fn record_instance_transition(
    instance: &RegisteredInstance,
    transition: InstanceExecutionTransition,
    now_nanos: u64,
) -> Option<u64> {
    instance.transition(transition, now_nanos)
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

    pub fn restart_cost(&self) -> u32 {
        self.entry().restart_cost
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

    /// Mark this instance for termination. The next call_hook
    /// transition observes the flag and the executor returns the
    /// recorded reason rather than resuming the guest.
    pub fn request_kill(&self, reason: KillReason) {
        let encoded = encode_kill_reason(reason);
        let _ = self.entry().kill_flag.compare_exchange(
            KILL_FLAG_NONE,
            encoded,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub fn transition(
        &self,
        transition: InstanceExecutionTransition,
        now_nanos: u64,
    ) -> Option<u64> {
        match transition {
            InstanceExecutionTransition::Resume => {
                self.entry().resume(now_nanos);
                None
            }
            InstanceExecutionTransition::Pause => self.entry().pause(now_nanos),
        }
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

    fn resume(&self, now_nanos: u64) {
        let previous_depth = self.active_depth.fetch_add(1, Ordering::AcqRel);
        if previous_depth == 0 {
            self.last_resume_at.store(now_nanos, Ordering::Release);
        }
    }

    fn pause(&self, now_nanos: u64) -> Option<u64> {
        let previous_depth = self.active_depth.fetch_sub(1, Ordering::AcqRel);
        assert!(
            previous_depth != 0,
            "instance {} paused while already inactive",
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
    use super::{InstanceExecutionTransition, InstanceRegistry};

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

        instance.transition(InstanceExecutionTransition::Resume, 220);
        let second = registry.snapshot(260);
        assert_eq!(second[0].cpu_busy, 666);

        instance.transition(InstanceExecutionTransition::Pause, 280);
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
