extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::collections::{BinaryHeap, VecDeque};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use core::cmp::Reverse;
use core::hash::BuildHasher;
use core::sync::atomic::{AtomicBool, Ordering};

use arrayvec::{ArrayString, ArrayVec};
use hashbrown::{DefaultHashBuilder, HashTable};
use helios_hal::cpu::HardwarePerfCounterDelta;
use spin::Mutex;
use triomphe::Arc;

use crate::{InstanceId, ObserveMachineCap};

pub const DEFAULT_TRACE_HISTORY_CAPACITY: usize = 512;
pub const DEFAULT_PROFILE_STACK_CAPACITY: usize = 1024;
pub const DEFAULT_PERF_METRIC_CAPACITY: usize = 512;
const OBSERVER_TEXT_STACK_BYTES: usize = 256;

/// Most prefixes one filter holds, in each of the three filter types.
///
/// The bound is fixed by the interface, not by the caller, as
/// [`crate::MAX_PROFILE_READ`] is: a filter arrives from a guest, and what
/// the kernel keeps and matches against every event has to be bounded
/// before the guest sends it. Every producer in the tree stays inside it:
/// the kernel's own filters carry one prefix, and the inspector and `perf`
/// take one repeated flag per prefix, a handful at most. The outside
/// precedent is Linux's fixed limits on caller-supplied filter programs
/// (`BPF_MAXINSNS`, the seccomp program length).
pub const MAX_FILTER_PREFIXES: usize = 8;

/// Longest prefix, in bytes, a filter holds.
///
/// The prefixes match a tracing target (a Rust module path, for one
/// `helios_kernel::exec::phases`) or a profile stack or metric name built
/// from `;`-separated segments. The longest of those in the tree is well
/// under 128 bytes, and one prefix is stored inline, so the filter is a
/// fixed value of about 1 KiB that a stream keeps for its life with no
/// heap of its own.
pub const MAX_FILTER_PREFIX_BYTES: usize = 128;

/// Why a prefix list does not fit a filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrefixSetError {
    /// More than [`MAX_FILTER_PREFIXES`] prefixes.
    TooManyPrefixes,
    /// A prefix longer than [`MAX_FILTER_PREFIX_BYTES`] bytes.
    PrefixTooLong,
}

/// The prefixes of one filter: a name matches when it starts with any of
/// them, and an empty set matches every name.
///
/// Holding the bounds in the type means a filter that exists fits them,
/// and a stream that keeps one owns no per-prefix allocation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrefixSet(ArrayVec<ArrayString<MAX_FILTER_PREFIX_BYTES>, MAX_FILTER_PREFIXES>);

impl PrefixSet {
    /// Whether a list of `count` prefixes fits, decided from the count
    /// alone so a caller can refuse before it reads any prefix.
    pub const fn admit_count(count: usize) -> Result<(), PrefixSetError> {
        if count > MAX_FILTER_PREFIXES {
            return Err(PrefixSetError::TooManyPrefixes);
        }
        Ok(())
    }

    /// Whether a prefix of `bytes` bytes fits, decided from the length
    /// alone so a caller can refuse before it copies the prefix.
    pub const fn admit_prefix_len(bytes: usize) -> Result<(), PrefixSetError> {
        if bytes > MAX_FILTER_PREFIX_BYTES {
            return Err(PrefixSetError::PrefixTooLong);
        }
        Ok(())
    }

    /// Adds `prefix`, copying it only once it is known to fit.
    pub fn push(&mut self, prefix: &str) -> Result<(), PrefixSetError> {
        Self::admit_prefix_len(prefix.len())?;
        let prefix = ArrayString::from(prefix).map_err(|_| PrefixSetError::PrefixTooLong)?;
        self.0
            .try_push(prefix)
            .map_err(|_| PrefixSetError::TooManyPrefixes)
    }

    /// The set of `prefixes`, refused whole when they do not fit.
    pub fn from_prefixes<'a>(
        prefixes: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, PrefixSetError> {
        let mut set = Self::default();
        for prefix in prefixes {
            set.push(prefix)?;
        }
        Ok(set)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether `name` starts with one of the prefixes, or the set is
    /// empty and so restricts nothing.
    pub fn admits(&self, name: &str) -> bool {
        self.0.is_empty()
            || self
                .0
                .iter()
                .any(|prefix| name.starts_with(prefix.as_str()))
    }
}

#[derive(Clone, Debug)]
pub struct TraceFilter {
    pub min_level: Option<TraceLevel>,
    pub target_prefixes: PrefixSet,
}

#[derive(Clone, Debug)]
pub struct TraceEvent {
    /// The instance that wrote the event, or `None` for the kernel's own
    /// console. Fixed when the event is recorded and never changed after.
    pub origin: Option<InstanceId>,
    pub timestamp: u64,
    pub level: TraceLevel,
    pub target: String,
    pub fields: Vec<TraceField>,
}

#[derive(Clone, Debug)]
pub struct TraceField {
    pub key: String,
    pub value: TraceValue,
}

#[derive(Clone, Debug)]
pub enum TraceValue {
    Boolean(bool),
    Signed64(i64),
    Unsigned64(u64),
    Float64(f64),
    Text(String),
    Blob(Vec<u8>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraceLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatsSample {
    pub timestamp: u64,
    pub uptime: u64,
    /// Wall time in nanoseconds since the Unix epoch, as the real-time
    /// clock the kernel read at boot places it. Zero on a machine that
    /// carried no such clock, where wall time is uptime.
    pub wall_clock: u64,
    pub configured_processors: u32,
    pub online_processors: u32,
    /// The ISA level guest code is compiled for, without the extras its
    /// name appends (`x86-64-v4` for `x86-64-v4+vbmi+bitalg`).
    pub guest_isa_level: &'static str,
    /// The features guest code uses beyond `guest_isa_level`.
    pub guest_isa_extras: Vec<&'static str>,
    /// The block device the kernel owns, absent on a machine that gave
    /// it none.
    pub block: Option<crate::BlockStats>,
    /// The platform's I/O translation unit, absent on a machine whose
    /// devices are not confined by one.
    pub iommu: Option<crate::IommuStats>,
    /// The memory balloon the host resizes this guest through, absent on
    /// a machine that gave the kernel none.
    pub balloon: Option<crate::memory::BalloonStats>,
    /// What the kernel has swapped out and what bringing it back costs,
    /// absent on a machine that runs without swap.
    pub swap: Option<crate::memory::SwapStats>,
    /// How the host share's 9p caches have served the kernel, absent on
    /// a machine with no host share behind a 9p session.
    pub host_share: Option<crate::HostFsCacheStats>,
    /// How the interface's work is spread across processors, absent on a
    /// machine that gave the kernel no network device.
    pub network: Option<crate::NetworkStats>,
    /// The hardware the kernel does not drive itself and has offered to
    /// user-mode drivers. Empty on a machine that described none.
    pub devices: Vec<crate::GrantedDeviceSnapshot>,
    /// The input devices the kernel owns and drains. Empty on a machine
    /// that gave it none.
    pub inputs: Vec<crate::InputDeviceSnapshot>,
    /// The playback streams the kernel owns and pumps. Empty on a
    /// machine that gave it no sound device.
    pub audio: Vec<crate::AudioStreamSnapshot>,
}

impl StatsSample {
    /// Drop the device, input and audio entries `authority` could not
    /// claim.
    ///
    /// A claim an instance may not make is answered as if the hardware
    /// were absent, so the inventory it can read has to agree: a listed
    /// device would answer the existence question the claim hides.
    pub(crate) fn restrict_to(&mut self, authority: &crate::ProcessAuthority) {
        let granted = authority.granted_devices();
        self.devices.retain(|device| granted.contains(&device.name));
        if authority.derive_input_claim_cap().is_err() {
            self.inputs.clear();
        }
        if authority.derive_audio_claim_cap().is_err() {
            self.audio.clear();
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProfileFilter {
    pub scope: Option<ProfileScope>,
    pub stack_prefixes: PrefixSet,
}

/// One folded stack and its weight.
///
/// The stack is shared so that copying a sample out of a history clones a
/// pointer and allocates nothing, which is what lets a read copy the
/// history under its lock and do the rest after releasing it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoldedProfileSample {
    pub scope: ProfileScope,
    pub stack: Arc<str>,
    pub weight: u64,
}

#[derive(Clone, Debug)]
pub struct PerfMetricFilter {
    pub name_prefixes: PrefixSet,
}

/// One perf metric. Its name is shared for the reason
/// [`FoldedProfileSample::stack`] is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PerfMetricSample {
    pub scope: ProfileScope,
    pub name: Arc<str>,
    pub count: u64,
    pub total_events: u64,
    pub total_nanos: u64,
    pub min_nanos: u64,
    pub max_nanos: u64,
    pub total_bytes: u64,
    pub total_reference_cycles: u64,
    pub total_cpu_cycles: u64,
    pub total_instructions_retired: u64,
}

impl PerfMetricSample {
    /// Folds `other`, a row of the same scope and name held for another
    /// origin, into this one, combining each field as a record into a
    /// held row does: every count and total is summed, saturating, the
    /// minimum is the smaller and the maximum the larger.
    fn merge(&mut self, other: &Self) {
        self.count = self.count.saturating_add(other.count);
        self.total_events = self.total_events.saturating_add(other.total_events);
        self.total_nanos = self.total_nanos.saturating_add(other.total_nanos);
        self.min_nanos = self.min_nanos.min(other.min_nanos);
        self.max_nanos = self.max_nanos.max(other.max_nanos);
        self.total_bytes = self.total_bytes.saturating_add(other.total_bytes);
        self.total_reference_cycles = self
            .total_reference_cycles
            .saturating_add(other.total_reference_cycles);
        self.total_cpu_cycles = self.total_cpu_cycles.saturating_add(other.total_cpu_cycles);
        self.total_instructions_retired = self
            .total_instructions_retired
            .saturating_add(other.total_instructions_retired);
    }
}

/// A held sample and the instance it is attributed to: `None` for the
/// kernel's own, `Some` for a user program's.
///
/// The origin is part of the key a history holds a sample under, so two
/// instances recording one name hold two rows. A read that does not care
/// which instance a row came from merges them; see [`ProfileHistory::rank`].
/// The public sample types carry no origin, so what a reader sees is the
/// same whichever way the rows were held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attributed<Sample> {
    pub origin: Option<InstanceId>,
    pub sample: Sample,
}

/// Ordered so a read can sort a snapshot by `(scope, name)` to merge the
/// rows of one name; the order itself carries no meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProfileScope {
    Kernel,
    User,
}

/// Which records a reader may see, by the instance each is attributed to
/// (`None` for the kernel's own).
///
/// A read is generic over its scope rather than branching on a value: the
/// scope is chosen once per call, from the caller's authority, and a
/// holder of the machine-wide right pays nothing for the check. Every
/// read applies [`Self::admits`] to its copy of the history after the
/// history's lock is released, so a scope never runs under a lock a
/// record also takes.
pub trait ObservationScope {
    /// Whether a record attributed to `origin` is visible in this scope.
    fn admits(&self, origin: Option<InstanceId>) -> bool;
}

/// Holding the right to observe the machine admits every record, the
/// kernel's own included.
impl ObservationScope for ObserveMachineCap {
    fn admits(&self, _origin: Option<InstanceId>) -> bool {
        true
    }
}

/// What a reader without the machine-wide right sees: the records its own
/// instance wrote, and never the kernel's or another instance's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnInstance(InstanceId);

impl OwnInstance {
    pub const fn new(instance: InstanceId) -> Self {
        Self(instance)
    }
}

impl ObservationScope for OwnInstance {
    fn admits(&self, origin: Option<InstanceId>) -> bool {
        origin == Some(self.0)
    }
}

/// Most entries one stream poll copies out of the trace history in one
/// lock hold, and so the size of the stack buffer it matches them in.
pub const TRACE_STREAM_BATCH: usize = 32;

/// Most lock holds one stream poll makes. With [`TRACE_STREAM_BATCH`] it
/// covers the default history in one poll.
const TRACE_STREAM_POLL_BATCHES: usize = 16;

/// The events of a [`TraceLog`], oldest first, each with the sequence
/// number it was recorded under.
///
/// Sequence numbers are contiguous: an event's index is its sequence
/// number minus the first retained one, which is what lets a stream resume
/// by position instead of by search. The events are shared so a reader
/// copies pointers out of the history; nothing here allocates once
/// constructed, because the queue is built at its capacity.
///
/// Concurrency: none of its own. It is only reachable through the lock of
/// the [`TraceLog`] that owns it.
#[derive(Debug)]
pub struct TraceHistory {
    next_seq: u64,
    capacity: usize,
    events: VecDeque<(u64, Arc<TraceEvent>)>,
}

/// The kernel's recent trace events and how they are read.
///
/// Concurrency contract: any processor records and any processor reads.
/// Every operation takes the one history lock for a bounded, allocation-free
/// copy and does everything else outside it.
/// - Recording builds each event's [`Arc`] before locking, so the lock
///   covers one `push_back`, and an evicted event is moved out under the
///   lock and dropped after it, so no allocation or free happens under the
///   lock.
/// - [`Self::recent`] reserves its snapshot before locking, clones at most
///   the history's capacity of pointers under the lock, and filters,
///   limits and lowers after it.
/// - [`Self::next_after`] keeps no buffer: each lock hold copies at most
///   [`TRACE_STREAM_BATCH`] pointers after the caller's cursor into a stack
///   array, and the filter runs after the lock is released, in at most
///   `TRACE_STREAM_POLL_BATCHES` holds.
///
/// Nothing a reader does under the lock can re-enter it: the filter runs
/// on pointers the reader already owns.
#[derive(Debug)]
pub struct TraceLog {
    /// The history's capacity, kept outside the lock so a reader can size
    /// its snapshot before taking it.
    capacity: usize,
    history: Mutex<TraceHistory>,
}

impl TraceLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            history: Mutex::new(TraceHistory::new(capacity)),
        }
    }

    /// Records every non-empty line of `text` as one event stamped
    /// `timestamp` and attributed to `origin` (`None` for the kernel's own
    /// console), each pushed as it is parsed.
    pub fn record_console_text(&self, timestamp: u64, origin: Option<InstanceId>, text: &str) {
        parse_console_text(timestamp, origin, text, |event| {
            let event = Arc::new(event);
            let evicted = self.history.lock().push(event);
            // The oldest event's last reference may free it; that happens
            // here, with the lock released.
            drop(evicted);
        });
    }

    /// The newest `limit` events `scope` admits and `filter` matches,
    /// oldest first.
    pub fn recent(
        &self,
        scope: &impl ObservationScope,
        filter: &TraceFilter,
        limit: u32,
    ) -> Vec<Arc<TraceEvent>> {
        let mut events = Vec::with_capacity(self.capacity);
        self.history.lock().copy_into(&mut events);
        events.retain(|event| scope.admits(event.origin) && matches_trace_filter(event, filter));
        let keep = limit as usize;
        if events.len() > keep {
            events.drain(..events.len() - keep);
        }
        events
    }

    /// The first event after `*cursor` that `scope` admits and `filter`
    /// matches, with `*cursor` moved onto it. When none is found, `*cursor`
    /// is left on the last event examined so the next call resumes after
    /// it. A cursor that has fallen behind the oldest retained event
    /// resumes at that event.
    pub fn next_after(
        &self,
        scope: &impl ObservationScope,
        cursor: &mut u64,
        filter: &TraceFilter,
    ) -> Option<Arc<TraceEvent>> {
        for _ in 0..TRACE_STREAM_POLL_BATCHES {
            let mut batch = ArrayVec::<(u64, Arc<TraceEvent>), TRACE_STREAM_BATCH>::new();
            self.history.lock().copy_after_into(*cursor, &mut batch);
            let exhausted = batch.len() < TRACE_STREAM_BATCH;
            for (seq, event) in batch {
                *cursor = seq;
                if scope.admits(event.origin) && matches_trace_filter(&event, filter) {
                    return Some(event);
                }
            }
            if exhausted {
                return None;
            }
        }
        None
    }
}

/// The profile and perf histories a subsystem records into.
///
/// It is the whole of what a subsystem needs in order to be profiled,
/// held apart from [`crate::RuntimeState`] so that a subsystem the
/// runtime state itself owns can record into it. The network service is
/// exactly that: the runtime state holds the service, and the service
/// records its own phases, so a service that reached the histories
/// through the runtime state would be a type that contains itself.
///
/// Concurrency contract: every operation is callable from any processor.
/// The enable flag is an atomic and each history is behind its own lock.
/// A record holds its lock for the length of one record. A read
/// ([`Self::folded`], [`Self::metrics`]) reserves its snapshot before
/// locking, clones at most the history's capacity of pointers under the
/// lock, and filters, merges the rows of one name across origins, sorts
/// and truncates after releasing it, so no read allocates, frees or
/// matches while a history lock is held. The merge sorts the snapshot in
/// place and folds adjacent rows; it builds no table.
#[derive(Clone)]
pub struct ProfileSink {
    inner: Arc<ProfileSinkInner>,
}

struct ProfileSinkInner {
    enabled: AtomicBool,
    /// The histories' capacities, kept outside their locks so a read can
    /// size its snapshot before taking one.
    profile_capacity: usize,
    perf_metric_capacity: usize,
    profiling: Mutex<ProfileHistory>,
    perf_metrics: Mutex<PerfMetricHistory>,
}

impl ProfileSink {
    pub fn new(profile_capacity: usize, perf_metric_capacity: usize) -> Self {
        Self {
            inner: Arc::new(ProfileSinkInner {
                enabled: AtomicBool::new(false),
                profile_capacity,
                perf_metric_capacity,
                profiling: Mutex::new(ProfileHistory::new(profile_capacity)),
                perf_metrics: Mutex::new(PerfMetricHistory::new(perf_metric_capacity)),
            }),
        }
    }

    /// The folded stacks `scope` admits and `filter` matches, heaviest
    /// first, at most `limit` of them (all when `limit` is zero), with the
    /// rows of one stack held for different instances merged into one.
    pub fn folded(
        &self,
        scope: &impl ObservationScope,
        filter: &ProfileFilter,
        limit: u32,
    ) -> Vec<FoldedProfileSample> {
        let mut samples = Vec::with_capacity(self.inner.profile_capacity);
        self.inner.profiling.lock().copy_into(&mut samples);
        ProfileHistory::rank(samples, scope, filter, limit)
    }

    /// The perf metrics `scope` admits and `filter` matches, slowest total
    /// first, at most `limit` of them (all when `limit` is zero), with the
    /// rows of one name held for different instances merged into one.
    pub fn metrics(
        &self,
        scope: &impl ObservationScope,
        filter: &PerfMetricFilter,
        limit: u32,
    ) -> Vec<PerfMetricSample> {
        let mut samples = Vec::with_capacity(self.inner.perf_metric_capacity);
        self.inner.perf_metrics.lock().copy_into(&mut samples);
        PerfMetricHistory::rank(samples, scope, filter, limit)
    }

    /// Whether anything is recording. Every record path tests this
    /// first, so a kernel that is not profiling pays one relaxed load.
    pub fn enabled(&self) -> bool {
        self.inner.enabled.load(Ordering::Acquire)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.inner.enabled.store(enabled, Ordering::Release);
    }

    pub fn profiling(&self) -> &Mutex<ProfileHistory> {
        &self.inner.profiling
    }

    pub fn perf_metrics(&self) -> &Mutex<PerfMetricHistory> {
        &self.inner.perf_metrics
    }

    /// Records one folded stack, named by a prefix and a suffix so the
    /// caller needs no allocation to name a phase.
    pub fn record_profile_stack_parts_nanos(
        &self,
        scope: ProfileScope,
        prefix: &str,
        suffix: &str,
        weight_nanos: u64,
    ) {
        if !self.enabled() {
            return;
        }
        self.inner
            .profiling
            .lock()
            .record_parts(scope, prefix, suffix, weight_nanos);
    }

    /// Records one perf sample under the same split name.
    pub fn record_perf_metric_parts(
        &self,
        scope: ProfileScope,
        prefix: &str,
        suffix: &str,
        sample: PerfSample,
    ) {
        if !self.enabled() {
            return;
        }
        self.inner
            .perf_metrics
            .lock()
            .record_parts(scope, prefix, suffix, sample);
    }
}

/// Folded stacks aggregated by `(scope, origin, stack)`, at most
/// `capacity` of them. The origin is the instance a user sample is
/// attributed to and `None` for a kernel one.
///
/// A record is O(1) in the number of stacks held: the stack is hashed
/// once and found through [`KeyedSamples`]. Only a new stack arriving
/// at a full history pays more, an amortised O(log capacity) to find
/// the lightest stack; see [`KeyedSamples`] for why that is the same
/// stack the linear scan this replaced would have found.
#[derive(Debug)]
pub struct ProfileHistory {
    samples: KeyedSamples<Attributed<FoldedProfileSample>>,
}

/// Perf metrics aggregated by `(scope, origin, name)`, at most `capacity`
/// of them, with the same O(1) record path as [`ProfileHistory`].
#[derive(Debug)]
pub struct PerfMetricHistory {
    samples: KeyedSamples<Attributed<PerfMetricSample>>,
}

/// A sample a [`KeyedSamples`] can hold: named, scoped, attributed, and
/// ranked by the number that decides which sample a full history gives
/// up.
trait KeyedSample {
    fn scope(&self) -> ProfileScope;

    fn origin(&self) -> Option<InstanceId>;

    fn name(&self) -> &str;

    /// The eviction rank. It only grows while the sample is held, which
    /// is what lets [`KeyedSamples`] keep stale lower bounds for it.
    fn rank(&self) -> u64;
}

impl KeyedSample for Attributed<FoldedProfileSample> {
    fn scope(&self) -> ProfileScope {
        self.sample.scope
    }

    fn origin(&self) -> Option<InstanceId> {
        self.origin
    }

    fn name(&self) -> &str {
        &self.sample.stack
    }

    fn rank(&self) -> u64 {
        self.sample.weight
    }
}

impl KeyedSample for Attributed<PerfMetricSample> {
    fn scope(&self) -> ProfileScope {
        self.sample.scope
    }

    fn origin(&self) -> Option<InstanceId> {
        self.origin
    }

    fn name(&self) -> &str {
        &self.sample.name
    }

    fn rank(&self) -> u64 {
        self.sample.count
    }
}

/// A bounded set of samples keyed by `(scope, origin, name)`.
///
/// `samples` is laid out exactly as the linear history it replaces laid
/// it out: appended on insert, `swap_remove`d on eviction. That order is
/// observable — it breaks ties in [`ProfileHistory::folded`] and decides
/// which of several equally light samples is evicted — so it is kept
/// rather than redefined. `index` maps a key to its position in
/// `samples`, which is what makes a record O(1).
///
/// Eviction takes the lightest sample, the first in `samples` order
/// among equals: the minimum of `(rank, position)`. `lightest` is a
/// lazy min-heap of `(rank, position, id)` lower bounds for that key.
/// A hit raises a sample's rank without touching the heap, so the entry
/// it leaves there is a lower bound rather than the value; a heap top
/// whose rank is stale is re-pushed with the current one, and a top
/// whose position no longer holds its `id` (the sample was evicted, or
/// moved by a `swap_remove`) is dropped. A move pushes a fresh entry for
/// the moved sample's new position, so every held sample always has an
/// entry no greater than its true key, and the first fresh top is the
/// true minimum. The heap is rebuilt from `samples` when stale entries
/// outnumber held ones, which keeps it O(capacity) and the eviction
/// path amortised O(log capacity).
///
/// Concurrency: none of its own. Each history sits behind the lock of
/// the [`ProfileSink`] that owns it, held for one record or for one
/// pointer-cloning copy of the held samples.
#[derive(Debug)]
struct KeyedSamples<Sample> {
    capacity: usize,
    samples: Vec<Sample>,
    /// The identity of the sample at each position, parallel to
    /// `samples`, so a heap entry can tell whether it still describes
    /// the sample at its position.
    ids: Vec<u64>,
    next_id: u64,
    index: HashTable<usize>,
    hasher: DefaultHashBuilder,
    lightest: BinaryHeap<Reverse<(u64, usize, u64)>>,
}

impl<Sample> KeyedSamples<Sample>
where
    Sample: KeyedSample,
{
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            samples: Vec::with_capacity(capacity),
            ids: Vec::with_capacity(capacity),
            next_id: 0,
            index: HashTable::with_capacity(capacity),
            hasher: DefaultHashBuilder::default(),
            lightest: BinaryHeap::with_capacity(capacity),
        }
    }

    fn clear(&mut self) {
        self.samples.clear();
        self.ids.clear();
        self.index.clear();
        self.lightest.clear();
    }

    fn is_full(&self) -> bool {
        self.samples.len() == self.capacity
    }

    fn iter(&self) -> impl Iterator<Item = &Sample> {
        self.samples.iter()
    }

    fn hash(&self, scope: ProfileScope, origin: Option<InstanceId>, name: &str) -> u64 {
        self.hasher.hash_one((scope, origin, name))
    }

    /// The held sample named `name` in `scope` and attributed to `origin`.
    fn get_mut(
        &mut self,
        scope: ProfileScope,
        origin: Option<InstanceId>,
        name: &str,
    ) -> Option<&mut Sample> {
        let hash = self.hash(scope, origin, name);
        let samples = &self.samples;
        let position = *self.index.find(hash, |&position| {
            let sample = &samples[position];
            sample.scope() == scope && sample.origin() == origin && sample.name() == name
        })?;
        Some(&mut self.samples[position])
    }

    /// The position of the lightest held sample: least rank, first in
    /// `samples` order among equals. `None` when nothing is held.
    fn lightest(&mut self) -> Option<usize> {
        loop {
            let Reverse((rank, position, id)) = *self.lightest.peek()?;
            if self.ids.get(position) != Some(&id) {
                // Evicted, or moved to a lower position that has its
                // own entry.
                self.lightest.pop();
                continue;
            }
            let current = self.samples[position].rank();
            if current == rank {
                return Some(position);
            }
            self.lightest.pop();
            self.lightest.push(Reverse((current, position, id)));
        }
    }

    /// Removes the sample at `position`, which [`Self::lightest`] has
    /// just named and therefore sits on top of the heap.
    fn evict(&mut self, position: usize) {
        let popped = self.lightest.pop();
        debug_assert_eq!(
            popped.map(|Reverse((_, at, _))| at),
            Some(position),
            "evicting a sample that is not the lightest"
        );
        let evicted = &self.samples[position];
        let hash = self.hash(evicted.scope(), evicted.origin(), evicted.name());
        match self.index.find_entry(hash, |&at| at == position) {
            Ok(entry) => {
                entry.remove();
            }
            Err(_) => panic!("profile sample at {position} is missing from its index"),
        }
        let last = self.samples.len() - 1;
        self.samples.swap_remove(position);
        self.ids.swap_remove(position);
        if position == last {
            return;
        }
        let moved = &self.samples[position];
        let hash = self.hash(moved.scope(), moved.origin(), moved.name());
        match self.index.find_mut(hash, |&at| at == last) {
            Some(at) => *at = position,
            None => panic!("profile sample at {last} is missing from its index"),
        }
        let rank = moved.rank();
        let id = self.ids[position];
        self.push_bound(rank, position, id);
    }

    /// Appends `sample`, which must not already be held, to a history
    /// that has room for it.
    fn insert(&mut self, sample: Sample) {
        assert!(!self.is_full(), "profile history inserted past capacity");
        let position = self.samples.len();
        let id = self.next_id;
        self.next_id += 1;
        let hash = self.hash(sample.scope(), sample.origin(), sample.name());
        let rank = sample.rank();
        self.samples.push(sample);
        self.ids.push(id);
        let Self {
            index,
            samples,
            hasher,
            ..
        } = self;
        index.insert_unique(hash, position, |&at| {
            let held = &samples[at];
            hasher.hash_one((held.scope(), held.origin(), held.name()))
        });
        self.push_bound(rank, position, id);
    }

    fn push_bound(&mut self, rank: u64, position: usize, id: u64) {
        self.lightest.push(Reverse((rank, position, id)));
        if self.lightest.len() > self.capacity.saturating_mul(2) {
            self.lightest = self
                .samples
                .iter()
                .zip(self.ids.iter())
                .enumerate()
                .map(|(at, (held, &id))| Reverse((held.rank(), at, id)))
                .collect();
        }
    }
}

struct ProfileStackParts<'a> {
    prefix: &'a str,
    suffix: &'a str,
}

/// A stack or metric name as a record path receives it.
trait ProfileStackInput {
    /// Runs `with` over the whole name. A name given in parts is joined
    /// on the stack, so looking one up allocates nothing.
    fn with_name<R>(&self, with: impl FnOnce(&str) -> R) -> R;

    /// The name as the shared string a held sample keeps.
    fn into_shared(self) -> Arc<str>;
}

impl ProfileStackInput for String {
    fn with_name<R>(&self, with: impl FnOnce(&str) -> R) -> R {
        with(self)
    }

    fn into_shared(self) -> Arc<str> {
        Arc::from(self)
    }
}

impl ProfileStackInput for &str {
    fn with_name<R>(&self, with: impl FnOnce(&str) -> R) -> R {
        with(self)
    }

    fn into_shared(self) -> Arc<str> {
        Arc::from(self)
    }
}

impl ProfileStackParts<'_> {
    fn joined(&self) -> ArrayString<OBSERVER_TEXT_STACK_BYTES> {
        let len = self.prefix.len().saturating_add(self.suffix.len());
        assert!(
            len <= OBSERVER_TEXT_STACK_BYTES,
            "profile stack exceeds observer stack buffer capacity"
        );
        let mut stack = ArrayString::<OBSERVER_TEXT_STACK_BYTES>::new();
        stack.push_str(self.prefix);
        stack.push_str(self.suffix);
        stack
    }
}

impl ProfileStackInput for ProfileStackParts<'_> {
    fn with_name<R>(&self, with: impl FnOnce(&str) -> R) -> R {
        with(self.joined().as_str())
    }

    fn into_shared(self) -> Arc<str> {
        Arc::from(self.joined().as_str())
    }
}

impl TraceHistory {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity != 0, "trace history capacity must be non-zero");
        Self {
            next_seq: 1,
            capacity,
            events: VecDeque::with_capacity(capacity),
        }
    }

    /// Appends `event`, handing back the oldest event when the history was
    /// full so the caller can drop it after releasing the lock.
    #[must_use = "an evicted event is dropped by the caller, outside the history's lock"]
    pub fn push(&mut self, event: Arc<TraceEvent>) -> Option<Arc<TraceEvent>> {
        let evicted = (self.events.len() == self.capacity).then(|| {
            self.events
                .pop_front()
                .expect("a full trace history holds an event")
                .1
        });
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        self.events.push_back((seq, event));
        evicted
    }

    /// Appends a pointer to every held event to `out`, oldest first.
    /// `out` must have room for them, so nothing allocates.
    pub fn copy_into(&self, out: &mut Vec<Arc<TraceEvent>>) {
        debug_assert!(
            out.capacity() - out.len() >= self.events.len(),
            "the snapshot was not reserved before the lock"
        );
        out.extend(self.events.iter().map(|(_, event)| event.clone()));
    }

    /// Appends the events after sequence number `cursor`, oldest first, to
    /// `out` until it is full. A cursor behind the oldest retained event
    /// starts at that event.
    pub fn copy_after_into<const N: usize>(
        &self,
        cursor: u64,
        out: &mut ArrayVec<(u64, Arc<TraceEvent>), N>,
    ) {
        let Some(&(first_seq, _)) = self.events.front() else {
            return;
        };
        // Sequence numbers are contiguous, so the first event after the
        // cursor sits at this offset from the front.
        let start = cursor.saturating_add(1).saturating_sub(first_seq);
        let start = usize::try_from(start).unwrap_or(usize::MAX);
        if start >= self.events.len() {
            return;
        }
        let room = out.remaining_capacity();
        out.extend(
            self.events
                .range(start..)
                .take(room)
                .map(|(seq, event)| (*seq, event.clone())),
        );
    }
}

impl ProfileHistory {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity != 0, "profile stack capacity must be non-zero");
        Self {
            samples: KeyedSamples::new(capacity),
        }
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Records a kernel stack: it is attributed to no instance.
    pub fn record(&mut self, scope: ProfileScope, stack: String, weight: u64) {
        self.record_input(scope, None, stack, weight);
    }

    /// Records a kernel stack, borrowed.
    pub fn record_str(&mut self, scope: ProfileScope, stack: &str, weight: u64) {
        self.record_input(scope, None, stack, weight);
    }

    /// Records a kernel stack named by a prefix and a suffix.
    pub fn record_parts(&mut self, scope: ProfileScope, prefix: &str, suffix: &str, weight: u64) {
        self.record_input(scope, None, ProfileStackParts { prefix, suffix }, weight);
    }

    /// Records the time `instance` spent running a user program, as the
    /// user-scope stack `prefix` + `suffix` attributed to it.
    pub fn record_user_parts(
        &mut self,
        instance: InstanceId,
        prefix: &str,
        suffix: &str,
        weight: u64,
    ) {
        self.record_input(
            ProfileScope::User,
            Some(instance),
            ProfileStackParts { prefix, suffix },
            weight,
        );
    }

    fn record_input<Stack>(
        &mut self,
        scope: ProfileScope,
        origin: Option<InstanceId>,
        stack: Stack,
        weight: u64,
    ) where
        Stack: ProfileStackInput,
    {
        if weight == 0 {
            return;
        }

        let held = stack.with_name(|name| match self.samples.get_mut(scope, origin, name) {
            Some(held) => {
                held.sample.weight = held.sample.weight.saturating_add(weight);
                true
            }
            None => false,
        });
        if held {
            return;
        }

        if self.samples.is_full() {
            let lightest = self
                .samples
                .lightest()
                .expect("a full profile history has a lightest entry");
            if self.samples.samples[lightest].sample.weight >= weight {
                return;
            }
            self.samples.evict(lightest);
        }

        self.samples.insert(Attributed {
            origin,
            sample: FoldedProfileSample {
                scope,
                stack: stack.into_shared(),
                weight,
            },
        });
    }

    /// Appends a copy of every held sample to `out`, in the history's
    /// own order. `out` must have room for them, so nothing allocates: a
    /// copy clones a pointer, not the stack.
    pub fn copy_into(&self, out: &mut Vec<Attributed<FoldedProfileSample>>) {
        debug_assert!(
            out.capacity() - out.len() >= self.samples.samples.len(),
            "the snapshot was not reserved before the lock"
        );
        out.extend(self.samples.iter().cloned());
    }

    /// Filters a copy taken by [`Self::copy_into`] to the rows `observation`
    /// admits and `filter` matches, merges the rows of one
    /// `(scope, stack)` held for different origins into one row of their
    /// summed weight, orders the result heaviest first (ties by stack,
    /// then scope) and keeps `limit` of it, all when `limit` is zero. The
    /// observation scope is applied before the merge, so a reader never
    /// sees a weight that includes an origin it may not read.
    ///
    /// The merge sorts the copy by `(scope, stack)` in place and folds
    /// adjacent equal rows in place, so it allocates nothing and builds no
    /// table. Only the returned list, which drops the origins, is a new
    /// allocation. It takes no lock, so it is for after the lock is
    /// released.
    pub fn rank(
        mut samples: Vec<Attributed<FoldedProfileSample>>,
        observation: &impl ObservationScope,
        filter: &ProfileFilter,
        limit: u32,
    ) -> Vec<FoldedProfileSample> {
        samples.retain(|row| {
            observation.admits(row.origin) && matches_profile_filter(&row.sample, filter)
        });
        samples.sort_unstable_by(|left, right| {
            left.sample
                .scope
                .cmp(&right.sample.scope)
                .then_with(|| (*left.sample.stack).cmp(&*right.sample.stack))
        });
        samples.dedup_by(|next, kept| {
            let same =
                next.sample.scope == kept.sample.scope && *next.sample.stack == *kept.sample.stack;
            if same {
                kept.sample.weight = kept.sample.weight.saturating_add(next.sample.weight);
            }
            same
        });
        // `(scope, stack)` is unique after the merge, so this order is
        // total and an unstable sort is exact.
        samples.sort_unstable_by(|left, right| {
            right
                .sample
                .weight
                .cmp(&left.sample.weight)
                .then_with(|| (*left.sample.stack).cmp(&*right.sample.stack))
                .then_with(|| left.sample.scope.cmp(&right.sample.scope))
        });
        let keep = limit as usize;
        if keep != 0 && samples.len() > keep {
            samples.truncate(keep);
        }
        samples.into_iter().map(|row| row.sample).collect()
    }
}

/// One measured interval: the work it did, how long it took, and what
/// the hardware counters saw while it ran.
///
/// The four numbers are always produced and consumed together, so they
/// travel as one value from the measuring site down into the history.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PerfSample {
    /// Discrete units of work, for rates the elapsed time cannot express
    /// on its own. Zero when the interval is only timed.
    pub events: u64,
    pub elapsed_nanos: u64,
    pub counters: HardwarePerfCounterDelta,
    pub bytes: u64,
}

impl PerfSample {
    /// A timed interval that counted no discrete events.
    pub fn timed(elapsed_nanos: u64, counters: HardwarePerfCounterDelta, bytes: u64) -> Self {
        Self {
            events: 0,
            elapsed_nanos,
            counters,
            bytes,
        }
    }
}

impl PerfMetricHistory {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity != 0, "perf metric capacity must be non-zero");
        Self {
            samples: KeyedSamples::new(capacity),
        }
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Records a kernel metric, which is attributed to no instance.
    pub fn record_str(&mut self, scope: ProfileScope, name: &str, sample: PerfSample) {
        self.record_input(scope, None, name, sample);
    }

    /// Records a kernel metric named by a prefix and a suffix.
    pub fn record_parts(
        &mut self,
        scope: ProfileScope,
        prefix: &str,
        suffix: &str,
        sample: PerfSample,
    ) {
        self.record_input(scope, None, ProfileStackParts { prefix, suffix }, sample);
    }

    fn record_input<Name>(
        &mut self,
        scope: ProfileScope,
        origin: Option<InstanceId>,
        name: Name,
        sample: PerfSample,
    ) where
        Name: ProfileStackInput,
    {
        let PerfSample {
            events,
            elapsed_nanos,
            counters,
            bytes,
        } = sample;
        if events == 0
            && elapsed_nanos == 0
            && bytes == 0
            && counters.reference_cycles == 0
            && counters.cpu_cycles == 0
            && counters.instructions_retired == 0
        {
            return;
        }

        let held = name.with_name(|key| match self.samples.get_mut(scope, origin, key) {
            Some(held) => {
                let sample = &mut held.sample;
                sample.count = sample.count.saturating_add(1);
                sample.total_events = sample.total_events.saturating_add(events);
                sample.total_nanos = sample.total_nanos.saturating_add(elapsed_nanos);
                sample.min_nanos = sample.min_nanos.min(elapsed_nanos);
                sample.max_nanos = sample.max_nanos.max(elapsed_nanos);
                sample.total_bytes = sample.total_bytes.saturating_add(bytes);
                sample.total_reference_cycles = sample
                    .total_reference_cycles
                    .saturating_add(counters.reference_cycles);
                sample.total_cpu_cycles =
                    sample.total_cpu_cycles.saturating_add(counters.cpu_cycles);
                sample.total_instructions_retired = sample
                    .total_instructions_retired
                    .saturating_add(counters.instructions_retired);
                true
            }
            None => false,
        });
        if held {
            return;
        }

        if self.samples.is_full() {
            let coldest = self
                .samples
                .lightest()
                .expect("a full perf metric history has a coldest entry");
            self.samples.evict(coldest);
        }

        self.samples.insert(Attributed {
            origin,
            sample: PerfMetricSample {
                scope,
                name: name.into_shared(),
                count: 1,
                total_events: events,
                total_nanos: elapsed_nanos,
                min_nanos: elapsed_nanos,
                max_nanos: elapsed_nanos,
                total_bytes: bytes,
                total_reference_cycles: counters.reference_cycles,
                total_cpu_cycles: counters.cpu_cycles,
                total_instructions_retired: counters.instructions_retired,
            },
        });
    }

    /// Appends a copy of every held sample to `out`, in the history's
    /// own order. `out` must have room for them, so nothing allocates: a
    /// copy clones a pointer, not the name.
    pub fn copy_into(&self, out: &mut Vec<Attributed<PerfMetricSample>>) {
        debug_assert!(
            out.capacity() - out.len() >= self.samples.samples.len(),
            "the snapshot was not reserved before the lock"
        );
        out.extend(self.samples.iter().cloned());
    }

    /// Filters a copy taken by [`Self::copy_into`] to the rows `observation`
    /// admits (before the merge, as [`ProfileHistory::rank`] does) and
    /// `filter` matches, merges the rows of one
    /// `(scope, name)` held for different origins into one row (see
    /// [`PerfMetricSample::merge`]: counts and totals are summed, the
    /// minimum and maximum latency are the least and the greatest), orders
    /// the result by total time, then count, then name, then scope, and
    /// keeps `limit` of it, all when `limit` is zero.
    ///
    /// The merge sorts the copy by `(scope, name)` in place and folds
    /// adjacent equal rows in place, so it allocates nothing and builds no
    /// table. Only the returned list, which drops the origins, is a new
    /// allocation. It takes no lock, so it is for after the lock is
    /// released.
    pub fn rank(
        mut samples: Vec<Attributed<PerfMetricSample>>,
        observation: &impl ObservationScope,
        filter: &PerfMetricFilter,
        limit: u32,
    ) -> Vec<PerfMetricSample> {
        samples.retain(|row| {
            observation.admits(row.origin) && matches_perf_metric_filter(&row.sample, filter)
        });
        samples.sort_unstable_by(|left, right| {
            left.sample
                .scope
                .cmp(&right.sample.scope)
                .then_with(|| (*left.sample.name).cmp(&*right.sample.name))
        });
        samples.dedup_by(|next, kept| {
            let same =
                next.sample.scope == kept.sample.scope && *next.sample.name == *kept.sample.name;
            if same {
                kept.sample.merge(&next.sample);
            }
            same
        });
        // `(scope, name)` is unique after the merge, so this order is
        // total and an unstable sort is exact.
        samples.sort_unstable_by(|left, right| {
            right
                .sample
                .total_nanos
                .cmp(&left.sample.total_nanos)
                .then_with(|| right.sample.count.cmp(&left.sample.count))
                .then_with(|| (*left.sample.name).cmp(&*right.sample.name))
                .then_with(|| left.sample.scope.cmp(&right.sample.scope))
        });
        let keep = limit as usize;
        if keep != 0 && samples.len() > keep {
            samples.truncate(keep);
        }
        samples.into_iter().map(|row| row.sample).collect()
    }
}

/// Hands `push` one event per non-empty line of `text`, each as soon as it
/// is parsed, so no list of them is built.
pub fn parse_console_text(
    timestamp: u64,
    origin: Option<InstanceId>,
    text: &str,
    mut push: impl FnMut(TraceEvent),
) {
    let stripped = strip_ansi(text);
    for raw_line in stripped.lines() {
        let line = raw_line.trim();
        if !line.is_empty() {
            push(parse_console_line(timestamp, origin, line));
        }
    }
}

pub fn matches_trace_filter(event: &TraceEvent, filter: &TraceFilter) -> bool {
    if let Some(min_level) = filter.min_level
        && level_priority(event.level) > level_priority(min_level)
    {
        return false;
    }

    filter.target_prefixes.admits(&event.target)
}

pub fn matches_profile_filter(sample: &FoldedProfileSample, filter: &ProfileFilter) -> bool {
    if filter.scope.is_some_and(|scope| sample.scope != scope) {
        return false;
    }

    filter.stack_prefixes.admits(&sample.stack)
}

pub fn matches_perf_metric_filter(sample: &PerfMetricSample, filter: &PerfMetricFilter) -> bool {
    filter.name_prefixes.admits(&sample.name)
}

fn parse_console_line(timestamp: u64, origin: Option<InstanceId>, line: &str) -> TraceEvent {
    if let Some((level, target, message)) = split_prefixed_line(line) {
        return TraceEvent {
            origin,
            timestamp,
            level,
            target,
            fields: vec![TraceField {
                key: "message".to_owned(),
                value: TraceValue::Text(message),
            }],
        };
    }

    TraceEvent {
        origin,
        timestamp,
        level: TraceLevel::Info,
        target: "console".to_owned(),
        fields: vec![TraceField {
            key: "message".to_owned(),
            value: TraceValue::Text(line.to_owned()),
        }],
    }
}

fn split_prefixed_line(line: &str) -> Option<(TraceLevel, String, String)> {
    let open = line.find('[')?;
    let close = line[open..].find(']')? + open;
    let level = parse_level(line[..open].trim())?;
    let target = line[open + 1..close].trim();
    let message = line[close + 1..].trim();
    if target.is_empty() || message.is_empty() {
        return None;
    }
    Some((level, target.to_owned(), message.to_owned()))
}

fn parse_level(level: &str) -> Option<TraceLevel> {
    Some(match level {
        "ERROR" => TraceLevel::Error,
        "WARN" => TraceLevel::Warn,
        "INFO" => TraceLevel::Info,
        "DEBUG" => TraceLevel::Debug,
        "TRACE" => TraceLevel::Trace,
        _ => return None,
    })
}

fn level_priority(level: TraceLevel) -> u8 {
    match level {
        TraceLevel::Error => 0,
        TraceLevel::Warn => 1,
        TraceLevel::Info => 2,
        TraceLevel::Debug => 3,
        TraceLevel::Trace => 4,
    }
}

fn strip_ansi(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == 0x1b {
            index += 1;
            if index < bytes.len() && bytes[index] == b'[' {
                index += 1;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if byte.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
        }

        output.push(bytes[index] as char);
        index += 1;
    }

    output
}

#[cfg(test)]
mod tests {
    use alloc::borrow::ToOwned;
    use alloc::string::{String, ToString};
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{
        FoldedProfileSample, MAX_FILTER_PREFIX_BYTES, MAX_FILTER_PREFIXES, ObservationScope,
        OwnInstance, PerfMetricFilter, PerfMetricHistory, PerfMetricSample, PerfSample, PrefixSet,
        PrefixSetError, ProfileFilter, ProfileHistory, ProfileScope, ProfileSink,
        ProfileStackParts, TRACE_STREAM_BATCH, TraceEvent, TraceField, TraceFilter, TraceLevel,
        TraceLog, TraceValue, matches_trace_filter, parse_console_text,
    };
    use crate::InstanceId;
    use crate::test_support::test_observe_machine_cap as machine;
    use helios_hal::cpu::HardwarePerfCounterDelta;
    use triomphe::Arc;

    fn prefixes<const N: usize>(list: [&str; N]) -> PrefixSet {
        PrefixSet::from_prefixes(list).expect("the test prefixes fit the filter bounds")
    }

    /// A read of one history alone, the copy and the ranking that
    /// [`super::ProfileSink::folded`] runs either side of its lock.
    fn folded(
        history: &ProfileHistory,
        filter: &ProfileFilter,
        limit: u32,
    ) -> Vec<FoldedProfileSample> {
        let mut samples = Vec::with_capacity(history.samples.samples.len());
        history.copy_into(&mut samples);
        ProfileHistory::rank(samples, &machine(), filter, limit)
    }

    /// As [`folded`], for a reader that sees only what `scope` admits.
    fn folded_in(
        history: &ProfileHistory,
        scope: &impl ObservationScope,
        filter: &ProfileFilter,
        limit: u32,
    ) -> Vec<FoldedProfileSample> {
        let mut samples = Vec::with_capacity(history.samples.samples.len());
        history.copy_into(&mut samples);
        ProfileHistory::rank(samples, scope, filter, limit)
    }

    /// As [`metrics`], for a reader that sees only what `scope` admits.
    fn metrics_in(
        history: &PerfMetricHistory,
        scope: &impl ObservationScope,
        filter: &PerfMetricFilter,
        limit: u32,
    ) -> Vec<PerfMetricSample> {
        let mut samples = Vec::with_capacity(history.samples.samples.len());
        history.copy_into(&mut samples);
        PerfMetricHistory::rank(samples, scope, filter, limit)
    }

    fn metrics(
        history: &PerfMetricHistory,
        filter: &PerfMetricFilter,
        limit: u32,
    ) -> Vec<PerfMetricSample> {
        let mut samples = Vec::with_capacity(history.samples.samples.len());
        history.copy_into(&mut samples);
        PerfMetricHistory::rank(samples, &machine(), filter, limit)
    }

    fn event(target: &str, message: &str) -> TraceEvent {
        TraceEvent {
            origin: None,
            timestamp: 0,
            level: TraceLevel::Info,
            target: target.to_owned(),
            fields: vec![TraceField {
                key: "message".to_owned(),
                value: TraceValue::Text(message.to_owned()),
            }],
        }
    }

    fn message(event: &TraceEvent) -> &str {
        match &event.fields[0].value {
            TraceValue::Text(text) => text,
            other => panic!("the test events carry text, not {other:?}"),
        }
    }

    /// A log holding `count` events whose messages are their numbers and
    /// whose targets alternate between `even` and `odd`.
    fn numbered_log(capacity: usize, count: usize) -> TraceLog {
        let log = TraceLog::new(capacity);
        for number in 0..count {
            let target = if number % 2 == 0 { "even" } else { "odd" };
            let mut history = log.history.lock();
            let evicted = history.push(Arc::new(event(target, &number.to_string())));
            drop(history);
            drop(evicted);
        }
        log
    }

    fn messages(events: &[Arc<TraceEvent>]) -> Vec<String> {
        events
            .iter()
            .map(|event| message(event).to_owned())
            .collect()
    }

    fn only(target: &str) -> TraceFilter {
        TraceFilter {
            min_level: None,
            target_prefixes: prefixes([target]),
        }
    }

    #[test]
    fn parses_kernel_console_prefixes() {
        let mut parsed = Vec::new();
        parse_console_text(
            42,
            None,
            "INFO [helios_kernel] Kernel initialized",
            |event| parsed.push(event),
        );
        let event = parsed
            .into_iter()
            .next()
            .expect("console line should produce one event");

        assert_eq!(event.timestamp, 42);
        assert_eq!(event.level, TraceLevel::Info);
        assert_eq!(event.target, "helios_kernel");
        assert_eq!(event.fields.len(), 1);
        assert_eq!(event.fields[0].key, "message");
        assert!(
            matches!(&event.fields[0].value, TraceValue::Text(text) if text == "Kernel initialized")
        );
    }

    #[test]
    fn trace_history_applies_filter_and_limit() {
        let history = TraceLog::new(8);
        history.record_console_text(
            1,
            None,
            "INFO [helios_kernel] one\nWARN [helios_shell] two\n",
        );

        let filter = TraceFilter {
            min_level: Some(TraceLevel::Warn),
            target_prefixes: prefixes(["helios_shell"]),
        };
        let events = history.recent(&machine(), &filter, 8);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, TraceLevel::Warn);
        assert_eq!(events[0].target, "helios_shell");
    }

    #[test]
    fn recent_returns_the_matching_subset_oldest_first() {
        let log = numbered_log(16, 10);

        let evens = log.recent(&machine(), &only("even"), 16);
        assert_eq!(messages(&evens), ["0", "2", "4", "6", "8"]);

        let newest_two = log.recent(&machine(), &only("odd"), 2);
        assert_eq!(messages(&newest_two), ["7", "9"]);

        let everything = log.recent(
            &machine(),
            &TraceFilter {
                min_level: None,
                target_prefixes: PrefixSet::default(),
            },
            16,
        );
        assert_eq!(everything.len(), 10);
        assert!(log.recent(&machine(), &only("even"), 0).is_empty());
    }

    #[test]
    fn eviction_drops_the_oldest_events_in_order() {
        let log = numbered_log(4, 10);

        let all = PrefixSet::default();
        let retained = log.recent(
            &machine(),
            &TraceFilter {
                min_level: None,
                target_prefixes: all,
            },
            16,
        );
        assert_eq!(messages(&retained), ["6", "7", "8", "9"]);
        assert_eq!(
            messages(&log.recent(&machine(), &only("even"), 16)),
            ["6", "8"]
        );
    }

    #[test]
    fn push_hands_back_the_evicted_event() {
        let log = TraceLog::new(2);
        let mut history = log.history.lock();
        assert!(history.push(Arc::new(event("t", "a"))).is_none());
        assert!(history.push(Arc::new(event("t", "b"))).is_none());
        let evicted = history
            .push(Arc::new(event("t", "c")))
            .expect("a full history gives up its oldest event");
        assert_eq!(message(&evicted), "a");
    }

    /// Drains a log through the stream poll, returning the messages in
    /// delivery order.
    fn drain(log: &TraceLog, cursor: &mut u64, filter: &TraceFilter) -> Vec<String> {
        let mut delivered = Vec::new();
        while let Some(event) = log.next_after(&machine(), cursor, filter) {
            delivered.push(message(&event).to_owned());
        }
        delivered
    }

    #[test]
    fn stream_poll_returns_the_events_recent_returns() {
        let log = numbered_log(64, 50);
        let filter = only("odd");

        let mut cursor = 0;
        let streamed = drain(&log, &mut cursor, &filter);

        assert_eq!(streamed, messages(&log.recent(&machine(), &filter, 64)));
        assert_eq!(streamed.len(), 25);
        assert_eq!(cursor, 50, "the cursor ends on the last event examined");
        assert!(log.next_after(&machine(), &mut cursor, &filter).is_none());
    }

    #[test]
    fn stream_batches_do_not_skip_or_repeat_events() {
        // More events than one batch holds, and a filter that drops
        // whole batches, so the poll crosses batch boundaries both while
        // delivering and while skipping.
        let count = TRACE_STREAM_BATCH * 5 + 7;
        let log = numbered_log(256, count);
        let everything = TraceFilter {
            min_level: None,
            target_prefixes: PrefixSet::default(),
        };

        let mut cursor = 0;
        let streamed = drain(&log, &mut cursor, &everything);
        let expected = (0..count)
            .map(|number| number.to_string())
            .collect::<Vec<_>>();
        assert_eq!(streamed, expected);

        let log = TraceLog::new(256);
        for number in 0..count {
            // Whole batches of "skip" around a few "keep" events.
            let target = if number % 100 == 99 { "keep" } else { "skip" };
            let evicted = log
                .history
                .lock()
                .push(Arc::new(event(target, &number.to_string())));
            drop(evicted);
        }
        let mut cursor = 0;
        let kept = drain(&log, &mut cursor, &only("keep"));
        assert_eq!(kept, ["99"]);
    }

    #[test]
    fn stream_cursor_behind_the_history_resumes_at_the_oldest_event() {
        let log = numbered_log(4, 10);
        let everything = TraceFilter {
            min_level: None,
            target_prefixes: PrefixSet::default(),
        };

        // Sequence numbers start at 1, so events 6..=9 hold 7..=10 and a
        // cursor at 2 has fallen behind them.
        let mut cursor = 2;
        let streamed = drain(&log, &mut cursor, &everything);
        assert_eq!(streamed, ["6", "7", "8", "9"]);
        assert_eq!(cursor, 10);

        let mut fresh = 0;
        assert_eq!(drain(&log, &mut fresh, &everything), ["6", "7", "8", "9"]);
    }

    #[test]
    fn stream_sees_events_recorded_after_it_drained() {
        let log = numbered_log(8, 3);
        let everything = TraceFilter {
            min_level: None,
            target_prefixes: PrefixSet::default(),
        };
        let mut cursor = 0;
        assert_eq!(drain(&log, &mut cursor, &everything), ["0", "1", "2"]);

        log.record_console_text(0, None, "INFO [late] later\n");
        let late = log
            .next_after(&machine(), &mut cursor, &everything)
            .expect("an event recorded after the drain is delivered");
        assert_eq!(late.target, "late");
        assert!(
            log.next_after(&machine(), &mut cursor, &everything)
                .is_none()
        );
    }

    #[test]
    fn filter_accepts_matching_event() {
        let event = TraceEvent {
            origin: None,
            timestamp: 7,
            level: TraceLevel::Error,
            target: "helios_kernel".to_owned(),
            fields: vec![TraceField {
                key: "message".to_owned(),
                value: TraceValue::Text("panic".to_owned()),
            }],
        };
        let filter = TraceFilter {
            min_level: Some(TraceLevel::Warn),
            target_prefixes: prefixes(["helios_"]),
        };

        assert!(matches_trace_filter(&event, &filter));
    }

    #[test]
    fn profile_history_aggregates_borrowed_stack_names() {
        let mut history = ProfileHistory::new(4);
        history.record_str(ProfileScope::Kernel, "kernel;executor;processor-0", 5);
        history.record_str(ProfileScope::Kernel, "kernel;executor;processor-0", 7);

        let samples = folded(
            &history,
            &ProfileFilter {
                scope: Some(ProfileScope::Kernel),
                stack_prefixes: PrefixSet::default(),
            },
            4,
        );

        assert_eq!(samples.len(), 1);
        assert_eq!(&*samples[0].stack, "kernel;executor;processor-0");
        assert_eq!(samples[0].weight, 12);
    }

    #[test]
    fn sink_reads_filter_sort_and_limit_after_the_copy() {
        let sink = ProfileSink::new(4, 4);
        sink.set_enabled(true);
        sink.record_profile_stack_parts_nanos(ProfileScope::Kernel, "kernel;", "a", 1);
        sink.record_profile_stack_parts_nanos(ProfileScope::Kernel, "kernel;", "b", 9);
        sink.record_profile_stack_parts_nanos(ProfileScope::User, "user;", "c", 5);
        sink.record_perf_metric_parts(
            ProfileScope::Kernel,
            "kernel;",
            "m",
            PerfSample::timed(7, HardwarePerfCounterDelta::default(), 0),
        );

        let folded = sink.folded(
            &machine(),
            &ProfileFilter {
                scope: None,
                stack_prefixes: prefixes(["kernel;"]),
            },
            1,
        );
        assert_eq!(folded.len(), 1);
        assert_eq!(&*folded[0].stack, "kernel;b");

        let metrics = sink.metrics(
            &machine(),
            &PerfMetricFilter {
                name_prefixes: PrefixSet::default(),
            },
            0,
        );
        assert_eq!(metrics.len(), 1);
        assert_eq!(&*metrics[0].name, "kernel;m");
    }

    #[test]
    fn profile_history_aggregates_stack_parts() {
        let mut history = ProfileHistory::new(4);
        history.record_parts(ProfileScope::User, "user;", "dash", 3);
        history.record_parts(ProfileScope::User, "user;", "dash", 9);

        let samples = folded(
            &history,
            &ProfileFilter {
                scope: Some(ProfileScope::User),
                stack_prefixes: prefixes(["user;"]),
            },
            4,
        );

        assert_eq!(samples.len(), 1);
        assert_eq!(&*samples[0].stack, "user;dash");
        assert_eq!(samples[0].weight, 12);
    }

    /// The linear history `ProfileHistory` replaced, kept verbatim as
    /// the reference its folded output has to match.
    struct LinearProfileHistory {
        capacity: usize,
        samples: Vec<FoldedProfileSample>,
    }

    impl LinearProfileHistory {
        fn record(&mut self, scope: ProfileScope, stack: &str, weight: u64) {
            if weight == 0 {
                return;
            }
            if let Some(sample) = self
                .samples
                .iter_mut()
                .find(|sample| sample.scope == scope && &*sample.stack == stack)
            {
                sample.weight = sample.weight.saturating_add(weight);
                return;
            }
            if self.samples.len() == self.capacity {
                let lightest_index = self
                    .samples
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, sample)| sample.weight)
                    .map(|(index, _)| index)
                    .expect("non-empty profile samples should have a lightest entry");
                if self.samples[lightest_index].weight >= weight {
                    return;
                }
                self.samples.swap_remove(lightest_index);
            }
            self.samples.push(FoldedProfileSample {
                scope,
                stack: Arc::from(stack),
                weight,
            });
        }

        fn folded(&self) -> Vec<FoldedProfileSample> {
            let mut samples = self.samples.clone();
            // The read orders equal weights and stacks by scope, where the
            // history it replaced left them in insertion order.
            samples.sort_by(|left, right| {
                right
                    .weight
                    .cmp(&left.weight)
                    .then_with(|| (*left.stack).cmp(&*right.stack))
                    .then_with(|| left.scope.cmp(&right.scope))
            });
            samples
        }
    }

    /// The linear perf history `PerfMetricHistory` replaced, reduced to
    /// the fields its eviction and ordering depend on.
    struct LinearPerfHistory {
        capacity: usize,
        samples: Vec<(ProfileScope, String, u64, u64)>,
    }

    impl LinearPerfHistory {
        fn record(&mut self, scope: ProfileScope, name: &str, nanos: u64) {
            if let Some(sample) = self
                .samples
                .iter_mut()
                .find(|sample| sample.0 == scope && sample.1 == name)
            {
                sample.2 += 1;
                sample.3 += nanos;
                return;
            }
            if self.samples.len() == self.capacity {
                let coldest_index = self
                    .samples
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, sample)| sample.2)
                    .map(|(index, _)| index)
                    .expect("non-empty perf metric samples should have a coldest entry");
                self.samples.swap_remove(coldest_index);
            }
            self.samples.push((scope, name.to_owned(), 1, nanos));
        }

        fn recent(&self) -> Vec<(ProfileScope, String, u64, u64)> {
            let mut samples = self.samples.clone();
            samples.sort_by(|left, right| {
                right
                    .3
                    .cmp(&left.3)
                    .then_with(|| right.2.cmp(&left.2))
                    .then_with(|| left.1.cmp(&right.1))
                    .then_with(|| left.0.cmp(&right.0))
            });
            samples
        }
    }

    /// A deterministic stream of records over more stacks than the
    /// history holds, with small weights so that equal weights — the
    /// case the eviction order decides — come up constantly.
    fn record_stream(mut each: impl FnMut(ProfileScope, &str, u64)) {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        for _ in 0..20_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let scope = if state & 1 == 0 {
                ProfileScope::Kernel
            } else {
                ProfileScope::User
            };
            let stack = alloc::format!("kernel;stack-{}", (state >> 8) % 24);
            let weight = (state >> 32) % 6;
            each(scope, &stack, weight);
        }
    }

    #[test]
    fn profile_history_folds_like_the_linear_history_through_evictions() {
        for capacity in [1, 2, 3, 7, 16, 64] {
            let mut keyed = ProfileHistory::new(capacity);
            let mut linear = LinearProfileHistory {
                capacity,
                samples: Vec::new(),
            };
            let mut step = 0_u32;
            record_stream(|scope, stack, weight| {
                // Alternate the three input shapes the record path takes.
                match step % 3 {
                    0 => keyed.record(scope, stack.to_owned(), weight),
                    1 => keyed.record_str(scope, stack, weight),
                    _ => {
                        let (prefix, suffix) = stack.split_at(7);
                        keyed.record_parts(scope, prefix, suffix, weight);
                    }
                }
                linear.record(scope, stack, weight);
                step += 1;
                if step.is_multiple_of(97) {
                    let filter = ProfileFilter {
                        scope: None,
                        stack_prefixes: PrefixSet::default(),
                    };
                    assert_eq!(
                        folded(&keyed, &filter, 0),
                        linear.folded(),
                        "capacity {capacity}, record {step}"
                    );
                }
            });
            let filter = ProfileFilter {
                scope: None,
                stack_prefixes: PrefixSet::default(),
            };
            assert_eq!(folded(&keyed, &filter, 0), linear.folded());
        }
    }

    #[test]
    fn profile_history_evicts_the_first_of_equally_light_stacks() {
        let mut history = ProfileHistory::new(3);
        history.record_str(ProfileScope::Kernel, "a", 2);
        history.record_str(ProfileScope::Kernel, "b", 1);
        history.record_str(ProfileScope::Kernel, "c", 1);
        // Full: `b` and `c` tie as lightest and `b` comes first.
        history.record_str(ProfileScope::Kernel, "d", 5);
        // Not heavier than the lightest (`c`, 1): dropped.
        history.record_str(ProfileScope::Kernel, "e", 1);
        let samples = folded(
            &history,
            &ProfileFilter {
                scope: None,
                stack_prefixes: PrefixSet::default(),
            },
            0,
        );
        let stacks = samples
            .iter()
            .map(|sample| (&*sample.stack, sample.weight))
            .collect::<Vec<_>>();
        assert_eq!(stacks, vec![("d", 5), ("a", 2), ("c", 1)]);
    }

    #[test]
    fn perf_metric_history_matches_the_linear_history_through_evictions() {
        for capacity in [1, 2, 5, 16] {
            let mut keyed = PerfMetricHistory::new(capacity);
            let mut linear = LinearPerfHistory {
                capacity,
                samples: Vec::new(),
            };
            record_stream(|scope, name, nanos| {
                let nanos = nanos + 1;
                keyed.record_str(
                    scope,
                    name,
                    PerfSample::timed(nanos, HardwarePerfCounterDelta::default(), 0),
                );
                linear.record(scope, name, nanos);
            });
            let recent = metrics(
                &keyed,
                &PerfMetricFilter {
                    name_prefixes: PrefixSet::default(),
                },
                0,
            )
            .into_iter()
            .map(|sample| {
                (
                    sample.scope,
                    String::from(&*sample.name),
                    sample.count,
                    sample.total_nanos,
                )
            })
            .collect::<Vec<_>>();
            assert_eq!(recent, linear.recent(), "capacity {capacity}");
        }
    }

    #[test]
    fn perf_metric_history_aggregates_latency_bytes_and_hardware_counters() {
        let mut history = PerfMetricHistory::new(4);
        history.record_parts(
            ProfileScope::Kernel,
            "kernel;network;",
            "tx-submit",
            PerfSample {
                events: 0,
                elapsed_nanos: 10,
                counters: HardwarePerfCounterDelta {
                    reference_cycles: 30,
                    cpu_cycles: 40,
                    instructions_retired: 50,
                },
                bytes: 1500,
            },
        );
        history.record_parts(
            ProfileScope::Kernel,
            "kernel;network;",
            "tx-submit",
            PerfSample {
                events: 0,
                elapsed_nanos: 25,
                counters: HardwarePerfCounterDelta {
                    reference_cycles: 70,
                    cpu_cycles: 80,
                    instructions_retired: 90,
                },
                bytes: 9000,
            },
        );

        let samples = metrics(
            &history,
            &PerfMetricFilter {
                name_prefixes: prefixes(["kernel;network;"]),
            },
            4,
        );

        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].scope, ProfileScope::Kernel);
        assert_eq!(&*samples[0].name, "kernel;network;tx-submit");
        assert_eq!(samples[0].count, 2);
        assert_eq!(samples[0].total_events, 0);
        assert_eq!(samples[0].total_nanos, 35);
        assert_eq!(samples[0].min_nanos, 10);
        assert_eq!(samples[0].max_nanos, 25);
        assert_eq!(samples[0].total_bytes, 10_500);
        assert_eq!(samples[0].total_reference_cycles, 100);
        assert_eq!(samples[0].total_cpu_cycles, 120);
        assert_eq!(samples[0].total_instructions_retired, 140);
    }

    #[test]
    fn perf_metric_history_aggregates_events_without_latency() {
        let mut history = PerfMetricHistory::new(4);
        history.record_parts(
            ProfileScope::Kernel,
            "kernel;executor;",
            "local-runnable",
            PerfSample {
                events: 7,
                elapsed_nanos: 0,
                counters: HardwarePerfCounterDelta::default(),
                bytes: 0,
            },
        );
        history.record_parts(
            ProfileScope::Kernel,
            "kernel;executor;",
            "local-runnable",
            PerfSample {
                events: 11,
                elapsed_nanos: 0,
                counters: HardwarePerfCounterDelta::default(),
                bytes: 0,
            },
        );

        let samples = metrics(
            &history,
            &PerfMetricFilter {
                name_prefixes: prefixes(["kernel;executor;"]),
            },
            4,
        );

        assert_eq!(samples.len(), 1);
        assert_eq!(&*samples[0].name, "kernel;executor;local-runnable");
        assert_eq!(samples[0].count, 2);
        assert_eq!(samples[0].total_events, 18);
        assert_eq!(samples[0].total_nanos, 0);
    }

    #[test]
    fn prefix_set_refuses_a_ninth_prefix() {
        let at_the_bound = (0..MAX_FILTER_PREFIXES)
            .map(|_| "helios_")
            .collect::<Vec<_>>();
        let set = PrefixSet::from_prefixes(at_the_bound.iter().copied())
            .expect("a filter at the prefix bound is accepted");
        assert_eq!(set.len(), MAX_FILTER_PREFIXES);

        let over = (0..=MAX_FILTER_PREFIXES)
            .map(|_| "helios_")
            .collect::<Vec<_>>();
        assert_eq!(
            PrefixSet::from_prefixes(over.iter().copied()),
            Err(PrefixSetError::TooManyPrefixes)
        );
        assert_eq!(
            PrefixSet::admit_count(MAX_FILTER_PREFIXES + 1),
            Err(PrefixSetError::TooManyPrefixes)
        );
        assert_eq!(PrefixSet::admit_count(MAX_FILTER_PREFIXES), Ok(()));
    }

    #[test]
    fn prefix_set_refuses_a_prefix_longer_than_128_bytes() {
        let at_the_bound = "a".repeat(MAX_FILTER_PREFIX_BYTES);
        let set = PrefixSet::from_prefixes([at_the_bound.as_str()])
            .expect("a prefix at the byte bound is accepted");
        assert!(set.admits(&at_the_bound));

        let over = "a".repeat(MAX_FILTER_PREFIX_BYTES + 1);
        assert_eq!(
            PrefixSet::from_prefixes([over.as_str()]),
            Err(PrefixSetError::PrefixTooLong)
        );
        assert_eq!(
            PrefixSet::admit_prefix_len(MAX_FILTER_PREFIX_BYTES + 1),
            Err(PrefixSetError::PrefixTooLong)
        );
        assert_eq!(PrefixSet::admit_prefix_len(MAX_FILTER_PREFIX_BYTES), Ok(()));
    }

    #[test]
    fn prefix_set_counts_bytes_not_characters() {
        // CJK text: each of these characters is three UTF-8 bytes, so 43
        // of them are 129 bytes, and the bound is in bytes.
        let japanese = "計".repeat(MAX_FILTER_PREFIX_BYTES / 3 + 1);
        assert_eq!(
            PrefixSet::from_prefixes([japanese.as_str()]),
            Err(PrefixSetError::PrefixTooLong)
        );
        let korean = "한".repeat(MAX_FILTER_PREFIX_BYTES / 3);
        assert!(PrefixSet::from_prefixes([korean.as_str()]).is_ok());
    }

    fn instance(raw: u64) -> InstanceId {
        InstanceId::from_raw(raw)
    }

    fn any_stack() -> ProfileFilter {
        ProfileFilter {
            scope: None,
            stack_prefixes: PrefixSet::default(),
        }
    }

    fn any_metric() -> PerfMetricFilter {
        PerfMetricFilter {
            name_prefixes: PrefixSet::default(),
        }
    }

    #[test]
    fn two_instances_recording_one_user_stack_are_two_rows_read_as_one() {
        let mut history = ProfileHistory::new(8);
        let (first, second) = (instance(1), instance(2));
        history.record_user_parts(first, "user;", "dash", 3);
        history.record_user_parts(second, "user;", "dash", 4);
        history.record_user_parts(first, "user;", "dash", 5);

        // Held apart: one row per origin.
        assert_eq!(history.samples.samples.len(), 2);

        let samples = folded(&history, &any_stack(), 0);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].scope, ProfileScope::User);
        assert_eq!(&*samples[0].stack, "user;dash");
        assert_eq!(samples[0].weight, 12);
    }

    /// A metric merge sums `count`, `total_events`, `total_nanos`,
    /// `total_bytes` and the three hardware totals, and takes the least
    /// `min_nanos` and the greatest `max_nanos`: the same combination a
    /// record into a held row applies.
    #[test]
    fn two_instances_recording_one_metric_are_two_rows_read_as_one() {
        let mut history = PerfMetricHistory::new(8);
        let (first, second) = (instance(1), instance(2));
        let sample = |events, elapsed_nanos, bytes, cycles| PerfSample {
            events,
            elapsed_nanos,
            counters: HardwarePerfCounterDelta {
                reference_cycles: cycles,
                cpu_cycles: cycles * 2,
                instructions_retired: cycles * 3,
            },
            bytes,
        };
        history.record_input(
            ProfileScope::User,
            Some(first),
            "user;m",
            sample(1, 10, 100, 1),
        );
        history.record_input(
            ProfileScope::User,
            Some(first),
            "user;m",
            sample(2, 30, 200, 2),
        );
        history.record_input(
            ProfileScope::User,
            Some(second),
            "user;m",
            sample(4, 4, 400, 4),
        );

        assert_eq!(history.samples.samples.len(), 2);

        let merged = metrics(&history, &any_metric(), 0);
        assert_eq!(merged.len(), 1);
        let merged = &merged[0];
        assert_eq!(merged.scope, ProfileScope::User);
        assert_eq!(&*merged.name, "user;m");
        assert_eq!(merged.count, 3);
        assert_eq!(merged.total_events, 7);
        assert_eq!(merged.total_nanos, 44);
        assert_eq!(merged.min_nanos, 4);
        assert_eq!(merged.max_nanos, 30);
        assert_eq!(merged.total_bytes, 700);
        assert_eq!(merged.total_reference_cycles, 7);
        assert_eq!(merged.total_cpu_cycles, 14);
        assert_eq!(merged.total_instructions_retired, 21);
    }

    #[test]
    fn kernel_and_user_rows_of_one_name_do_not_merge() {
        let mut history = ProfileHistory::new(8);
        history.record_str(ProfileScope::Kernel, "shared", 5);
        history.record_user_parts(instance(1), "", "shared", 7);
        let samples = folded(&history, &any_stack(), 0);
        let rows = samples
            .iter()
            .map(|sample| (sample.scope, &*sample.stack, sample.weight))
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                (ProfileScope::User, "shared", 7),
                (ProfileScope::Kernel, "shared", 5)
            ]
        );

        let mut perf = PerfMetricHistory::new(8);
        let timed = PerfSample::timed(9, HardwarePerfCounterDelta::default(), 0);
        perf.record_str(ProfileScope::Kernel, "shared", timed);
        perf.record_input(ProfileScope::User, Some(instance(1)), "shared", timed);
        let samples = metrics(&perf, &any_metric(), 0);
        assert_eq!(samples.len(), 2);
        assert!(samples.iter().all(|sample| sample.count == 1));
    }

    #[test]
    fn merged_read_orders_and_limits_like_a_single_origin_history() {
        let mut split = ProfileHistory::new(64);
        let mut single = ProfileHistory::new(64);
        let mut split_perf = PerfMetricHistory::new(64);
        let mut single_perf = PerfMetricHistory::new(64);
        for step in 0..120_u64 {
            let stack = alloc::format!("stack-{}", step % 7);
            let weight = step % 5 + 1;
            // Three instances take the records in turn; the single-origin
            // history takes all of them.
            let origin = instance(step % 3);
            split.record_user_parts(origin, "user;", &stack, weight);
            single.record_input(
                ProfileScope::User,
                None,
                ProfileStackParts {
                    prefix: "user;",
                    suffix: &stack,
                },
                weight,
            );
            let timed = PerfSample::timed(weight, HardwarePerfCounterDelta::default(), weight);
            split_perf.record_input(ProfileScope::User, Some(origin), stack.as_str(), timed);
            single_perf.record_input(ProfileScope::User, None, stack.as_str(), timed);
        }
        assert!(split.samples.samples.len() > single.samples.samples.len());

        for limit in [0, 1, 3, 7] {
            assert_eq!(
                folded(&split, &any_stack(), limit),
                folded(&single, &any_stack(), limit),
                "folded, limit {limit}"
            );
            assert_eq!(
                metrics(&split_perf, &any_metric(), limit),
                metrics(&single_perf, &any_metric(), limit),
                "metrics, limit {limit}"
            );
        }
    }

    #[test]
    fn an_event_keeps_its_origin_through_recent_and_next_after() {
        let log = TraceLog::new(8);
        let writer = instance(7);
        log.record_console_text(1, Some(writer), "INFO [guest] hello\n");
        log.record_console_text(2, None, "INFO [kernel] boot\n");
        let everything = TraceFilter {
            min_level: None,
            target_prefixes: PrefixSet::default(),
        };

        let recent = log.recent(&machine(), &everything, 8);
        let origins = recent.iter().map(|event| event.origin).collect::<Vec<_>>();
        assert_eq!(origins, [Some(writer), None]);

        let mut cursor = 0;
        let first = log
            .next_after(&machine(), &mut cursor, &everything)
            .expect("the first event is delivered");
        assert_eq!(first.origin, Some(writer));
        let second = log
            .next_after(&machine(), &mut cursor, &everything)
            .expect("the second event is delivered");
        assert_eq!(second.origin, None);
    }

    fn everything() -> TraceFilter {
        TraceFilter {
            min_level: None,
            target_prefixes: PrefixSet::default(),
        }
    }

    /// A log holding one line from instance 1, one from instance 2, one
    /// from the kernel and a second from instance 1, in that order.
    fn attributed_log() -> TraceLog {
        let log = TraceLog::new(16);
        log.record_console_text(1, Some(instance(1)), "INFO [guest] one-a\n");
        log.record_console_text(2, Some(instance(2)), "INFO [guest] two-a\n");
        log.record_console_text(3, None, "INFO [kernel] boot\n");
        log.record_console_text(4, Some(instance(1)), "INFO [guest] one-b\n");
        log
    }

    fn origins(events: &[Arc<TraceEvent>]) -> Vec<Option<InstanceId>> {
        events.iter().map(|event| event.origin).collect()
    }

    #[test]
    fn own_instance_scope_reads_only_its_own_events() {
        let log = attributed_log();

        let own = log.recent(&OwnInstance::new(instance(1)), &everything(), 16);
        assert_eq!(origins(&own), [Some(instance(1)), Some(instance(1))]);
        assert_eq!(own[0].timestamp, 1);
        assert_eq!(own[1].timestamp, 4);

        // The limit counts only what the scope admits.
        let newest = log.recent(&OwnInstance::new(instance(1)), &everything(), 1);
        assert_eq!(newest.len(), 1);
        assert_eq!(newest[0].timestamp, 4);

        // An instance that wrote nothing sees nothing, and the kernel's
        // own records are never returned without the right.
        assert!(
            log.recent(&OwnInstance::new(instance(3)), &everything(), 16)
                .is_empty()
        );

        // A stream reads through the same scope and skips the rest.
        let scope = OwnInstance::new(instance(2));
        let mut cursor = 0;
        let delivered = log
            .next_after(&scope, &mut cursor, &everything())
            .expect("the instance's own event is delivered");
        assert_eq!(delivered.origin, Some(instance(2)));
        assert!(log.next_after(&scope, &mut cursor, &everything()).is_none());
        assert_eq!(cursor, 4, "the cursor passed the events it was not shown");

        let scope = OwnInstance::new(instance(3));
        let mut cursor = 0;
        assert!(log.next_after(&scope, &mut cursor, &everything()).is_none());
    }

    #[test]
    fn machine_scope_reads_every_event() {
        let log = attributed_log();

        let all = log.recent(&machine(), &everything(), 16);
        assert_eq!(
            origins(&all),
            [
                Some(instance(1)),
                Some(instance(2)),
                None,
                Some(instance(1))
            ]
        );

        let mut cursor = 0;
        let mut streamed = Vec::new();
        while let Some(event) = log.next_after(&machine(), &mut cursor, &everything()) {
            streamed.push(event.origin);
        }
        assert_eq!(streamed, origins(&all));
    }

    /// A scope that writes to the log it is reading from inside `admits`.
    /// If a read still held the log's lock while it applied the scope,
    /// the record would spin on it forever and the test would hang.
    struct RecordsWhileAdmitting<'log>(&'log TraceLog);

    impl ObservationScope for RecordsWhileAdmitting<'_> {
        fn admits(&self, _origin: Option<InstanceId>) -> bool {
            self.0
                .record_console_text(9, None, "INFO [reentrant] from the scope\n");
            true
        }
    }

    #[test]
    fn recent_matches_after_releasing_the_trace_lock() {
        let log = numbered_log(64, 3);
        let events = log.recent(&RecordsWhileAdmitting(&log), &everything(), 8);
        assert_eq!(events.len(), 3, "the read sees the events it copied");

        let mut cursor = 0;
        let event = log
            .next_after(&RecordsWhileAdmitting(&log), &mut cursor, &everything())
            .expect("the stream delivers the first event");
        assert_eq!(message(&event), "0");
    }

    #[test]
    fn own_instance_folded_returns_only_its_own_user_samples() {
        let mut history = ProfileHistory::new(16);
        let (first, second) = (instance(1), instance(2));
        history.record_str(ProfileScope::Kernel, "kernel;executor", 100);
        history.record_user_parts(first, "user;", "dash", 3);
        history.record_user_parts(second, "user;", "dash", 40);
        history.record_user_parts(second, "user;", "other", 5);
        history.record_user_parts(first, "user;", "dash", 4);

        // The first instance sees only its own, and the weight is its
        // own, not the sum over both instances.
        let own = folded_in(&history, &OwnInstance::new(first), &any_stack(), 0);
        assert_eq!(own.len(), 1);
        assert_eq!(own[0].scope, ProfileScope::User);
        assert_eq!(&*own[0].stack, "user;dash");
        assert_eq!(own[0].weight, 7);

        // No kernel row and nothing of the first instance's for a third.
        assert!(folded_in(&history, &OwnInstance::new(instance(3)), &any_stack(), 0).is_empty());

        // The machine scope sees every row, merged across instances.
        let all = folded_in(&history, &machine(), &any_stack(), 0);
        let rows = all
            .iter()
            .map(|row| (row.scope, &*row.stack, row.weight))
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                (ProfileScope::Kernel, "kernel;executor", 100),
                (ProfileScope::User, "user;dash", 47),
                (ProfileScope::User, "user;other", 5),
            ]
        );
    }

    #[test]
    fn own_instance_metrics_return_only_its_own_samples() {
        let mut history = PerfMetricHistory::new(16);
        let (first, second) = (instance(1), instance(2));
        let timed = |nanos| PerfSample::timed(nanos, HardwarePerfCounterDelta::default(), 0);
        history.record_str(ProfileScope::Kernel, "kernel;m", timed(9));
        history.record_input(ProfileScope::User, Some(first), "user;m", timed(2));
        history.record_input(ProfileScope::User, Some(second), "user;m", timed(30));

        let own = metrics_in(&history, &OwnInstance::new(first), &any_metric(), 0);
        assert_eq!(own.len(), 1);
        assert_eq!(&*own[0].name, "user;m");
        assert_eq!(own[0].total_nanos, 2);

        assert!(metrics_in(&history, &OwnInstance::new(instance(3)), &any_metric(), 0).is_empty());

        let all = metrics_in(&history, &machine(), &any_metric(), 0);
        let user = all
            .iter()
            .find(|sample| &*sample.name == "user;m")
            .expect("the machine scope sees the user metric");
        assert_eq!(user.total_nanos, 32);
        assert!(all.iter().any(|sample| &*sample.name == "kernel;m"));
    }

    #[test]
    fn an_empty_prefix_set_admits_every_name() {
        assert!(PrefixSet::default().admits("anything"));
        let set = prefixes(["kernel;", "user;"]);
        assert!(set.admits("kernel;executor"));
        assert!(set.admits("user;dash"));
        assert!(!set.admits("other;dash"));
    }
}
