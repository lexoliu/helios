//! ACPI processor hotplug on x86: the SCI, the GPE blocks, and the AML
//! that `Notify`s a hot-added or hot-removed processor.
//!
//! QEMU/KVM announces a vCPU hot-add or hot-remove by setting a GPE
//! status bit (`_E02` on q35), which raises the SCI. The SCI arrives
//! through the I/O APIC on the bootstrap processor, where
//! [`SciEvents::on_interrupt`] masks the pending GPEs, remembers them and
//! signals the hotplug task. That task, local to the bootstrap processor
//! (`helios_kernel`'s processor hotplug service), runs each GPE's method
//! in the AML interpreter (`\_SB.CPUS.CSCN` on QEMU), turns the
//! processor notifications into requests on the possible set by the local
//! APIC id each processor's `_MAT` names, and hands them to the kernel's
//! lifecycle; the answers go back through `_EJ0` and `_OST`. The ACPI
//! mechanics are `helios_acpi`'s; this module routes the line, maps APIC
//! ids to slots and waits for a processor to stop before it is ejected.
//!
//! # Concurrency contract
//!
//! The SCI handler and the hotplug task both run on the bootstrap
//! processor, which never goes offline; they share only [`SciEvents`]:
//! the GPE registers, whose enable writes the task makes with that
//! processor's interrupts masked, the latched set (atomic) and the
//! kernel `Notify` (a coalesced permit, so a latch between the task's
//! check and its park is not lost). The AML interpreter is the task's
//! alone. A processor with no slot in the possible set — firmware
//! notifying a processor whose `_MAT` names an APIC id the MADT did not
//! list at boot — panics: every per-processor structure was sized at
//! boot, and QEMU lists every possible vCPU in the MADT.

use alloc::boxed::Box;
use alloc::vec::Vec;

use acpi::AcpiTables;
use acpi::registers::{FixedRegisters, Pm1ControlBit, Pm1EventFlags};
use acpi::sdt::fadt::Fadt;
use helios_acpi::gpe::{GpeMethods, GpeRegisters};
use helios_acpi::processor::{LocalApicId, ProcessorObjects};
use helios_hal::cpu::ProcessorId;
use helios_hal::processor_hotplug::{HotplugOutcome, HotplugRequest, ProcessorHotplug};
use helios_kernel::Notify;

use crate::X86Cpu;
use crate::exceptions::SCI_INTERRUPT_VECTOR;
use crate::ioapic::LegacyLine;
use crate::smp::{AcpiNamespace, NotifyingAcpiHandler, PhysicalOffsetAcpiHandler};

/// How long firmware may take to hand the ACPI registers to the OS once
/// asked: ACPICA's `AcpiEnable` waits three seconds for `SCI_EN`.
const ACPI_ENABLE_TIMEOUT_MILLIS: u64 = 3_000;

/// What the SCI handler and the hotplug task share.
pub(crate) struct SciEvents {
    gpes: GpeRegisters<PhysicalOffsetAcpiHandler>,
    signal: Notify,
}

impl SciEvents {
    /// The SCI handler: masks and latches every pending GPE, and wakes
    /// the hotplug task when there was one. An SCI with nothing latched
    /// is a shared line's other source, or a GPE the task already took.
    pub(crate) fn on_interrupt(&self) {
        if self.gpes.latch() {
            self.signal.notify_one_coalesced();
        }
    }
}

/// The hotplug task's half: the namespace, the GPE methods and the
/// processors requests have named.
pub(crate) struct X86ProcessorHotplug {
    namespace: AcpiNamespace,
    recorder: NotifyingAcpiHandler,
    events: &'static SciEvents,
    methods: GpeMethods,
    processors: ProcessorObjects<LocalApicId>,
    cpu: X86Cpu,
}

/// Takes the ACPI event hardware from firmware and routes the SCI to the
/// bootstrap processor: every GPE is disabled and cleared, the fixed
/// events stay off, the GPEs whose `_Exx`/`_Lxx` the namespace provides
/// are enabled, and the I/O APIC entry is programmed last.
///
/// Runs once on the bootstrap processor, with its interrupts masked and
/// before any secondary starts. Returns `None` on a hardware-reduced ACPI
/// machine, which has no SCI or GPE blocks and announces events through a
/// Generic Event Device instead.
pub(crate) fn install(
    cpu: &X86Cpu,
    tables: &AcpiTables<PhysicalOffsetAcpiHandler>,
    namespace: AcpiNamespace,
    recorder: NotifyingAcpiHandler,
) -> Option<X86ProcessorHotplug> {
    let fadt = tables
        .find_table::<Fadt>()
        .unwrap_or_else(|| panic!("x86 ACPI: the tables carry no FADT"));
    // `Fadt` is packed, so its flags are copied out first.
    if { fadt.flags }.system_is_hw_reduced_acpi() {
        tracing::info!(
            "hardware-reduced ACPI: no SCI or GPE blocks, so no processor hotplug through them"
        );
        return None;
    }
    let handler = recorder.inner().clone();
    // The event registers are firmware's until the mode switch.
    take_acpi_mode(&fadt, &handler);
    let gpes = GpeRegisters::from_fadt(&fadt, handler.clone())
        .unwrap_or_else(|error| panic!("x86 ACPI: {error}"));
    gpes.disable_all();
    let methods =
        GpeMethods::discover(&namespace, &gpes).unwrap_or_else(|error| panic!("x86 ACPI: {error}"));
    let events: &'static SciEvents = Box::leak(Box::new(SciEvents {
        gpes,
        signal: Notify::new(),
    }));
    cpu.platform_state().install_sci_events(events);
    methods.enable_all(&events.gpes);
    let sci_interrupt = fadt.sci_interrupt;
    cpu.platform_state().io_apics().program_redirection(
        LegacyLine::sci(sci_interrupt),
        SCI_INTERRUPT_VECTOR,
        cpu.bootstrap_apic_id(),
    );
    let served: Vec<_> = methods
        .methods()
        .iter()
        .map(|method| alloc::format!("{}", method.path))
        .collect();
    tracing::info!(
        sci = sci_interrupt,
        gpes = events.gpes.gpes().count(),
        methods = ?served,
        "ACPI SCI online"
    );
    Some(X86ProcessorHotplug {
        namespace,
        recorder,
        events,
        methods,
        processors: ProcessorObjects::default(),
        cpu: cpu.clone(),
    })
}

/// Hands the ACPI hardware from firmware to the OS: `SCI_EN` set, by
/// writing the FADT's `ACPI_ENABLE` to its `SMI_CMD` when firmware left
/// it clear (ACPI 6.5 §5.2.9). Every fixed event then stays off: the
/// kernel serves none, and an enabled one would raise an SCI no method
/// answers.
fn take_acpi_mode(fadt: &Fadt, handler: &PhysicalOffsetAcpiHandler) {
    let registers = FixedRegisters::new(fadt, handler.clone())
        .unwrap_or_else(|error| panic!("x86 ACPI: FADT fixed registers: {error:?}"));
    enable_sci(fadt, handler, &registers);
    let events = &registers.pm1_event_registers;
    events
        .set_enable_flags(Pm1EventFlags::empty())
        .and_then(|()| events.clear_events(Pm1EventFlags::all()))
        .unwrap_or_else(|error| panic!("x86 ACPI: PM1 event registers: {error:?}"));
}

fn enable_sci(
    fadt: &Fadt,
    handler: &PhysicalOffsetAcpiHandler,
    registers: &FixedRegisters<PhysicalOffsetAcpiHandler>,
) {
    let sci_enabled = || {
        registers
            .pm1_control_registers
            .read_bit(Pm1ControlBit::SciEnable)
            .unwrap_or_else(|error| panic!("x86 ACPI: reading SCI_EN: {error:?}"))
    };
    if sci_enabled() {
        return;
    }
    let smi_command = fadt.smi_cmd_port;
    let acpi_enable = fadt.acpi_enable;
    let port = u16::try_from(smi_command)
        .ok()
        .filter(|port| *port != 0 && acpi_enable != 0)
        .unwrap_or_else(|| {
            panic!(
                "x86 ACPI: SCI_EN is clear and the FADT names no SMI command to set it \
                 (SMI_CMD {smi_command:#x}, ACPI_ENABLE {acpi_enable:#x})"
            )
        });
    acpi::Handler::write_io_u8(handler, port, acpi_enable);
    for _ in 0..ACPI_ENABLE_TIMEOUT_MILLIS {
        if sci_enabled() {
            return;
        }
        acpi::Handler::stall(handler, 1_000);
    }
    panic!(
        "x86 ACPI: firmware did not set SCI_EN within {ACPI_ENABLE_TIMEOUT_MILLIS} ms of \
         ACPI_ENABLE {acpi_enable:#x} written to SMI_CMD {port:#x}"
    );
}

impl ProcessorHotplug for X86ProcessorHotplug {
    async fn requests(&mut self) -> Vec<HotplugRequest> {
        loop {
            // A coalesced permit: an SCI latched before this wait was
            // armed, or since the last pass, completes it at once.
            self.events.signal.notified().await;
            for gpe in self.events.gpes.take_latched() {
                self.methods
                    .run(gpe, &self.namespace, &self.events.gpes)
                    .unwrap_or_else(|error| panic!("x86 ACPI: {error}"));
            }
            let platform = self.cpu.platform_state();
            let requests = self
                .processors
                .requests(&self.namespace, self.recorder.take(), |apic| {
                    platform.processor_with_apic_id(apic.0)
                })
                .unwrap_or_else(|error| panic!("x86 processor hotplug: {error}"));
            if !requests.is_empty() {
                return requests;
            }
        }
    }

    async fn eject(&mut self, processor: ProcessorId) {
        let platform = self.cpu.platform_state();
        // The kernel publishes a processor offline just before it runs
        // the stop path; firmware destroys the vCPU on `_EJ0`, so it has
        // to be in its halt loop first, holding no lock.
        let deadline = crate::read_tsc()
            .checked_add(platform.stop_reach_ticks())
            .unwrap_or_else(|| panic!("x86 eject deadline overflow"));
        while !platform.has_stopped(processor) {
            assert!(
                crate::read_tsc() <= deadline,
                "x86 processor {} went offline but never reached its halt, so it cannot be \
                 released to firmware",
                processor.id()
            );
            helios_kernel::yield_now().await;
        }
        self.processors
            .eject(&self.namespace, processor)
            .unwrap_or_else(|error| panic!("x86 processor hotplug: {error}"));
        tracing::info!(processor = processor.id(), "Processor released to firmware");
    }

    fn report(&mut self, request: HotplugRequest, outcome: HotplugOutcome) {
        self.processors
            .report(&self.namespace, request, outcome)
            .unwrap_or_else(|error| panic!("x86 processor hotplug: {error}"));
    }
}
