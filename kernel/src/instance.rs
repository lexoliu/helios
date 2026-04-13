extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use spin::Mutex;

const INACTIVE_RESUME_AT: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstanceId(u64);

impl InstanceId {
    pub const fn raw(self) -> u64 {
        self.0
    }
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

#[derive(Clone)]
pub struct InstanceRegistry {
    inner: Arc<InstanceRegistryInner>,
}

pub struct RegisteredInstance {
    registry: InstanceRegistry,
    entry: Arc<InstanceEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstanceExecutionTransition {
    Resume,
    Pause,
}

struct InstanceRegistryInner {
    next_id: AtomicU64,
    entries: Mutex<Vec<Arc<InstanceEntry>>>,
    sampling: Mutex<SamplingState>,
}

struct InstanceEntry {
    id: InstanceId,
    name: String,
    started_at: u64,
    memory_bytes: AtomicU64,
    active_nanos: AtomicU64,
    active_depth: AtomicU32,
    last_resume_at: AtomicU64,
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
                entries: Mutex::new(Vec::new()),
                sampling: Mutex::new(SamplingState::default()),
            }),
        }
    }

    pub fn register(&self, name: impl Into<String>, started_at: u64) -> RegisteredInstance {
        let id = InstanceId(self.inner.next_id.fetch_add(1, Ordering::AcqRel));
        let entry = Arc::new(InstanceEntry {
            id,
            name: name.into(),
            started_at,
            memory_bytes: AtomicU64::new(0),
            active_nanos: AtomicU64::new(0),
            active_depth: AtomicU32::new(0),
            last_resume_at: AtomicU64::new(INACTIVE_RESUME_AT),
        });
        self.inner.entries.lock().push(entry.clone());
        RegisteredInstance {
            registry: self.clone(),
            entry,
        }
    }

    pub fn snapshot(&self, now_nanos: u64) -> Vec<InstanceSnapshot> {
        let entries = self.inner.entries.lock().clone();
        let mut sampling = self.inner.sampling.lock();
        let elapsed_nanos = sampling
            .last_at
            .map(|last_at| now_nanos.saturating_sub(last_at))
            .unwrap_or(0);
        sampling.last_at = Some(now_nanos);

        let mut snapshots = Vec::with_capacity(entries.len());
        let mut next_totals = Vec::with_capacity(entries.len());

        for entry in entries {
            let total_active_nanos = entry.total_active_nanos(now_nanos);
            let previous_total = sampling
                .totals
                .iter()
                .find(|sample| sample.id == entry.id)
                .map(|sample| sample.active_nanos)
                .unwrap_or(total_active_nanos);
            let delta_active_nanos = total_active_nanos.saturating_sub(previous_total);
            let cpu_busy = if elapsed_nanos == 0 {
                0
            } else {
                ((delta_active_nanos.saturating_mul(1_000)) / elapsed_nanos).min(1_000) as u16
            };

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
}

impl Default for InstanceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub fn record_instance_transition(
    instance: &RegisteredInstance,
    transition: InstanceExecutionTransition,
    now_nanos: u64,
) {
    instance.transition(transition, now_nanos);
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
    pub fn id(&self) -> InstanceId {
        self.entry.id
    }

    pub fn name(&self) -> &str {
        &self.entry.name
    }

    pub fn started_at(&self) -> u64 {
        self.entry.started_at
    }

    pub fn set_memory_bytes(&self, memory_bytes: u64) {
        self.entry
            .memory_bytes
            .store(memory_bytes, Ordering::Release);
    }

    pub fn transition(&self, transition: InstanceExecutionTransition, now_nanos: u64) {
        match transition {
            InstanceExecutionTransition::Resume => self.entry.resume(now_nanos),
            InstanceExecutionTransition::Pause => self.entry.pause(now_nanos),
        }
    }
}

impl Drop for RegisteredInstance {
    fn drop(&mut self) {
        let id = self.entry.id;
        self.registry
            .inner
            .entries
            .lock()
            .retain(|entry| entry.id != id);
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

    fn pause(&self, now_nanos: u64) {
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
            self.active_nanos
                .fetch_add(now_nanos.saturating_sub(resumed_at), Ordering::AcqRel);
        }
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
}
