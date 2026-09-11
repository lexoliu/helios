//! Per-launch phase timing on the [`TARGET`] tracing target.
//!
//! One event marks each boundary a program launch crosses — RPC
//! arrival, source read, artifact trust, cache lookup, deserialize,
//! `instantiate_pre`, store preparation, `instantiate_async`, start,
//! guest run, completion and reply — at `DEBUG` under one dedicated
//! target. The console filter holds the target off until a session
//! enables it by name through `helios:system/tracing`
//! (`set-target-enabled`), which is why these events exist at `DEBUG`
//! rather than at `INFO`: a boot that never asked for them emits none.
//!
//! Off costs one `enabled` check per site and nothing else. `tracing`
//! evaluates field values only inside its enabled branch, so `at_ns`
//! below reads the monotonic clock after the gate, never before it —
//! the read is the field expression, not a statement ahead of it.

use helios_hal::cpu::Cpu;

use super::monotonic_nanos;

/// The `tracing` target every launch-phase event is emitted under.
///
/// It is the name a session enables through `tracing
/// --enable-target` or `vm --enable-target`, and the prefix a
/// `tracing --target-prefix` fetch filters on.
pub(crate) const TARGET: &str = "helios_kernel::exec::phases";

/// The session gate for [`TARGET`], registered in
/// [`crate::log::DIAGNOSTIC_TARGETS`].
pub(crate) static GATE: crate::log::DiagnosticTarget = crate::log::DiagnosticTarget::new(TARGET);

/// One boundary a launch crosses, named for the work it completes.
///
/// Consecutive events of one launch — same `program` and `instance`,
/// ascending `at_ns` — bracket a phase: the time between `store-prepare`
/// and `instantiate` is `instantiate_async`, and so on. `Deserialize`
/// and the miss arm of `InstantiatePre` are absent on a warm cache, so
/// a launch's own sequence — not this enum's order — says which ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LaunchPhase {
    /// The `exec`/`spawn` host call — or a guest's own process
    /// launch syscall, which reaches the loader without passing
    /// through the `helios:system/programs` RPC — entered the kernel.
    /// Carries `op` and `program`; `instance` is unassigned (`0`).
    RpcArrival,
    /// The program's bytes were read out of its source. Carries
    /// `source_bytes`.
    SourceRead,
    /// The artifact's trust was established — the bootfs trailer parse,
    /// or the signature check for a signed artifact.
    ArtifactTrust,
    /// The deserialize cache answered. Carries `kind` and `hit`.
    CacheLookup,
    /// The `cwasm` payload finished deserializing. Emitted only on a
    /// cache miss.
    Deserialize,
    /// The `InstancePre` cache answered or `linker.instantiate_pre`
    /// built one. Carries `kind` and `hit`.
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
    /// being written. Carries `op`; `instance` is set when the call
    /// produced one.
    Reply,
}

impl LaunchPhase {
    /// The `phase` field value the event prints.
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
}

/// Emits one phase-boundary event.
///
/// `instance` is `0` before the registry assigns one — RPC arrival,
/// source read and the whole load path run before the spawn that
/// registers it. Every field is evaluated only when [`TARGET`] is
/// enabled, so a boot that never enabled it pays the check alone.
pub(crate) fn boundary<CpuImpl: Cpu>(
    cpu: &CpuImpl,
    program: &str,
    instance: Option<crate::InstanceId>,
    phase: LaunchPhase,
) {
    tracing::debug!(
        target: TARGET,
        at_ns = monotonic_nanos(cpu),
        program,
        instance = instance.map_or(0, crate::InstanceId::raw),
        phase = phase.as_str(),
    );
}

/// A boundary on a guest's own launch surface. `proc_spawn*` and
/// `proc_exec*` reach the loader without passing through the
/// `helios:system/programs` RPC handlers, so their `rpc-arrival` and
/// `reply` events are emitted where the syscall's launch work begins
/// and its result is committed.
pub(crate) fn syscall_boundary<CpuImpl: Cpu>(
    cpu: &CpuImpl,
    op: &'static str,
    program: &str,
    instance: u64,
    phase: LaunchPhase,
) {
    tracing::debug!(
        target: TARGET,
        at_ns = monotonic_nanos(cpu),
        op,
        program,
        instance,
        phase = phase.as_str(),
    );
}

/// Runs `f` only while [`TARGET`] is enabled. A call site that decodes
/// a guest-side string purely to fill an event's `program` field wraps
/// the decode in this, so a boot that never enabled the target never
/// does the work.
pub(crate) fn gated<T>(f: impl FnOnce() -> T) -> Option<T> {
    tracing::enabled!(target: TARGET, tracing::Level::DEBUG).then(f)
}

#[cfg(test)]
mod tests {
    use super::{GATE, LaunchPhase, TARGET};

    /// The phase names are the lane artifact's vocabulary: a rename
    /// breaks every grep that reads them, so the strings are pinned.
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

    /// The target string is what the inspector enables and what a
    /// serial-log grep filters on; it is pinned for the same reason.
    #[test]
    fn phases_target_name_is_stable() {
        assert_eq!(TARGET, "helios_kernel::exec::phases");
        assert_eq!(GATE.name(), TARGET);
    }
}
