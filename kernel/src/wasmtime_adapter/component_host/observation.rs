//! What a caller may do through `helios:system/tracing` and
//! `helios:system/profiling`, decided once for both worlds.
//!
//! Both worlds link both interfaces, and every program is linked against
//! them (linkage is cached per binary while authority belongs to the
//! instance), so the refusal happens at the call. Each host function in
//! either world reaches the kernel through one function here, which reads
//! the caller's [`crate::ProcessAuthority`] and answers in kernel types;
//! the world's own conversion then renders the answer in its generated
//! types. That keeps the two worlds from drifting apart in policy: the
//! debugger sees the whole machine because it was provisioned with the
//! right, not because its world skips the check.
//!
//! Two kinds of call:
//! - A control (`set-target-enabled`, `profiling.set-enabled`, `clear`,
//!   `set-instruction-sampling`, `instruction-samples`, `raw-profile-size`
//!   and `raw-profile-read`) acts on the whole machine, or reads what
//!   cannot be attributed to one instance, and is refused with
//!   [`Denied`] unless the caller holds [`ObserveMachineCap`].
//! - A read of the histories (`recent`, `subscribe`, `folded`, `metrics`)
//!   is answered for every caller, through the scope its authority
//!   selects: the whole machine for a holder of the right, and only the
//!   caller's own records otherwise. The kernel's own records are never
//!   returned without the right.
//!
//! Concurrency: everything here runs on the task that owns the store and
//! reads the store's own authority, which no other processor changes. The
//! reads then take the history locks only as `exec/observer.rs` states.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use helios_hal::cpu::Cpu;
use triomphe::Arc;

use crate::{
    ComponentHostNetwork, FoldedProfileSample, InstructionProfile, InstructionProfileError,
    KernelLlvmProfile, LlvmProfile, LlvmProfileError, ObserveMachineCap, OwnInstance,
    PerfMetricFilter, PerfMetricSample, ProfileFilter, TraceEvent, TraceFilter,
};

use super::StoreData;

/// The caller does not hold the right to observe the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Denied;

/// A control that the caller may not make, or that it made and the kernel
/// could not carry out.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Refused<Failure> {
    Denied,
    Failed(Failure),
}

impl<Failure> From<Denied> for Refused<Failure> {
    fn from(Denied: Denied) -> Self {
        Self::Denied
    }
}

/// Why `set-target-enabled` did not flip a gate.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum TargetRefusal {
    Denied,
    /// The kernel gates no diagnostic target under that name; the text is
    /// the kernel's own rendering of why.
    Unknown(String),
}

impl From<Denied> for TargetRefusal {
    fn from(Denied: Denied) -> Self {
        Self::Denied
    }
}

/// The scope a read runs in, chosen from the caller's authority.
///
/// A read is generic over [`crate::ObservationScope`], and the scope is a
/// different type for the two cases, so the choice is made here, once, and
/// each read then runs monomorphised. This carries the choice from the
/// authority check to the call that uses it; it is not itself a scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Visibility {
    /// The caller holds the right: every record, the kernel's included.
    Machine(ObserveMachineCap),
    /// The caller does not: only what its own instance wrote.
    Own(OwnInstance),
}

impl Visibility {
    pub(super) fn of<CpuImpl, Net, HostFs>(data: &StoreData<CpuImpl, Net, HostFs>) -> Self
    where
        CpuImpl: Cpu + Clone,
        Net: ComponentHostNetwork,
        HostFs: crate::HostFileSystem,
    {
        match data.process_authority().derive_observe_machine_cap() {
            Ok(cap) => Self::Machine(cap),
            Err(_) => Self::Own(OwnInstance::new(data.instance().id())),
        }
    }
}

/// The proof that the caller may control and read the whole machine.
fn observe_machine<CpuImpl, Net, HostFs>(
    data: &StoreData<CpuImpl, Net, HostFs>,
) -> Result<ObserveMachineCap, Denied>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    data.process_authority()
        .derive_observe_machine_cap()
        .map_err(|_| Denied)
}

/// `tracing.set-target-enabled`.
pub(super) fn set_target_enabled<CpuImpl, Net, HostFs>(
    data: &StoreData<CpuImpl, Net, HostFs>,
    target: &str,
    enabled: bool,
) -> Result<(), TargetRefusal>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    observe_machine(data)?;
    crate::log::set_gated_target(target, enabled)
        .map_err(|error| TargetRefusal::Unknown(error.to_string()))
}

/// `tracing.recent`, after the filter is lifted.
pub(super) fn recent_events<CpuImpl, Net, HostFs>(
    data: &StoreData<CpuImpl, Net, HostFs>,
    filter: &TraceFilter,
    limit: u32,
) -> Vec<Arc<TraceEvent>>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    match Visibility::of(data) {
        Visibility::Machine(scope) => data.runtime_state.recent(&scope, filter, limit),
        Visibility::Own(scope) => data.runtime_state.recent(&scope, filter, limit),
    }
}

/// `profiling.set-enabled`.
pub(super) fn set_profiling_enabled<CpuImpl, Net, HostFs>(
    data: &StoreData<CpuImpl, Net, HostFs>,
    enabled: bool,
) -> Result<(), Denied>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    observe_machine(data)?;
    data.runtime_state.set_profiling_enabled(enabled);
    Ok(())
}

/// `profiling.clear`.
pub(super) fn clear_profile<CpuImpl, Net, HostFs>(
    data: &StoreData<CpuImpl, Net, HostFs>,
) -> Result<(), Denied>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    observe_machine(data)?;
    data.runtime_state.clear_profile();
    Ok(())
}

/// `profiling.folded`, after the filter is lifted.
pub(super) fn folded_samples<CpuImpl, Net, HostFs>(
    data: &StoreData<CpuImpl, Net, HostFs>,
    filter: &ProfileFilter,
    limit: u32,
) -> Vec<FoldedProfileSample>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let ticks = data.cpu.now().ticks();
    match Visibility::of(data) {
        Visibility::Machine(scope) => data
            .runtime_state
            .folded_profile(ticks, &scope, filter, limit),
        Visibility::Own(scope) => data
            .runtime_state
            .folded_profile(ticks, &scope, filter, limit),
    }
}

/// `profiling.metrics`, after the filter is lifted.
pub(super) fn metric_samples<CpuImpl, Net, HostFs>(
    data: &StoreData<CpuImpl, Net, HostFs>,
    filter: &PerfMetricFilter,
    limit: u32,
) -> Vec<PerfMetricSample>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    match Visibility::of(data) {
        Visibility::Machine(scope) => data.runtime_state.perf_metrics(&scope, filter, limit),
        Visibility::Own(scope) => data.runtime_state.perf_metrics(&scope, filter, limit),
    }
}

/// `profiling.set-instruction-sampling`.
pub(super) fn set_instruction_sampling<CpuImpl, Net, HostFs>(
    data: &StoreData<CpuImpl, Net, HostFs>,
    enabled: bool,
) -> Result<(), Refused<InstructionProfileError>>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    observe_machine(data)?;
    data.runtime_state
        .instruction_sampler()
        .set_sampling(enabled)
        .map_err(Refused::Failed)
}

/// `profiling.instruction-samples`.
///
/// The outer error is a bring-up ordering defect, not a refusal: only the
/// program service knows the wasm code that can run, so a read before it
/// exists is not an empty profile.
pub(super) fn instruction_samples<CpuImpl, Net, HostFs>(
    data: &StoreData<CpuImpl, Net, HostFs>,
) -> wasmtime::Result<Result<InstructionProfile, Refused<InstructionProfileError>>>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    if observe_machine(data).is_err() {
        return Ok(Err(Refused::Denied));
    }
    let service = data.runtime_state.program_service().ok_or_else(|| {
        wasmtime::Error::msg(
            "instruction samples were read before the program service was installed",
        )
    })?;
    Ok(data
        .runtime_state
        .instruction_sampler()
        .profile(service.code_map())
        .map_err(Refused::Failed))
}

/// `profiling.raw-profile-size`.
pub(super) fn raw_profile_size<CpuImpl, Net, HostFs>(
    data: &StoreData<CpuImpl, Net, HostFs>,
) -> Result<u64, Refused<LlvmProfileError>>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    observe_machine(data)?;
    KernelLlvmProfile.size().map_err(Refused::Failed)
}

/// `profiling.raw-profile-read`.
pub(super) fn raw_profile_read<CpuImpl, Net, HostFs>(
    data: &StoreData<CpuImpl, Net, HostFs>,
    offset: u64,
    length: u32,
) -> Result<Vec<u8>, Refused<LlvmProfileError>>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    observe_machine(data)?;
    super::read_raw_profile(offset, length).map_err(Refused::Failed)
}

#[cfg(test)]
mod observation_authority_tests {
    use alloc::vec::Vec;

    use super::*;
    use crate::wasmtime_adapter::component_host::service::test_store::{
        TestStoreData, store_data_authorized,
    };
    use crate::{
        DiagnosticAuthorityRights, InstanceId, PerfSample, PrefixSet, ProcessAuthority,
        ProfileScope,
    };
    use helios_hal::cpu::HardwarePerfCounterDelta;

    /// A store whose instance holds `rights`.
    fn store(rights: DiagnosticAuthorityRights) -> TestStoreData {
        let mut authority = ProcessAuthority::empty();
        if !rights.is_empty() {
            authority.grant_diagnostic_rights(rights);
        }
        store_data_authorized(authority, |_| {})
    }

    fn ordinary() -> TestStoreData {
        store(DiagnosticAuthorityRights::empty())
    }

    fn observer() -> TestStoreData {
        store(DiagnosticAuthorityRights::OBSERVE_MACHINE)
    }

    /// An instance other than the one `data` runs.
    fn other_instance(data: &TestStoreData) -> InstanceId {
        InstanceId::from_raw(data.instance().id().raw() + 1000)
    }

    fn any_trace() -> TraceFilter {
        TraceFilter {
            min_level: None,
            target_prefixes: PrefixSet::default(),
        }
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

    /// Records a console line from the kernel, one from `data`'s own
    /// instance and one from another instance.
    fn record_events(data: &TestStoreData) {
        let own = data.instance().id();
        let other = other_instance(data);
        let state = &data.runtime_state;
        state.record_console_text(1, None, "INFO [kernel] boot\n");
        state.record_console_text(2, Some(own), "INFO [guest] mine\n");
        state.record_console_text(3, Some(other), "INFO [guest] theirs\n");
    }

    /// Enables profiling and records a kernel stack and metric, and a
    /// user stack and metric for `data`'s own instance and for another.
    fn record_profile(data: &TestStoreData) {
        let own = data.instance().id();
        let other = other_instance(data);
        let state = &data.runtime_state;
        state.set_profiling_enabled(true);
        state.record_profile_stack_str_nanos(ProfileScope::Kernel, "kernel;executor", 100);
        state.record_user_profile(own, "dash", 3);
        state.record_user_profile(other, "dash", 40);
        state.record_user_profile(other, "other", 5);
        let timed = |nanos| PerfSample::timed(nanos, HardwarePerfCounterDelta::default(), 0);
        state.record_perf_metric_str(ProfileScope::Kernel, "kernel;m", timed(9));
        state.record_perf_metric_str(ProfileScope::User, "user;m", timed(2));
    }

    fn origins(events: &[Arc<TraceEvent>]) -> Vec<Option<InstanceId>> {
        events.iter().map(|event| event.origin).collect()
    }

    #[test]
    fn visibility_follows_the_callers_authority() {
        let holder = observer();
        assert!(matches!(Visibility::of(&holder), Visibility::Machine(_)));

        let program = ordinary();
        assert_eq!(
            Visibility::of(&program),
            Visibility::Own(OwnInstance::new(program.instance().id()))
        );
    }

    #[test]
    fn ordinary_program_is_refused_the_global_controls() {
        let program = ordinary();
        let enabled_before = program.runtime_state.profiling_enabled();
        // A real target: were the check not first, this would flip the
        // gate and answer `Ok`.
        assert_eq!(
            set_target_enabled(&program, crate::exec::phases::TARGET, true),
            Err(TargetRefusal::Denied)
        );
        assert_eq!(set_profiling_enabled(&program, true), Err(Denied));
        assert_eq!(clear_profile(&program), Err(Denied));
        assert_eq!(
            set_instruction_sampling(&program, true),
            Err(Refused::Denied)
        );
        assert!(matches!(
            instruction_samples(&program).expect("the refusal precedes the bring-up check"),
            Err(Refused::Denied)
        ));
        assert_eq!(raw_profile_size(&program), Err(Refused::Denied));
        assert_eq!(raw_profile_read(&program, 0, 16), Err(Refused::Denied));
        // Refused before the window is even checked against the limit.
        assert_eq!(
            raw_profile_read(&program, 0, crate::MAX_PROFILE_READ + 1),
            Err(Refused::Denied)
        );

        // Nothing the refused calls would have changed was changed.
        assert_eq!(
            program.runtime_state.profiling_enabled(),
            enabled_before,
            "a refused set-enabled must not turn profiling on"
        );
    }

    #[test]
    fn refused_clear_leaves_the_profile_alone() {
        let program = ordinary();
        record_profile(&program);

        assert_eq!(clear_profile(&program), Err(Denied));

        let own = folded_samples(&program, &any_stack(), 0);
        assert_eq!(own.len(), 1, "the caller's own sample survived the clear");
    }

    #[test]
    fn holder_of_the_right_passes_every_global_control() {
        let holder = observer();
        // Each of these reaches the kernel and answers with what the
        // fixture's kernel says, never `Denied`. A name nothing registered
        // keeps the target gate untouched.
        assert!(matches!(
            set_target_enabled(&holder, "helios_kernel::exec::nope", true),
            Err(TargetRefusal::Unknown(_))
        ));
        assert_eq!(set_profiling_enabled(&holder, true), Ok(()));
        assert!(holder.runtime_state.profiling_enabled());
        assert_eq!(clear_profile(&holder), Ok(()));
        assert_eq!(set_profiling_enabled(&holder, false), Ok(()));
        assert!(!holder.runtime_state.profiling_enabled());
        // The fixture installs no instruction capture and no program
        // service, and its kernel image carries no instrumentation.
        assert_eq!(
            set_instruction_sampling(&holder, true),
            Err(Refused::Failed(InstructionProfileError::NotCaptured))
        );
        assert!(instruction_samples(&holder).is_err());
        assert!(!matches!(raw_profile_size(&holder), Err(Refused::Denied)));
        assert!(!matches!(
            raw_profile_read(&holder, 0, 16),
            Err(Refused::Denied)
        ));
    }

    #[test]
    fn ordinary_program_cannot_read_another_instances_events() {
        let program = ordinary();
        record_events(&program);

        // It sees what it wrote, and neither the kernel's console nor the
        // other instance's output.
        let seen = recent_events(&program, &any_trace(), 16);
        assert_eq!(origins(&seen), [Some(program.instance().id())]);
        assert_eq!(seen[0].target, "guest");
    }

    #[test]
    fn program_that_wrote_nothing_reads_no_events() {
        let program = ordinary();
        let state = &program.runtime_state;
        state.record_console_text(1, None, "INFO [kernel] boot\n");
        state.record_console_text(2, Some(other_instance(&program)), "INFO [guest] theirs\n");

        assert!(recent_events(&program, &any_trace(), 16).is_empty());
    }

    #[test]
    fn debugger_scope_reads_every_instances_events() {
        let holder = observer();
        record_events(&holder);

        let seen = recent_events(&holder, &any_trace(), 16);
        assert_eq!(
            origins(&seen),
            [
                None,
                Some(holder.instance().id()),
                Some(other_instance(&holder))
            ]
        );
    }

    #[test]
    fn ordinary_program_reads_only_its_own_profile() {
        let program = ordinary();
        record_profile(&program);

        let folded = folded_samples(&program, &any_stack(), 0);
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].scope, ProfileScope::User);
        assert_eq!(&*folded[0].stack, "user;dash");
        assert_eq!(folded[0].weight, 3, "not summed with another instance's");

        let metrics = metric_samples(&program, &any_metric(), 0);
        assert!(
            metrics.is_empty(),
            "the user metric was recorded for no instance and the kernel's is not its own"
        );
    }

    #[test]
    fn holder_of_the_right_reads_the_merged_profile() {
        let holder = observer();
        record_profile(&holder);

        let folded = folded_samples(&holder, &any_stack(), 0);
        let rows = folded
            .iter()
            .map(|row| (row.scope, &*row.stack, row.weight))
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                (ProfileScope::Kernel, "kernel;executor", 100),
                (ProfileScope::User, "user;dash", 43),
                (ProfileScope::User, "user;other", 5),
            ]
        );

        let metrics = metric_samples(&holder, &any_metric(), 0);
        assert_eq!(metrics.len(), 2);
    }
}
