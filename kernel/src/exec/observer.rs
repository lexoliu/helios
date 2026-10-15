extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::collections::{BinaryHeap, VecDeque};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use core::cmp::Reverse;
use core::hash::BuildHasher;
use core::sync::atomic::{AtomicBool, Ordering};

use arrayvec::ArrayString;
use hashbrown::{DefaultHashBuilder, HashTable};
use helios_hal::cpu::HardwarePerfCounterDelta;
use spin::Mutex;
use triomphe::Arc;

pub const DEFAULT_TRACE_HISTORY_CAPACITY: usize = 512;
pub const DEFAULT_PROFILE_STACK_CAPACITY: usize = 1024;
pub const DEFAULT_PERF_METRIC_CAPACITY: usize = 512;
const OBSERVER_TEXT_STACK_BYTES: usize = 256;

#[derive(Clone, Debug)]
pub struct TraceFilter {
    pub min_level: Option<TraceLevel>,
    pub target_prefixes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct TraceEvent {
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

#[derive(Clone, Debug)]
pub struct ProfileFilter {
    pub scope: Option<ProfileScope>,
    pub stack_prefixes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoldedProfileSample {
    pub scope: ProfileScope,
    pub stack: String,
    pub weight: u64,
}

#[derive(Clone, Debug)]
pub struct PerfMetricFilter {
    pub name_prefixes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PerfMetricSample {
    pub scope: ProfileScope,
    pub name: String,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProfileScope {
    Kernel,
    User,
}

#[derive(Debug)]
pub struct TraceHistory {
    next_seq: u64,
    capacity: usize,
    events: VecDeque<(u64, TraceEvent)>,
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
/// Every operation is callable from any processor: the enable flag is
/// an atomic and each history is behind its own lock, taken for the
/// length of one record.
#[derive(Clone)]
pub struct ProfileSink {
    inner: Arc<ProfileSinkInner>,
}

struct ProfileSinkInner {
    enabled: AtomicBool,
    profiling: Mutex<ProfileHistory>,
    perf_metrics: Mutex<PerfMetricHistory>,
}

impl ProfileSink {
    pub fn new(profile_capacity: usize, perf_metric_capacity: usize) -> Self {
        Self {
            inner: Arc::new(ProfileSinkInner {
                enabled: AtomicBool::new(false),
                profiling: Mutex::new(ProfileHistory::new(profile_capacity)),
                perf_metrics: Mutex::new(PerfMetricHistory::new(perf_metric_capacity)),
            }),
        }
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

/// Folded stacks aggregated by `(scope, stack)`, at most `capacity` of
/// them.
///
/// A record is O(1) in the number of stacks held: the stack is hashed
/// once and found through [`KeyedSamples`]. Only a new stack arriving
/// at a full history pays more, an amortised O(log capacity) to find
/// the lightest stack; see [`KeyedSamples`] for why that is the same
/// stack the linear scan this replaced would have found.
#[derive(Debug)]
pub struct ProfileHistory {
    samples: KeyedSamples<FoldedProfileSample>,
}

/// Perf metrics aggregated by `(scope, name)`, at most `capacity` of
/// them, with the same O(1) record path as [`ProfileHistory`].
#[derive(Debug)]
pub struct PerfMetricHistory {
    samples: KeyedSamples<PerfMetricSample>,
}

/// A sample a [`KeyedSamples`] can hold: named, scoped, and ranked by
/// the number that decides which sample a full history gives up.
trait KeyedSample {
    fn scope(&self) -> ProfileScope;

    fn name(&self) -> &str;

    /// The eviction rank. It only grows while the sample is held, which
    /// is what lets [`KeyedSamples`] keep stale lower bounds for it.
    fn rank(&self) -> u64;
}

impl KeyedSample for FoldedProfileSample {
    fn scope(&self) -> ProfileScope {
        self.scope
    }

    fn name(&self) -> &str {
        &self.stack
    }

    fn rank(&self) -> u64 {
        self.weight
    }
}

impl KeyedSample for PerfMetricSample {
    fn scope(&self) -> ProfileScope {
        self.scope
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn rank(&self) -> u64 {
        self.count
    }
}

/// A bounded set of samples keyed by `(scope, name)`.
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
/// the [`ProfileSink`] that owns it, held for one record.
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

    fn hash(&self, scope: ProfileScope, name: &str) -> u64 {
        self.hasher.hash_one((scope, name))
    }

    /// The held sample named `name` in `scope`.
    fn get_mut(&mut self, scope: ProfileScope, name: &str) -> Option<&mut Sample> {
        let hash = self.hash(scope, name);
        let samples = &self.samples;
        let position = *self.index.find(hash, |&position| {
            let sample = &samples[position];
            sample.scope() == scope && sample.name() == name
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
        let hash = self.hash(evicted.scope(), evicted.name());
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
        let hash = self.hash(moved.scope(), moved.name());
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
        let hash = self.hash(sample.scope(), sample.name());
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
            hasher.hash_one((held.scope(), held.name()))
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

    fn into_string(self) -> String;
}

impl ProfileStackInput for String {
    fn with_name<R>(&self, with: impl FnOnce(&str) -> R) -> R {
        with(self)
    }

    fn into_string(self) -> String {
        self
    }
}

impl ProfileStackInput for &str {
    fn with_name<R>(&self, with: impl FnOnce(&str) -> R) -> R {
        with(self)
    }

    fn into_string(self) -> String {
        self.to_owned()
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

    fn into_string(self) -> String {
        self.joined().as_str().to_owned()
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

    pub fn push(&mut self, event: TraceEvent) {
        if self.events.len() == self.capacity {
            let dropped = self.events.pop_front();
            assert!(dropped.is_some(), "trace history underflowed");
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        self.events.push_back((seq, event));
    }

    pub fn record_console_text(&mut self, timestamp: u64, text: &str) {
        for event in parse_console_text(timestamp, text) {
            self.push(event);
        }
    }

    pub fn recent(&self, filter: &TraceFilter, limit: u32) -> Vec<TraceEvent> {
        let mut events = self
            .events
            .iter()
            .map(|(_, event)| event)
            .filter(|event| matches_trace_filter(event, filter))
            .cloned()
            .collect::<Vec<_>>();
        let keep = limit as usize;
        if events.len() > keep {
            events.drain(..events.len() - keep);
        }
        events
    }

    pub fn next_after(&self, cursor: u64, filter: &TraceFilter) -> Option<(u64, TraceEvent)> {
        self.events
            .iter()
            .find(|(seq, event)| *seq > cursor && matches_trace_filter(event, filter))
            .map(|(seq, event)| (*seq, event.clone()))
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

    pub fn record(&mut self, scope: ProfileScope, stack: String, weight: u64) {
        self.record_input(scope, stack, weight);
    }

    pub fn record_str(&mut self, scope: ProfileScope, stack: &str, weight: u64) {
        self.record_input(scope, stack, weight);
    }

    pub fn record_parts(&mut self, scope: ProfileScope, prefix: &str, suffix: &str, weight: u64) {
        self.record_input(scope, ProfileStackParts { prefix, suffix }, weight);
    }

    fn record_input<Stack>(&mut self, scope: ProfileScope, stack: Stack, weight: u64)
    where
        Stack: ProfileStackInput,
    {
        if weight == 0 {
            return;
        }

        let held = stack.with_name(|name| match self.samples.get_mut(scope, name) {
            Some(sample) => {
                sample.weight = sample.weight.saturating_add(weight);
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
            if self.samples.samples[lightest].weight >= weight {
                return;
            }
            self.samples.evict(lightest);
        }

        self.samples.insert(FoldedProfileSample {
            scope,
            stack: stack.into_string(),
            weight,
        });
    }

    pub fn folded(
        &self,
        filter: &ProfileFilter,
        additional: impl IntoIterator<Item = FoldedProfileSample>,
        limit: u32,
    ) -> Vec<FoldedProfileSample> {
        let mut samples = self
            .samples
            .iter()
            .cloned()
            .chain(additional)
            .filter(|sample| matches_profile_filter(sample, filter))
            .collect::<Vec<_>>();
        samples.sort_by(|left, right| {
            right
                .weight
                .cmp(&left.weight)
                .then_with(|| left.stack.cmp(&right.stack))
        });
        let keep = limit as usize;
        if keep != 0 && samples.len() > keep {
            samples.truncate(keep);
        }
        samples
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

    pub fn record_str(&mut self, scope: ProfileScope, name: &str, sample: PerfSample) {
        self.record_input(scope, name, sample);
    }

    pub fn record_parts(
        &mut self,
        scope: ProfileScope,
        prefix: &str,
        suffix: &str,
        sample: PerfSample,
    ) {
        self.record_input(scope, ProfileStackParts { prefix, suffix }, sample);
    }

    fn record_input<Name>(&mut self, scope: ProfileScope, name: Name, sample: PerfSample)
    where
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

        let held = name.with_name(|key| match self.samples.get_mut(scope, key) {
            Some(sample) => {
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

        self.samples.insert(PerfMetricSample {
            scope,
            name: name.into_string(),
            count: 1,
            total_events: events,
            total_nanos: elapsed_nanos,
            min_nanos: elapsed_nanos,
            max_nanos: elapsed_nanos,
            total_bytes: bytes,
            total_reference_cycles: counters.reference_cycles,
            total_cpu_cycles: counters.cpu_cycles,
            total_instructions_retired: counters.instructions_retired,
        });
    }

    pub fn recent(&self, filter: &PerfMetricFilter, limit: u32) -> Vec<PerfMetricSample> {
        let mut samples = self
            .samples
            .iter()
            .filter(|sample| matches_perf_metric_filter(sample, filter))
            .cloned()
            .collect::<Vec<_>>();
        samples.sort_by(|left, right| {
            right
                .total_nanos
                .cmp(&left.total_nanos)
                .then_with(|| right.count.cmp(&left.count))
                .then_with(|| left.name.cmp(&right.name))
        });
        let keep = limit as usize;
        if keep != 0 && samples.len() > keep {
            samples.truncate(keep);
        }
        samples
    }
}

pub fn parse_console_text(timestamp: u64, text: &str) -> Vec<TraceEvent> {
    let stripped = strip_ansi(text);
    stripped
        .lines()
        .filter_map(|raw_line| {
            let line = raw_line.trim();
            (!line.is_empty()).then(|| parse_console_line(timestamp, line))
        })
        .collect()
}

pub fn matches_trace_filter(event: &TraceEvent, filter: &TraceFilter) -> bool {
    if let Some(min_level) = filter.min_level
        && level_priority(event.level) > level_priority(min_level)
    {
        return false;
    }

    if filter.target_prefixes.is_empty() {
        return true;
    }

    filter
        .target_prefixes
        .iter()
        .any(|prefix| event.target.starts_with(prefix))
}

pub fn matches_profile_filter(sample: &FoldedProfileSample, filter: &ProfileFilter) -> bool {
    if filter.scope.is_some_and(|scope| sample.scope != scope) {
        return false;
    }

    if filter.stack_prefixes.is_empty() {
        return true;
    }

    filter
        .stack_prefixes
        .iter()
        .any(|prefix| sample.stack.starts_with(prefix))
}

pub fn matches_perf_metric_filter(sample: &PerfMetricSample, filter: &PerfMetricFilter) -> bool {
    if filter.name_prefixes.is_empty() {
        return true;
    }

    filter
        .name_prefixes
        .iter()
        .any(|prefix| sample.name.starts_with(prefix))
}

fn parse_console_line(timestamp: u64, line: &str) -> TraceEvent {
    if let Some((level, target, message)) = split_prefixed_line(line) {
        return TraceEvent {
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
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{
        FoldedProfileSample, PerfMetricFilter, PerfMetricHistory, PerfSample, ProfileFilter,
        ProfileHistory, ProfileScope, TraceEvent, TraceField, TraceFilter, TraceHistory,
        TraceLevel, TraceValue, matches_trace_filter, parse_console_text,
    };
    use helios_hal::cpu::HardwarePerfCounterDelta;

    #[test]
    fn parses_kernel_console_prefixes() {
        let event = parse_console_text(42, "INFO [helios_kernel] Kernel initialized")
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
        let mut history = TraceHistory::new(8);
        history.record_console_text(1, "INFO [helios_kernel] one\nWARN [helios_shell] two\n");

        let filter = TraceFilter {
            min_level: Some(TraceLevel::Warn),
            target_prefixes: vec!["helios_shell".to_owned()],
        };
        let events = history.recent(&filter, 8);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, TraceLevel::Warn);
        assert_eq!(events[0].target, "helios_shell");
    }

    #[test]
    fn filter_accepts_matching_event() {
        let event = TraceEvent {
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
            target_prefixes: vec!["helios_".to_owned()],
        };

        assert!(matches_trace_filter(&event, &filter));
    }

    #[test]
    fn profile_history_aggregates_borrowed_stack_names() {
        let mut history = ProfileHistory::new(4);
        history.record_str(ProfileScope::Kernel, "kernel;executor;processor-0", 5);
        history.record_str(ProfileScope::Kernel, "kernel;executor;processor-0", 7);

        let samples = history.folded(
            &ProfileFilter {
                scope: Some(ProfileScope::Kernel),
                stack_prefixes: vec![],
            },
            [],
            4,
        );

        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].stack, "kernel;executor;processor-0");
        assert_eq!(samples[0].weight, 12);
    }

    #[test]
    fn profile_history_aggregates_stack_parts() {
        let mut history = ProfileHistory::new(4);
        history.record_parts(ProfileScope::User, "user;", "dash", 3);
        history.record_parts(ProfileScope::User, "user;", "dash", 9);

        let samples = history.folded(
            &ProfileFilter {
                scope: Some(ProfileScope::User),
                stack_prefixes: vec!["user;".to_owned()],
            },
            [],
            4,
        );

        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].stack, "user;dash");
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
                .find(|sample| sample.scope == scope && sample.stack == stack)
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
                stack: stack.to_owned(),
                weight,
            });
        }

        fn folded(&self) -> Vec<FoldedProfileSample> {
            let mut samples = self.samples.clone();
            samples.sort_by(|left, right| {
                right
                    .weight
                    .cmp(&left.weight)
                    .then_with(|| left.stack.cmp(&right.stack))
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
                        stack_prefixes: vec![],
                    };
                    assert_eq!(
                        keyed.folded(&filter, [], 0),
                        linear.folded(),
                        "capacity {capacity}, record {step}"
                    );
                }
            });
            let filter = ProfileFilter {
                scope: None,
                stack_prefixes: vec![],
            };
            assert_eq!(keyed.folded(&filter, [], 0), linear.folded());
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
        let samples = history.folded(
            &ProfileFilter {
                scope: None,
                stack_prefixes: vec![],
            },
            [],
            0,
        );
        let stacks = samples
            .iter()
            .map(|sample| (sample.stack.as_str(), sample.weight))
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
            let recent = keyed
                .recent(
                    &PerfMetricFilter {
                        name_prefixes: vec![],
                    },
                    0,
                )
                .into_iter()
                .map(|sample| (sample.scope, sample.name, sample.count, sample.total_nanos))
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

        let samples = history.recent(
            &PerfMetricFilter {
                name_prefixes: vec!["kernel;network;".to_owned()],
            },
            4,
        );

        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].scope, ProfileScope::Kernel);
        assert_eq!(samples[0].name, "kernel;network;tx-submit");
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

        let samples = history.recent(
            &PerfMetricFilter {
                name_prefixes: vec!["kernel;executor;".to_owned()],
            },
            4,
        );

        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].name, "kernel;executor;local-runnable");
        assert_eq!(samples[0].count, 2);
        assert_eq!(samples[0].total_events, 18);
        assert_eq!(samples[0].total_nanos, 0);
    }
}
