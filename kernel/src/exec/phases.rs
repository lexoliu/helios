//! Per-launch phase timing on the [`TARGET`] tracing target.
//!
//! A launch — an `exec`/`spawn` host call, or a guest's own
//! `proc_spawn*`/`proc_exec*` launch — records the kernel monotonic
//! timestamp of every phase boundary it crosses into a
//! [`LaunchTimeline`]: a fixed-capacity array of one atomic slot per
//! [`LaunchPhase`] that travels with the launch the way its identity
//! does. Nothing is emitted per phase; when the launch ends the
//! timeline emits one `DEBUG` event whose fields are each recorded
//! phase's offset in nanoseconds from `rpc-arrival`
//! (`source_read_ns=…`), so the serial write lands once, after the
//! guest ran, outside every measured interval. A phase the launch
//! never reached records nothing and its field is absent from the
//! line, which is what makes a cold launch's `deserialize_ns` absent
//! on the warm one.
//!
//! Ownership, as the module that crosses tasks must state it: the
//! task a launch call ran on records `rpc-arrival` through
//! `load-complete` and `reply`; the task the launch spawned records
//! `task-begin` through `completion`. The timeline crosses between
//! them inside the launch arguments — `ProgramLaunch` carries it
//! for `spawn` and `exec`, the guest exec-replacement value for a
//! `proc_exec*`. A spawned launch's line is emitted by the run task
//! (the handle [`Timeline::for_task`] hands it owns emission); an
//! `exec` launch's line is emitted by the calling task, which awaits
//! the run task and writes the reply — for `programs.exec` that is
//! the RPC handler, for `proc_exec*` it is the task being replaced.
//! Every slot is written exactly once by exactly one task, so the
//! slots are bare `AtomicU64`s and emit is an acquire read of each.
//!
//! Every launch that recorded `rpc-arrival` ends its line with a
//! terminal [`LaunchEnd`]: `completed`, `refused`, or `failed`, with
//! the `error_kind` (a [`ProgramExecErrorKind`] name) or `errno` the
//! exit carried. The caller-side [`Trace`] emits on drop, so each
//! early return is the terminal record; the run task's
//! [`TaskTrace::finish`] marks the outcome the run produced — a guest
//! trap is `failed` with its kind — and the `instance` field carries
//! whichever id the launch last registered, so a trapped launch still
//! names its instance.
//!
//! Off costs one `enabled` check at [`Timeline::begin`] and one
//! `Option` branch per boundary: the slots exist only when the target
//! was enabled at `rpc-arrival`, so no timestamp is read and no
//! timeline kept for a boot that never asked.
//!

use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU64, Ordering};

use helios_hal::cpu::Cpu;

use super::monotonic_nanos;
use crate::{ProgramExecError, ProgramExecErrorKind};

/// The `tracing` target the one-line launch report is emitted under.
///
/// It is the name a session enables through `tracing
/// --enable-target` or `vm --enable-target`, and the prefix a
/// `tracing --target-prefix` fetch filters on.
pub(crate) const TARGET: &str = "helios_kernel::exec::phases";

/// The session gate for [`TARGET`], registered in
/// [`crate::log::DIAGNOSTIC_TARGETS`]. A runtime flip needs a globally
/// reachable flag because the subscriber's concrete type is erased at
/// install — the linkage contract this is one half of is documented in
/// `kernel/src/log.rs`'s module doc.
pub(crate) static GATE: crate::log::DiagnosticTarget = crate::log::DiagnosticTarget::new(TARGET);

/// A slot the launch never recorded keeps this value; `emit` treats
/// it as "the launch ended before this phase" and omits the field.
const UNRECORDED: u64 = u64::MAX;

/// The `errno` slot's unset marker — a launch's `errno` field exists
/// only on an errno-shaped syscall exit.
const ERRNO_UNSET: i32 = i32::MIN;

/// One boundary a launch crosses, named for the work it completes.
///
/// Each variant owns one slot of a [`LaunchTimeline`]; the launch's
/// emitted line carries its offset from `rpc-arrival` under
/// [`offset_field`](Self::offset_field).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LaunchPhase {
    /// The `exec`/`spawn` host call — or a guest's own process
    /// launch syscall, which reaches the loader without passing
    /// through the `helios:system/programs` RPC — entered the kernel.
    /// It is the offset epoch: its own offset is always `0`.
    RpcArrival,
    /// The program's bytes were read out of its source.
    SourceRead,
    /// The artifact's trust was established — the bootfs trailer parse,
    /// the signature check for a signed artifact, or — on a raw-wasm
    /// source — the whole in-kernel compile+sign the trust step runs.
    ArtifactTrust,
    /// The deserialize cache answered. Its `hit` lands in the line's
    /// `cache_hit` field.
    CacheLookup,
    /// The `cwasm` payload finished deserializing. Recorded only on a
    /// cache miss, so `deserialize_ns` exists only on cold launches.
    Deserialize,
    /// The `InstancePre` cache answered or `linker.instantiate_pre`
    /// built one. Its `hit` lands in the line's `instantiate_pre_hit`
    /// field.
    InstantiatePre,
    /// `load_executable` entered: trust, caches and deserialize run
    /// between this and `load-complete`.
    LoadBegin,
    /// `load_executable` returned; everything a spawn needs exists.
    LoadComplete,
    /// The run task is live; the spawn/scheduling gap ends here.
    TaskBegin,
    /// A core module's shared memory was prepared. Core-module
    /// launches only.
    SharedMemory,
    /// The store and its filesystem snapshot are prepared.
    StorePrepare,
    /// `instance_pre.instantiate_async` (or the core `InstancePre`
    /// instantiate) returned — memory slot, data segments and imports
    /// resolved.
    Instantiate,
    /// The run function resolved and guest start is dispatching.
    Start,
    /// Guest code started running.
    GuestBegin,
    /// Guest code returned; the run task still owns teardown.
    GuestEnd,
    /// The core-module store was torn down. Core-module launches only.
    StoreTeardown,
    /// The run task finished — the guest's result is in hand.
    Completion,
    /// The host call's reply — or a guest launch syscall's result — is
    /// being written. `instance` is set when the call produced one.
    Reply,
}

impl LaunchPhase {
    /// How many slots a [`LaunchTimeline`] holds — the phase count.
    pub(crate) const LEN: usize = 18;

    /// Every phase, in launch order; [`LaunchTimeline`]'s slot array is
    /// indexed by this order.
    pub(crate) const ALL: [Self; Self::LEN] = [
        Self::RpcArrival,
        Self::SourceRead,
        Self::ArtifactTrust,
        Self::CacheLookup,
        Self::Deserialize,
        Self::InstantiatePre,
        Self::LoadBegin,
        Self::LoadComplete,
        Self::TaskBegin,
        Self::SharedMemory,
        Self::StorePrepare,
        Self::Instantiate,
        Self::Start,
        Self::GuestBegin,
        Self::GuestEnd,
        Self::StoreTeardown,
        Self::Completion,
        Self::Reply,
    ];

    /// The `phase` boundary name — retained because the boundary's
    /// identity is also the field name's base (`rpc-arrival` reads as
    /// `rpc_arrival_ns`).
    #[cfg(test)]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::RpcArrival => "rpc-arrival",
            Self::SourceRead => "source-read",
            Self::ArtifactTrust => "trust",
            Self::CacheLookup => "cache-lookup",
            Self::Deserialize => "deserialize",
            Self::InstantiatePre => "instantiate-pre",
            Self::LoadBegin => "load-begin",
            Self::LoadComplete => "load-complete",
            Self::TaskBegin => "task-begin",
            Self::SharedMemory => "shared-memory",
            Self::StorePrepare => "store-prepare",
            Self::Instantiate => "instantiate",
            Self::Start => "start",
            Self::GuestBegin => "guest-begin",
            Self::GuestEnd => "guest-end",
            Self::StoreTeardown => "store-teardown",
            Self::Completion => "completion",
            Self::Reply => "reply",
        }
    }

    /// The field the emitted line carries this phase's offset under.
    #[cfg(test)]
    fn offset_field(self) -> &'static str {
        match self {
            Self::RpcArrival => "rpc_arrival_ns",
            Self::SourceRead => "source_read_ns",
            Self::ArtifactTrust => "trust_ns",
            Self::CacheLookup => "cache_lookup_ns",
            Self::Deserialize => "deserialize_ns",
            Self::InstantiatePre => "instantiate_pre_ns",
            Self::LoadBegin => "load_begin_ns",
            Self::LoadComplete => "load_complete_ns",
            Self::TaskBegin => "task_begin_ns",
            Self::SharedMemory => "shared_memory_ns",
            Self::StorePrepare => "store_prepare_ns",
            Self::Instantiate => "instantiate_ns",
            Self::Start => "start_ns",
            Self::GuestBegin => "guest_begin_ns",
            Self::GuestEnd => "guest_end_ns",
            Self::StoreTeardown => "store_teardown_ns",
            Self::Completion => "completion_ns",
            Self::Reply => "reply_ns",
        }
    }

    /// This phase's slot index in [`LaunchTimeline`].
    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|phase| *phase == self)
            .expect("every phase is listed")
    }
}

/// How a launch's timeline ended — the `end` field of the emitted
/// line. Every exit of a launch that passed `rpc-arrival` records one:
/// the run task for the outcome it produced, the launch-call task for
/// an exit that never reached the run task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum LaunchEnd {
    /// The guest ran to its exit — any exit code completes a launch.
    Completed = 1,
    /// The launch was refused before it ran — denied authority, an
    /// unresolvable name, a malformed call, an unavailable service.
    Refused = 2,
    /// The launch was attempted and died — the source could not be
    /// read, the load or spawn failed, the guest trapped.
    Failed = 3,
}

impl LaunchEnd {
    /// The terminal a [`ProgramExecErrorKind`] exit maps to: a
    /// machinery failure is `failed`; a rejection of the call or the
    /// program itself is a refusal.
    pub(crate) fn of(kind: ProgramExecErrorKind) -> Self {
        match kind {
            ProgramExecErrorKind::OutOfMemory | ProgramExecErrorKind::Internal => Self::Failed,
            _ => Self::Refused,
        }
    }
}

/// One launch's recorded phase timestamps and the identity the emitted
/// line carries. Slots are atomic because a `spawn`'s `reply` lands on
/// the parent's task while the child's run task holds the timeline;
/// each slot is written once by its owning task and read once at emit.
pub(crate) struct LaunchTimeline {
    at_ns: [AtomicU64; LaunchPhase::LEN],
    source_bytes: AtomicU64,
    cache_hit: AtomicBool,
    instantiate_pre_hit: AtomicBool,
    instance: AtomicU64,
    /// `0` until an exit marks it; stores `end as u8`.
    end: AtomicU8,
    /// `0` until a `ProgramExecErrorKind`-carrying exit marks it;
    /// stores `kind.index() + 1`.
    error_kind: AtomicU8,
    /// [`ERRNO_UNSET`] until an errno-shaped syscall exit marks it.
    errno: AtomicI32,
    op: &'static str,
    program: String,
}

impl LaunchTimeline {
    fn new(op: &'static str, program: String) -> Self {
        Self {
            at_ns: [const { AtomicU64::new(UNRECORDED) }; LaunchPhase::LEN],
            source_bytes: AtomicU64::new(0),
            cache_hit: AtomicBool::new(false),
            instantiate_pre_hit: AtomicBool::new(false),
            instance: AtomicU64::new(0),
            end: AtomicU8::new(0),
            error_kind: AtomicU8::new(0),
            errno: AtomicI32::new(ERRNO_UNSET),
            op,
            program,
        }
    }

    fn stamp(&self, phase: LaunchPhase, at_ns: u64) {
        self.at_ns[phase.index()].store(at_ns, Ordering::Release);
    }

    /// The phase's offset from `rpc-arrival` — `None` when the launch
    /// ended before reaching it, which the emitted line renders as the
    /// field's absence (`Option<u64>` records nothing for `None`).
    fn offset(&self, phase: LaunchPhase) -> Option<u64> {
        let at = self.at_ns[phase.index()].load(Ordering::Acquire);
        let arrival = self.at_ns[LaunchPhase::RpcArrival.index()].load(Ordering::Acquire);
        (at != UNRECORDED && arrival != UNRECORDED).then(|| at - arrival)
    }

    /// How many phases the launch reached before the line emitted.
    fn recorded(&self) -> u64 {
        self.at_ns
            .iter()
            .filter(|slot| slot.load(Ordering::Acquire) != UNRECORDED)
            .count() as u64
    }

    /// The `end` field's text — the recorded terminal, or the honest
    /// reading of one nobody classified: a launch that ran to
    /// `completion` completed; anything else failed.
    fn end_name(&self) -> &'static str {
        match self.end.load(Ordering::Acquire) {
            end if end == LaunchEnd::Completed as u8 => "completed",
            end if end == LaunchEnd::Refused as u8 => "refused",
            end if end == LaunchEnd::Failed as u8 => "failed",
            _ if self
                .at_ns
                .get(LaunchPhase::Completion.index())
                .is_some_and(|slot| slot.load(Ordering::Acquire) != UNRECORDED) =>
            {
                "completed"
            }
            _ => "failed",
        }
    }

    /// The `error_kind` field's name — `None` when no
    /// `ProgramExecErrorKind`-carrying exit marked it.
    fn error_kind_name(&self) -> Option<&'static str> {
        let index = self.error_kind.load(Ordering::Acquire);
        (index != 0).then(|| ProgramExecErrorKind::ALL[index as usize - 1].field_name())
    }

    /// The `errno` field — `None` when no errno-shaped exit marked it.
    fn errno_value(&self) -> Option<i32> {
        let errno = self.errno.load(Ordering::Acquire);
        (errno != ERRNO_UNSET).then_some(errno)
    }

    /// The one line a launch emits — every recorded phase's offset
    /// from `rpc-arrival`, written once when the launch ends so the
    /// serial cost never lands inside a measured interval.
    fn emit<CpuImpl: Cpu>(&self, cpu: &CpuImpl) {
        tracing::debug!(
            target: TARGET,
            at_ns = monotonic_nanos(cpu),
            op = self.op,
            program = self.program.as_str(),
            instance = self.instance.load(Ordering::Acquire),
            source_bytes = self.source_bytes.load(Ordering::Acquire),
            cache_hit = self.cache_hit.load(Ordering::Acquire),
            instantiate_pre_hit = self.instantiate_pre_hit.load(Ordering::Acquire),
            phase_count = self.recorded(),
            end = self.end_name(),
            error_kind = self.error_kind_name(),
            errno = self.errno_value(),
            rpc_arrival_ns = self.offset(LaunchPhase::RpcArrival),
            source_read_ns = self.offset(LaunchPhase::SourceRead),
            trust_ns = self.offset(LaunchPhase::ArtifactTrust),
            cache_lookup_ns = self.offset(LaunchPhase::CacheLookup),
            deserialize_ns = self.offset(LaunchPhase::Deserialize),
            instantiate_pre_ns = self.offset(LaunchPhase::InstantiatePre),
            load_begin_ns = self.offset(LaunchPhase::LoadBegin),
            load_complete_ns = self.offset(LaunchPhase::LoadComplete),
            task_begin_ns = self.offset(LaunchPhase::TaskBegin),
            shared_memory_ns = self.offset(LaunchPhase::SharedMemory),
            store_prepare_ns = self.offset(LaunchPhase::StorePrepare),
            instantiate_ns = self.offset(LaunchPhase::Instantiate),
            start_ns = self.offset(LaunchPhase::Start),
            guest_begin_ns = self.offset(LaunchPhase::GuestBegin),
            guest_end_ns = self.offset(LaunchPhase::GuestEnd),
            store_teardown_ns = self.offset(LaunchPhase::StoreTeardown),
            completion_ns = self.offset(LaunchPhase::Completion),
            reply_ns = self.offset(LaunchPhase::Reply),
        );
    }
}

/// The handle a launch's phases are recorded through. It is an
/// `Option<Arc<LaunchTimeline>>` plus an emission flag: `None` for a
/// boot that never enabled the target, so `record` on a disabled
/// timeline is a branch and nothing more.
#[derive(Clone)]
pub(crate) struct Timeline {
    inner: Option<Arc<LaunchTimeline>>,
    /// Whether the run task this handle is handed to emits the line
    /// when its run ends. The launch call's side decides: `for_task`
    /// sets it for `spawn` and guest `proc_exec*` launches, whose lines
    /// the spawned (or replaced) task ends; `exec` keeps it `false`
    /// because the awaiting caller emits after `reply`.
    task_emits: bool,
}

impl Timeline {
    /// Begins a launch's timeline, recording `rpc-arrival` — but only
    /// when the target is enabled: off means no clock is read and no
    /// timeline allocated.
    pub(crate) fn begin<CpuImpl: Cpu>(cpu: &CpuImpl, op: &'static str, program: &str) -> Self {
        let inner = gated(|| Arc::new(LaunchTimeline::new(op, String::from(program))));
        if let Some(timeline) = &inner {
            timeline.stamp(LaunchPhase::RpcArrival, monotonic_nanos(cpu));
        }
        Self {
            inner,
            task_emits: false,
        }
    }

    /// A launch carrying no timeline — the value `ProgramLaunch` and
    /// friends default to.
    pub(crate) const fn disabled() -> Self {
        Self {
            inner: None,
            task_emits: false,
        }
    }

    /// The handle handed to the task a launch runs on: it emits the
    /// line when that task's run ends, completed or failed.
    pub(crate) fn for_task(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            task_emits: true,
        }
    }

    /// Records `phase`'s boundary timestamp — a no-op on a disabled
    /// timeline, so the clock read happens only when somebody asked.
    pub(crate) fn record<CpuImpl: Cpu>(&self, cpu: &CpuImpl, phase: LaunchPhase) {
        if let Some(timeline) = &self.inner {
            timeline.stamp(phase, monotonic_nanos(cpu));
        }
    }

    /// Records a cache boundary and its answer. `phase` is
    /// `CacheLookup` or `InstantiatePre`; the hit lands in the emitted
    /// line's `cache_hit`/`instantiate_pre_hit` field.
    pub(crate) fn record_hit<CpuImpl: Cpu>(&self, cpu: &CpuImpl, phase: LaunchPhase, hit: bool) {
        if let Some(timeline) = &self.inner {
            timeline.stamp(phase, monotonic_nanos(cpu));
            match phase {
                LaunchPhase::CacheLookup => timeline.cache_hit.store(hit, Ordering::Release),
                LaunchPhase::InstantiatePre => {
                    timeline.instantiate_pre_hit.store(hit, Ordering::Release);
                }
                _ => {}
            }
        }
    }

    /// Records the launched program's source size for the emitted
    /// line's `source_bytes` field.
    pub(crate) fn record_source_bytes(&self, bytes: usize) {
        if let Some(timeline) = &self.inner {
            timeline.source_bytes.store(bytes as u64, Ordering::Release);
        }
    }

    /// Records the instance the launch produced; the emitted line's
    /// `instance` is whichever id was set last.
    pub(crate) fn set_instance(&self, instance: crate::InstanceId) {
        if let Some(timeline) = &self.inner {
            timeline.instance.store(instance.raw(), Ordering::Release);
        }
    }

    /// Records how the launch ended — `end` on the emitted line. Set
    /// by whichever task knows the outcome: the run task for the
    /// outcome it produced, the launch-call task for an exit that
    /// never reached the run task.
    pub(crate) fn set_end(&self, end: LaunchEnd) {
        if let Some(timeline) = &self.inner {
            timeline.end.store(end as u8, Ordering::Release);
        }
    }

    /// The `error_kind` field on the emitted line — the
    /// [`ProgramExecErrorKind`] a refused or failed exit carried.
    pub(crate) fn set_error_kind(&self, kind: ProgramExecErrorKind) {
        if let Some(timeline) = &self.inner {
            timeline
                .error_kind
                .store(kind.index() as u8 + 1, Ordering::Release);
        }
    }

    /// The `errno` field on the emitted line — the syscall's errno on
    /// a refused or failed errno-shaped exit.
    pub(crate) fn set_errno(&self, errno: i32) {
        if let Some(timeline) = &self.inner {
            timeline.errno.store(errno, Ordering::Release);
        }
    }

    /// Emits the launch's line — only meaningful through the handle
    /// that owns emission (a `for_task` handle in the run task, or the
    /// caller-side [`Trace`]).
    pub(crate) fn emit<CpuImpl: Cpu>(&self, cpu: &CpuImpl) {
        if let Some(timeline) = &self.inner {
            timeline.emit(cpu);
        }
    }

    /// The run task's emission guard: the line emits when the guard
    /// drops — at the run's end or on an error exit — but only for a
    /// [`for_task`](Self::for_task) handle.
    pub(crate) fn task_guard<CpuImpl: Cpu>(&self, cpu: CpuImpl) -> TaskTrace<CpuImpl> {
        TaskTrace {
            cpu,
            timeline: self.clone(),
        }
    }
}

/// The run task's end of a [`Timeline`]: drops into the line's emit.
/// Held for the duration of the run so every exit — completion, guest
/// error, host error — emits what the launch reached, and
/// [`finish`](Self::finish) records the run's outcome on the shared
/// timeline so the emitting side (this task for `spawn`, the awaiting
/// caller for `exec`) reads it.
pub(crate) struct TaskTrace<CpuImpl: Cpu> {
    cpu: CpuImpl,
    timeline: Timeline,
}

impl<CpuImpl: Cpu> TaskTrace<CpuImpl> {
    /// The run produced the launch's terminal: `completed` on any
    /// child exit, `failed` with the error's kind on a guest trap or
    /// host failure.
    pub(crate) fn finish<T>(&self, outcome: &Result<T, ProgramExecError>) {
        match outcome {
            Ok(_) => self.timeline.set_end(LaunchEnd::Completed),
            Err(error) => {
                self.timeline.set_end(LaunchEnd::Failed);
                self.timeline.set_error_kind(error.kind);
            }
        }
    }
}

impl<CpuImpl: Cpu> Drop for TaskTrace<CpuImpl> {
    fn drop(&mut self) {
        if self.timeline.task_emits {
            self.timeline.emit(&self.cpu);
        }
    }
}

/// The launch-call task's end of a [`Timeline`]: emits the line on
/// drop unless [`disarm`](Self::disarm) hands emission to the run task.
/// Holding one for the duration of a launch call makes every early
/// return — unavailable service, refused authority, unreadable source,
/// failed spawn — emit the partial timeline, which is the "failure
/// exit" line the phase list promises.
pub(crate) struct Trace<CpuImpl: Cpu> {
    cpu: CpuImpl,
    timeline: Timeline,
    emit_on_drop: bool,
}

impl<CpuImpl: Cpu> Trace<CpuImpl> {
    /// Begins a launch on this task: records `rpc-arrival` and arms the
    /// emit-on-drop guard.
    pub(crate) fn begin(cpu: CpuImpl, op: &'static str, program: &str) -> Self {
        Self {
            timeline: Timeline::begin(&cpu, op, program),
            cpu,
            emit_on_drop: true,
        }
    }

    /// Re-arms the emit-on-drop guard around a `Timeline` another
    /// scope began — a guest launch syscall hands the timeline down
    /// call by call, and each callee's failure exits are its own to
    /// report.
    pub(crate) fn adopt(cpu: CpuImpl, timeline: Timeline) -> Self {
        Self {
            cpu,
            timeline,
            emit_on_drop: true,
        }
    }

    /// The shared handle — handed to the loader, the spawn, and the
    /// run task.
    pub(crate) fn timeline(&self) -> &Timeline {
        &self.timeline
    }

    /// Records a phase boundary on this task.
    pub(crate) fn record(&self, phase: LaunchPhase) {
        self.timeline.record(&self.cpu, phase);
    }

    /// Records the source size; see [`Timeline::record_source_bytes`].
    pub(crate) fn record_source_bytes(&self, bytes: usize) {
        self.timeline.record_source_bytes(bytes);
    }

    /// Records the instance the launch produced.
    pub(crate) fn set_instance(&self, instance: crate::InstanceId) {
        self.timeline.set_instance(instance);
    }

    /// The launch ends refused — denied authority, an unresolvable
    /// name, a malformed call — carrying the kind. Marks nothing once
    /// disarmed: a disarmed caller's later exits are the child's line
    /// to print, not this launch's.
    pub(crate) fn refused(&self, kind: ProgramExecErrorKind) {
        if self.emit_on_drop {
            self.timeline.set_end(LaunchEnd::Refused);
            self.timeline.set_error_kind(kind);
        }
    }

    /// The launch ends failed — the machinery attempted it and lost —
    /// carrying the kind.
    pub(crate) fn failed(&self, kind: ProgramExecErrorKind) {
        if self.emit_on_drop {
            self.timeline.set_end(LaunchEnd::Failed);
            self.timeline.set_error_kind(kind);
        }
    }

    /// The launch ends carrying an error's own kind — refused or
    /// failed as the kind classifies it.
    pub(crate) fn exit_error(&self, error: &ProgramExecError) {
        if self.emit_on_drop {
            self.timeline.set_end(LaunchEnd::of(error.kind));
            self.timeline.set_error_kind(error.kind);
        }
    }

    /// An errno-shaped syscall exit carrying an error's kind too —
    /// the launch-line gets `error_kind` and `errno` both — yields the
    /// errno back out for the `return` site.
    pub(crate) fn exit_error_errno(&self, error: &ProgramExecError, errno: i32) -> i32 {
        if self.emit_on_drop {
            self.timeline.set_end(LaunchEnd::of(error.kind));
            self.timeline.set_error_kind(error.kind);
            self.timeline.set_errno(errno);
        }
        errno
    }

    /// An errno-shaped syscall exit — `end` is the call site's
    /// classification — records the `errno` field and yields the value
    /// back out for the `return` site.
    pub(crate) fn exit_errno(&self, end: LaunchEnd, errno: i32) -> i32 {
        if self.emit_on_drop {
            self.timeline.set_end(end);
            self.timeline.set_errno(errno);
        }
        errno
    }

    /// Hands emission to the run task: the line comes out when that
    /// task's [`TaskTrace`] drops, not when this guard does.
    pub(crate) fn disarm(&mut self) {
        self.emit_on_drop = false;
    }
}

impl<CpuImpl: Cpu> Drop for Trace<CpuImpl> {
    fn drop(&mut self) {
        if self.emit_on_drop {
            self.timeline.emit(&self.cpu);
        }
    }
}

/// Runs `f` only while [`TARGET`] is enabled. A call site that decodes
/// a guest-side string purely to fill the timeline's `program` field
/// wraps the decode in this, so a boot that never enabled the target
/// never does the work.
pub(crate) fn gated<T>(f: impl FnOnce() -> T) -> Option<T> {
    tracing::enabled!(target: TARGET, tracing::Level::DEBUG).then(f)
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use super::{GATE, LaunchPhase, LaunchTimeline, TARGET};

    /// The boundary names are the phase vocabulary: the emitted
    /// field names and the docs both derive from them, so a rename
    /// breaks the lane artifact's greps — the strings are pinned.
    #[test]
    fn launch_phase_names_are_stable() {
        let phases = [
            (LaunchPhase::RpcArrival, "rpc-arrival"),
            (LaunchPhase::SourceRead, "source-read"),
            (LaunchPhase::ArtifactTrust, "trust"),
            (LaunchPhase::CacheLookup, "cache-lookup"),
            (LaunchPhase::Deserialize, "deserialize"),
            (LaunchPhase::InstantiatePre, "instantiate-pre"),
            (LaunchPhase::LoadBegin, "load-begin"),
            (LaunchPhase::LoadComplete, "load-complete"),
            (LaunchPhase::TaskBegin, "task-begin"),
            (LaunchPhase::SharedMemory, "shared-memory"),
            (LaunchPhase::StorePrepare, "store-prepare"),
            (LaunchPhase::Instantiate, "instantiate"),
            (LaunchPhase::Start, "start"),
            (LaunchPhase::GuestBegin, "guest-begin"),
            (LaunchPhase::GuestEnd, "guest-end"),
            (LaunchPhase::StoreTeardown, "store-teardown"),
            (LaunchPhase::Completion, "completion"),
            (LaunchPhase::Reply, "reply"),
        ];
        for (phase, name) in phases {
            assert_eq!(phase.as_str(), name);
        }
    }

    /// The emitted line's field list is what the smoke lane's greps
    /// and the docs promise; the list is pinned in launch order.
    #[test]
    fn launch_phase_field_names_are_stable() {
        let fields = [
            (LaunchPhase::RpcArrival, "rpc_arrival_ns"),
            (LaunchPhase::SourceRead, "source_read_ns"),
            (LaunchPhase::ArtifactTrust, "trust_ns"),
            (LaunchPhase::CacheLookup, "cache_lookup_ns"),
            (LaunchPhase::Deserialize, "deserialize_ns"),
            (LaunchPhase::InstantiatePre, "instantiate_pre_ns"),
            (LaunchPhase::LoadBegin, "load_begin_ns"),
            (LaunchPhase::LoadComplete, "load_complete_ns"),
            (LaunchPhase::TaskBegin, "task_begin_ns"),
            (LaunchPhase::SharedMemory, "shared_memory_ns"),
            (LaunchPhase::StorePrepare, "store_prepare_ns"),
            (LaunchPhase::Instantiate, "instantiate_ns"),
            (LaunchPhase::Start, "start_ns"),
            (LaunchPhase::GuestBegin, "guest_begin_ns"),
            (LaunchPhase::GuestEnd, "guest_end_ns"),
            (LaunchPhase::StoreTeardown, "store_teardown_ns"),
            (LaunchPhase::Completion, "completion_ns"),
            (LaunchPhase::Reply, "reply_ns"),
        ];
        for (phase, field) in fields {
            assert_eq!(phase.offset_field(), field);
        }
        assert_eq!(fields.len(), LaunchPhase::ALL.len());
    }

    /// Offsets are measured from `rpc-arrival`, and a phase the launch
    /// never reached stays `None` — which the emitted line renders as
    /// the field's absence.
    #[test]
    fn timeline_offsets_measure_from_arrival() {
        let timeline = LaunchTimeline::new("exec", String::from("/bin/python3"));
        timeline.stamp(LaunchPhase::RpcArrival, 1_000);
        timeline.stamp(LaunchPhase::SourceRead, 1_600);
        timeline.stamp(LaunchPhase::Completion, 61_000);

        assert_eq!(timeline.offset(LaunchPhase::RpcArrival), Some(0));
        assert_eq!(timeline.offset(LaunchPhase::SourceRead), Some(600));
        assert_eq!(timeline.offset(LaunchPhase::Completion), Some(60_000));
        assert_eq!(timeline.offset(LaunchPhase::Deserialize), None);
        assert_eq!(timeline.recorded(), 3);
    }

    /// The target string is what the inspector enables and what a
    /// serial-log grep filters on; it is pinned for the same reason.
    #[test]
    fn phases_target_name_is_stable() {
        assert_eq!(TARGET, "helios_kernel::exec::phases");
        assert_eq!(GATE.name(), TARGET);
    }

    /// A subscriber that captures the emitted launch lines as
    /// `name=value` text — the terminal tests drive launch exits and
    /// assert what the line says about them.
    struct Capture {
        lines: std::sync::Mutex<alloc::vec::Vec<String>>,
    }

    struct FieldWriter(String);

    impl tracing::field::Visit for FieldWriter {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn core::fmt::Debug) {
            use core::fmt::Write;
            let _ = write!(self.0, " {}={:?}", field.name(), value);
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            use core::fmt::Write;
            let _ = write!(self.0, " {}={value}", field.name());
        }

        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            use core::fmt::Write;
            let _ = write!(self.0, " {}={value}", field.name());
        }
    }

    impl tracing::Subscriber for Capture {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            metadata.target() == TARGET
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut writer = FieldWriter(String::new());
            event.record(&mut writer);
            self.lines.lock().unwrap().push(writer.0);
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    /// A spawn refused by authority ends with a terminal line naming
    /// the refusal — the only phase it crossed is `rpc-arrival`.
    #[test]
    fn a_refused_launch_ends_its_line_refused() {
        use super::Trace;
        use crate::ProgramExecErrorKind;
        use crate::test_support::TestCpu;

        let lines = alloc::sync::Arc::new(Capture {
            lines: std::sync::Mutex::new(alloc::vec::Vec::new()),
        });
        let captured = lines.clone();
        tracing::subscriber::with_default(captured, || {
            let trace = Trace::begin(TestCpu::without_entropy(), "spawn", "/bin/python3");
            trace.refused(ProgramExecErrorKind::PermissionDenied);
            drop(trace);
        });

        let lines = lines.lines.lock().unwrap();
        assert_eq!(lines.len(), 1, "one launch emits one line");
        let line = &lines[0];
        assert!(line.contains("end=refused"), "line was: {line}");
        assert!(
            line.contains("error_kind=permission-denied"),
            "line was: {line}"
        );
        assert!(line.contains("rpc_arrival_ns=0"), "line was: {line}");
        // A phase the launch never reached leaves no field at all.
        assert!(!line.contains("completion_ns"), "line was: {line}");
    }

    /// A guest that traps between `guest-end` and `completion` ends
    /// its line `failed` with the error's kind — the run task's
    /// `finish` marks what the run produced and the guard emits it.
    #[test]
    fn a_trapping_guest_ends_its_line_failed() {
        use super::{LaunchPhase, Trace};
        use crate::test_support::TestCpu;
        use crate::{ProgramExecError, ProgramExecErrorDetail, ProgramExecErrorKind};

        let lines = alloc::sync::Arc::new(Capture {
            lines: std::sync::Mutex::new(alloc::vec::Vec::new()),
        });
        let captured = lines.clone();
        tracing::subscriber::with_default(captured, || {
            let mut trace = Trace::begin(TestCpu::without_entropy(), "exec", "/bin/python3");
            let task = trace.timeline().for_task();
            {
                let guard = task.task_guard(TestCpu::without_entropy());
                task.record(&TestCpu::without_entropy(), LaunchPhase::TaskBegin);
                task.record(&TestCpu::without_entropy(), LaunchPhase::GuestBegin);
                task.record(&TestCpu::without_entropy(), LaunchPhase::GuestEnd);
                guard.finish(&Err::<(), ProgramExecError>(ProgramExecError {
                    kind: ProgramExecErrorKind::Internal,
                    detail: ProgramExecErrorDetail::InternalInvariant,
                }));
            }
            trace.disarm();
        });

        let lines = lines.lines.lock().unwrap();
        assert_eq!(lines.len(), 1, "one launch emits one line");
        let line = &lines[0];
        assert!(line.contains("end=failed"), "line was: {line}");
        assert!(line.contains("error_kind=internal"), "line was: {line}");
        assert!(line.contains("guest_end_ns=0"), "line was: {line}");
        assert!(!line.contains("completion_ns"), "line was: {line}");
    }

    /// A run that produced a child exit ends its line `completed` —
    /// `TaskTrace::finish` marks the outcome the run task produced.
    #[test]
    fn a_finished_run_ends_its_line_completed() {
        use super::{LaunchPhase, Trace};
        use crate::test_support::TestCpu;

        let lines = alloc::sync::Arc::new(Capture {
            lines: std::sync::Mutex::new(alloc::vec::Vec::new()),
        });
        let captured = lines.clone();
        tracing::subscriber::with_default(captured, || {
            let mut trace = Trace::begin(TestCpu::without_entropy(), "spawn", "/bin/dash");
            let task = trace.timeline().for_task();
            {
                let guard = task.task_guard(TestCpu::without_entropy());
                task.record(&TestCpu::without_entropy(), LaunchPhase::TaskBegin);
                task.record(&TestCpu::without_entropy(), LaunchPhase::Completion);
                guard.finish(&Ok::<(), crate::ProgramExecError>(()));
            }
            trace.disarm();
        });

        let lines = lines.lines.lock().unwrap();
        assert_eq!(lines.len(), 1, "one launch emits one line");
        let line = &lines[0];
        assert!(line.contains("end=completed"), "line was: {line}");
        assert!(line.contains("completion_ns=0"), "line was: {line}");
    }
}
