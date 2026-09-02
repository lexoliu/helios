extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use arrayvec::ArrayString;
use helios_hal::cpu::HardwarePerfCounterDelta;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatsSample {
    pub timestamp: u64,
    pub uptime: u64,
    pub configured_processors: u32,
    pub online_processors: u32,
    /// The block device the kernel owns, absent on a machine that gave
    /// it none.
    pub block: Option<crate::BlockStats>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Debug)]
pub struct ProfileHistory {
    capacity: usize,
    samples: Vec<FoldedProfileSample>,
}

#[derive(Debug)]
pub struct PerfMetricHistory {
    capacity: usize,
    samples: Vec<PerfMetricSample>,
}

struct ProfileStackParts<'a> {
    prefix: &'a str,
    suffix: &'a str,
}

trait ProfileStackInput {
    fn matches(&self, stack: &str) -> bool;

    fn into_string(self) -> String;
}

impl ProfileStackInput for String {
    fn matches(&self, stack: &str) -> bool {
        stack == self
    }

    fn into_string(self) -> String {
        self
    }
}

impl ProfileStackInput for &str {
    fn matches(&self, stack: &str) -> bool {
        stack == *self
    }

    fn into_string(self) -> String {
        self.to_owned()
    }
}

impl ProfileStackInput for ProfileStackParts<'_> {
    fn matches(&self, stack: &str) -> bool {
        stack.len() == self.prefix.len().saturating_add(self.suffix.len())
            && stack.starts_with(self.prefix)
            && stack[self.prefix.len()..] == *self.suffix
    }

    fn into_string(self) -> String {
        let len = self.prefix.len().saturating_add(self.suffix.len());
        assert!(
            len <= OBSERVER_TEXT_STACK_BYTES,
            "profile stack exceeds observer stack buffer capacity"
        );
        let mut stack = ArrayString::<OBSERVER_TEXT_STACK_BYTES>::new();
        stack.push_str(self.prefix);
        stack.push_str(self.suffix);
        stack.as_str().to_owned()
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
            capacity,
            samples: Vec::with_capacity(capacity),
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

        if let Some(sample) = self
            .samples
            .iter_mut()
            .find(|sample| sample.scope == scope && stack.matches(&sample.stack))
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

impl PerfMetricHistory {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity != 0, "perf metric capacity must be non-zero");
        Self {
            capacity,
            samples: Vec::with_capacity(capacity),
        }
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }

    pub fn record_str(
        &mut self,
        scope: ProfileScope,
        name: &str,
        elapsed_nanos: u64,
        counters: HardwarePerfCounterDelta,
        bytes: u64,
    ) {
        self.record_input(scope, name, 0, elapsed_nanos, counters, bytes);
    }

    pub fn record_parts(
        &mut self,
        scope: ProfileScope,
        prefix: &str,
        suffix: &str,
        elapsed_nanos: u64,
        counters: HardwarePerfCounterDelta,
        bytes: u64,
    ) {
        self.record_parts_events(scope, prefix, suffix, 0, elapsed_nanos, counters, bytes);
    }

    pub fn record_parts_events(
        &mut self,
        scope: ProfileScope,
        prefix: &str,
        suffix: &str,
        events: u64,
        elapsed_nanos: u64,
        counters: HardwarePerfCounterDelta,
        bytes: u64,
    ) {
        self.record_input(
            scope,
            ProfileStackParts { prefix, suffix },
            events,
            elapsed_nanos,
            counters,
            bytes,
        );
    }

    fn record_input<Name>(
        &mut self,
        scope: ProfileScope,
        name: Name,
        events: u64,
        elapsed_nanos: u64,
        counters: HardwarePerfCounterDelta,
        bytes: u64,
    ) where
        Name: ProfileStackInput,
    {
        if events == 0
            && elapsed_nanos == 0
            && bytes == 0
            && counters.reference_cycles == 0
            && counters.cpu_cycles == 0
            && counters.instructions_retired == 0
        {
            return;
        }

        if let Some(sample) = self
            .samples
            .iter_mut()
            .find(|sample| sample.scope == scope && name.matches(&sample.name))
        {
            sample.count = sample.count.saturating_add(1);
            sample.total_events = sample.total_events.saturating_add(events);
            sample.total_nanos = sample.total_nanos.saturating_add(elapsed_nanos);
            sample.min_nanos = sample.min_nanos.min(elapsed_nanos);
            sample.max_nanos = sample.max_nanos.max(elapsed_nanos);
            sample.total_bytes = sample.total_bytes.saturating_add(bytes);
            sample.total_reference_cycles = sample
                .total_reference_cycles
                .saturating_add(counters.reference_cycles);
            sample.total_cpu_cycles = sample.total_cpu_cycles.saturating_add(counters.cpu_cycles);
            sample.total_instructions_retired = sample
                .total_instructions_retired
                .saturating_add(counters.instructions_retired);
            return;
        }

        if self.samples.len() == self.capacity {
            let coldest_index = self
                .samples
                .iter()
                .enumerate()
                .min_by_key(|(_, sample)| sample.count)
                .map(|(index, _)| index)
                .expect("non-empty perf metric samples should have a coldest entry");
            self.samples.swap_remove(coldest_index);
        }

        self.samples.push(PerfMetricSample {
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
    use alloc::vec;

    use super::{
        PerfMetricFilter, PerfMetricHistory, ProfileFilter, ProfileHistory, ProfileScope,
        TraceEvent, TraceField, TraceFilter, TraceHistory, TraceLevel, TraceValue,
        matches_trace_filter, parse_console_text,
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

    #[test]
    fn perf_metric_history_aggregates_latency_bytes_and_hardware_counters() {
        let mut history = PerfMetricHistory::new(4);
        history.record_parts(
            ProfileScope::Kernel,
            "kernel;network;",
            "tx-submit",
            10,
            HardwarePerfCounterDelta {
                reference_cycles: 30,
                cpu_cycles: 40,
                instructions_retired: 50,
            },
            1500,
        );
        history.record_parts(
            ProfileScope::Kernel,
            "kernel;network;",
            "tx-submit",
            25,
            HardwarePerfCounterDelta {
                reference_cycles: 70,
                cpu_cycles: 80,
                instructions_retired: 90,
            },
            9000,
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
        history.record_parts_events(
            ProfileScope::Kernel,
            "kernel;executor;",
            "local-runnable",
            7,
            0,
            HardwarePerfCounterDelta::default(),
            0,
        );
        history.record_parts_events(
            ProfileScope::Kernel,
            "kernel;executor;",
            "local-runnable",
            11,
            0,
            HardwarePerfCounterDelta::default(),
            0,
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
