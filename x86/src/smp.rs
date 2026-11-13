extern crate alloc;

use acpi::platform::{AcpiPlatform, ProcessorState};
use acpi::{AcpiTables, Handler, PciAddress, RawPhysicalMapping};
use alloc::alloc::{Layout, alloc_zeroed};
use alloc::boxed::Box;
use alloc::sync::Arc;
use core::arch::x86_64::__cpuid;
use core::num::NonZeroUsize;

use core::arch::global_asm;
use core::ops::Range;
use core::ptr::NonNull;
use core::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicUsize, Ordering, compiler_fence,
};
use helios_artifact::ProbeAnswers;
use helios_hal::cpu::{GUEST_STACK_LIMIT_OFFSET, ProcessorId, Timebase};
use helios_hal::critical_section::ProcessorIdentity;
use helios_hal::watchdog::Watchdog;
use helios_kernel::{InstructionSampler, SampleCapture, Timer, WasmtimeTlsSlots};
use pci_types::ConfigRegionAccess;
use spin::Once;
use x86::apic::x2apic::X2APIC;
use x86::apic::xapic::{
    XAPIC, XAPIC_ESR, XAPIC_LVT_ERROR, XAPIC_LVT_TIMER, XAPIC_SVR, XAPIC_TIMER_CURRENT_COUNT,
    XAPIC_TIMER_DIV_CONF, XAPIC_TIMER_INIT_COUNT,
};
use x86::apic::{ApicControl, ApicId};
use x86::msr::{
    IA32_APIC_BASE, IA32_FS_BASE, IA32_X2APIC_CUR_COUNT, IA32_X2APIC_DIV_CONF, IA32_X2APIC_ESR,
    IA32_X2APIC_ICR, IA32_X2APIC_INIT_COUNT, IA32_X2APIC_LVT_ERROR, IA32_X2APIC_LVT_TIMER, rdmsr,
    wrmsr,
};
use x86_64::PhysAddr;
use x86_64::VirtAddr;
use x86_64::instructions::tlb;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::page_table::PageTableEntry;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageSize, PageTable, PageTableFlags, PhysFrame,
    Size2MiB, Size4KiB,
};

use crate::KERNEL_STACK_BYTES;
use crate::debug_state;
use crate::exceptions::{
    DeviceInterruptRoutes, EXCEPTION_STACK_BYTES, ProcessorIdt, ProcessorSegments,
};
use crate::extended_state::Xcr0;
use crate::ioapic::IoApic;
use crate::pci::LegacyPciConfigAccess;
use crate::read_tsc;
use crate::watchdog::X86Watchdog;

pub(crate) const WAKEUP_PAGE_BYTES: usize = 4096;
pub(crate) const SIPI_MAX_PHYSICAL_ADDRESS: usize = 0x10_0000;
const AP_STARTUP_INIT_ASSERT_MICROS: u64 = 10_000;
const AP_STARTUP_INTER_IPI_MICROS: u64 = 200;
/// The calibration window, which is also the scheduler tick: the count
/// the timer runs down in this window is the periodic initial count.
const APIC_TIMER_CALIBRATION_MICROS: u64 = 10_000;
/// Timer interrupts per scheduler tick while instruction sampling runs.
///
/// The scheduler tick is 10 ms (100 Hz), ten times too coarse to split a
/// workload's time between functions, and raising it would also raise the
/// guest preemption rate and change how often guests yield. Sampling
/// therefore shortens the period by this factor and runs the scheduler's
/// work — watchdog, guest preemption, kernel timer — on every tenth
/// interrupt only, so a profiled run keeps the scheduling an unprofiled
/// one has and is sampled at 1 kHz per processor.
const SAMPLES_PER_SCHEDULER_TICK: u32 = 10;
const APIC_TIMER_DIVIDE_BY_ONE: u32 = 0b1011;
const APIC_TIMER_MASKED: u64 = 1 << 16;
const APIC_TIMER_PERIODIC: u64 = 1 << 17;
const APIC_TIMER_MODE_MASK: u64 = 0b11 << 17;
const APIC_TIMER_VECTOR_MASK: u64 = 0xff;
const PAGE_BYTES: usize = 4096;

// TLB shootdown, and who takes part in it.
//
// `ONLINE_PROCESSOR_MASK` is the set of processors a broadcast waits on:
// a processor joins it in `join_shootdown_targets`, immediately before it
// enables interrupts, and never earlier. A member that cannot take the
// shootdown IPI would hold every broadcaster until it could, and a
// broadcaster it was itself waiting on would never get there. Before it
// joins, a processor may broadcast — its boot-time page-fault probe does,
// with interrupts still masked — because nobody waits on it; once it has
// joined it broadcasts with interrupts enabled only, and
// `shootdown_tlb_range` asserts exactly that, so a broadcast from fault
// context or from under a lock that masks interrupts fails at once with
// the processor named instead of wedging the machine.
//
// One broadcast is in flight at a time: `TLB_SHOOTDOWN_LOCK` is held from
// the moment the range is published until the last target has
// acknowledged it, so a receiver always reads the range of the broadcast
// whose IPI woke it, and an acknowledgement always counts toward the
// broadcast it answers. Without the lock two concurrent broadcasters
// overwrote each other's range and reset each other's acknowledgement
// mask: some receivers flushed the wrong range, and a broadcaster whose
// own bit the other had erased spun forever if it could not take the IPI
// that would have restored it (#332). A processor spinning on the lock has
// interrupts enabled, by the assertion above, so it keeps answering the
// holder's broadcast while it waits.
static ONLINE_PROCESSOR_MASK: AtomicUsize = AtomicUsize::new(0);
static TLB_SHOOTDOWN_LOCK: spin::Mutex<()> = spin::Mutex::new(());
static TLB_SHOOTDOWN_START: AtomicUsize = AtomicUsize::new(0);
static TLB_SHOOTDOWN_LEN: AtomicUsize = AtomicUsize::new(0);
static TLB_SHOOTDOWN_ACK_MASK: AtomicUsize = AtomicUsize::new(0);

/// The logical id the bootstrap anchor carries, so that a processor asking
/// which one it is before its runtime exists is told so by name instead of
/// being answered with processor zero.
const BOOTSTRAP_ANCHOR_LOGICAL_ID: u16 = u16::MAX;

/// The anchor the bootstrap processor runs on between its first Rust
/// instruction and [`activate_runtime`].
///
/// The bootstrap processor is singular, so one static anchor can never be
/// mistaken for a second processor's; every application processor has a real
/// runtime before it is started, and the wakeup trampoline installs that
/// runtime's address in `IA32_FS_BASE` before the first compiler-generated
/// instruction runs on it. This word exists before the allocator that would
/// otherwise own it, which is the whole of why it is a `static`.
static BOOTSTRAP_ANCHOR: ProcessorAnchor = ProcessorAnchor::new(BOOTSTRAP_ANCHOR_LOGICAL_ID);

/// The words the executing processor answers `fs`-relative loads with.
///
/// `IA32_FS_BASE` holds the address of the executing processor's anchor from
/// before that processor's first Rust instruction onwards, so the two
/// questions the kernel's hottest paths ask — which processor is this, and
/// which identity does it take critical sections with — are one load off `fs`
/// rather than an `rdmsr`, the way Linux's `this_cpu_*` reads `gs`. The layout
/// is `#[repr(C)]` and the offsets the loads use are asserted below against
/// `offset_of!`, so a field reordered here fails the build instead of
/// silently reading the wrong word.
///
/// # Concurrency
///
/// `identity` and `logical_id` are written once, by the bootstrap processor,
/// before the processor that reads them runs: `logical_id` when the runtime
/// is constructed, `identity` when [`build_boot_context`] has settled the
/// runtimes at their final addresses. Every read of them is the owning
/// processor reading its own anchor through its own `IA32_FS_BASE`.
/// `guest_stack_limit` follows [`GUEST_STACK_LIMIT_OFFSET`]'s contract: the
/// owning processor writes and reads it, any processor may store the
/// interrupt sentinel into it.
#[repr(C)]
pub(crate) struct ProcessorAnchor {
    /// Offset 0: the identity this processor answers a critical-section
    /// acquire with — its own runtime address once that runtime is installed,
    /// or a tagged local-APIC id while it is still bootstrapping. Never zero
    /// once the anchor is reachable through `IA32_FS_BASE`.
    identity: AtomicUsize,
    /// Offset 8: the logical processor index, or
    /// [`BOOTSTRAP_ANCHOR_LOGICAL_ID`] on the bootstrap anchor.
    logical_id: u16,
    _reserved: [u16; 3],
    /// Offset 16: the stack limit of the guest running on this processor,
    /// which compiled guest code compares `rsp` against as
    /// `fs:[GUEST_STACK_LIMIT_OFFSET]`. Wasmtime owns its contents through
    /// `wasmtime_processor_stack_limit`.
    guest_stack_limit: AtomicUsize,
}

/// Byte offset of [`ProcessorAnchor::identity`] from `IA32_FS_BASE`.
const ANCHOR_IDENTITY_OFFSET: usize = 0;
/// Byte offset of [`ProcessorAnchor::logical_id`] from `IA32_FS_BASE`.
const ANCHOR_LOGICAL_ID_OFFSET: usize = 8;

const _: () = assert!(
    core::mem::offset_of!(ProcessorRuntime, anchor) == 0,
    "the processor anchor must sit at IA32_FS_BASE itself"
);
const _: () = assert!(
    core::mem::offset_of!(ProcessorAnchor, identity) == ANCHOR_IDENTITY_OFFSET,
    "the fs-relative identity load would read the wrong word"
);
const _: () = assert!(
    core::mem::offset_of!(ProcessorAnchor, logical_id) == ANCHOR_LOGICAL_ID_OFFSET,
    "the fs-relative logical-id load would read the wrong word"
);
const _: () = assert!(
    core::mem::offset_of!(ProcessorAnchor, guest_stack_limit) == GUEST_STACK_LIMIT_OFFSET as usize,
    "compiled guest code would compare its stack pointer against the wrong word"
);

#[repr(C)]
pub(crate) struct ProcessorRuntime {
    anchor: ProcessorAnchor,
    physical_memory_offset: usize,
    tsc_hz: u64,
    apic_mode: LocalApicMode,
    pub(crate) wasmtime_tls: WasmtimeTlsSlots,
    pub(crate) exception_idt: ProcessorIdt,
    /// The GDT and TSS this processor loads beside its IDT, and the
    /// exception and interrupt stacks the TSS names.
    pub(crate) segments: ProcessorSegments,
    /// Whether an external interrupt is being dispatched on this
    /// processor's interrupt stack, which holds exactly one frame at a
    /// time. Written by this processor only, from
    /// `exceptions::helios_x86_interrupt_dispatch`.
    pub(crate) interrupt_in_progress: AtomicBool,
    /// The page the boot-time page-fault probe in
    /// `exceptions::verify_page_fault_returns` expects to fault on; zero
    /// when no probe is running. Written by this processor only.
    pub(crate) probe_fault: AtomicUsize,
    /// The zeroed frame the probe staged for the page-fault dispatcher
    /// to map at `probe_fault`, as its direct-map address; zero when
    /// none is staged. Written by this processor only.
    pub(crate) probe_frame: AtomicUsize,
    watchdog: X86Watchdog,
    timer: Once<Timer<crate::X86Cpu>>,
    program_service: Once<debug_state::ProgramService>,
    device_interrupts: Once<&'static DeviceInterruptRoutes>,
    local_timer_ready: AtomicBool,
    /// The instruction sampler this processor's timer feeds while
    /// instruction sampling runs.
    instruction_sampler: InstructionSampler,
    /// The periodic count of one scheduler tick, as calibrated when the
    /// timer was armed. Written by this processor only.
    scheduler_tick_count: AtomicU32,
    /// Whether the timer runs at the sampling period. Written by this
    /// processor only, from its timer interrupt.
    timer_sampling: AtomicBool,
    /// Sampling-period interrupts since the last scheduler tick. Written
    /// by this processor only, from its timer interrupt.
    samples_since_scheduler_tick: AtomicU32,
    /// Whether this processor has enabled its local APIC — the base
    /// MSR's enable bits and the spurious vector — which happens once,
    /// in [`attach_local_apic`], before the processor sends an IPI or
    /// ends an interrupt. Written by this processor only.
    local_apic_attached: AtomicBool,
    started: AtomicBool,
    /// Set by [`X86PlatformState::publish_wake`] on the *target*
    /// processor before the wake IPI goes out, and cleared by
    /// `park_current` under masked interrupts. It closes the window
    /// between the run loop finding its queue empty and the `hlt` that
    /// parks: a wake published in that window is observed instead of
    /// slept through. AArch64 carries the same latch for the same
    /// reason (`aarch64/src/lib.rs`).
    wake_pending: AtomicBool,
    /// Where this processor is in a stop and restart: [`STOP_RUNNING`],
    /// [`STOP_STOPPED`] or [`STOP_RESTARTING`]; see [`stop_current_processor`]
    /// for the handshake and which side writes each transition.
    stop_state: AtomicU8,
    /// The stack pointer `helios_x86_stop_current` saved the callee-saved
    /// context at. Written by this processor before it publishes
    /// [`STOP_STOPPED`], read by the restarter after it observes that.
    saved_stack: AtomicUsize,
}

impl ProcessorAnchor {
    /// The anchor of a runtime that has not been placed at its final address
    /// yet; [`publish_anchor_identities`] fills the identity in.
    const fn new(logical_id: u16) -> Self {
        Self {
            identity: AtomicUsize::new(0),
            logical_id,
            _reserved: [0; 3],
            guest_stack_limit: AtomicUsize::new(usize::MAX),
        }
    }
}

impl ProcessorRuntime {
    pub(crate) fn logical_id(&self) -> u16 {
        self.anchor.logical_id
    }

    /// Takes the pending wake, if there is one.
    ///
    /// Called by `park_current` with interrupts masked, so a wake
    /// published after this returns `false` arrives as a pending
    /// interrupt that the halt completes on rather than a store this
    /// processor has already stopped looking at.
    pub(crate) fn take_wake_pending(&self) -> bool {
        self.wake_pending.swap(false, Ordering::AcqRel)
    }

    /// Marks an external-interrupt dispatch as running on this
    /// processor's interrupt stack, asserting that none already is. The
    /// stack holds one frame at a time and a second dispatch would
    /// overwrite the first: every external vector arrives through an
    /// interrupt gate, which clears IF on delivery, and no handler
    /// re-enables it, so the flag may only be set once at a time. Only
    /// this processor touches it, which makes `Relaxed` the whole
    /// ordering it needs.
    pub(crate) fn begin_interrupt_dispatch(&self, vector: u64) {
        assert!(
            !self.interrupt_in_progress.swap(true, Ordering::Relaxed),
            "x86 interrupt vector {vector:#x} was delivered while an interrupt was already \
             in progress on this processor's interrupt stack"
        );
    }

    /// Marks the interrupt stack free again; see
    /// [`Self::begin_interrupt_dispatch`].
    pub(crate) fn end_interrupt_dispatch(&self) {
        self.interrupt_in_progress.store(false, Ordering::Relaxed);
    }
}

pub(crate) struct BootContext {
    platform: Arc<X86PlatformState>,
    xcr0: Xcr0,
    /// The bootstrap processor's answer to every ISA level feature, taken
    /// after it programmed XCR0. Immutable; each secondary compares its own.
    isa_answers: ProbeAnswers,
}

pub(crate) struct X86PlatformState {
    tsc_base: u64,
    timebase: Timebase,
    physical_memory_offset: usize,
    debug_state: debug_state::RuntimeState,
    watchdog: X86Watchdog,
    io_apic: IoApic,
    processors: Box<[ProcessorSlot]>,
    wakeup_page: Option<WakeupPage>,
    boot_context: AtomicPtr<BootContext>,
    /// Serialises use of the one wakeup page: a start or restart patches
    /// it, sends INIT/SIPI and holds this until the target has read the
    /// patched words, so two launches never overwrite each other's.
    wakeup_lock: spin::Mutex<()>,
}

struct ProcessorSlot {
    apic_id: u32,
    runtime: ProcessorRuntime,
    stack_top: usize,
}

struct WakeupPage {
    physical_start: usize,
    virtual_start: usize,
}

#[derive(Clone)]
pub(crate) struct PhysicalOffsetAcpiHandler {
    pub(crate) physical_memory_offset: usize,
    pub(crate) tsc_base: u64,
    pub(crate) timebase: Timebase,
}

pub(crate) fn build_boot_context(
    rsdp_address: usize,
    physical_memory_offset: usize,
    wakeup_page: Option<Range<usize>>,
    tsc_base: u64,
    timebase: Timebase,
    debug_state: debug_state::RuntimeState,
    xcr0: Xcr0,
) -> &'static BootContext {
    let tsc_hz = timebase.frequency();
    let handler = PhysicalOffsetAcpiHandler {
        physical_memory_offset,
        tsc_base,
        timebase,
    };
    let tables = unsafe { AcpiTables::from_rsdp(handler.clone(), rsdp_address) }
        .unwrap_or_else(|error| panic!("failed to parse ACPI tables: {error:?}"));
    let watchdog = crate::watchdog::discover(&tables, physical_memory_offset);
    // Probed before the MADT is consumed: the I/O APIC's destination
    // encoding depends on which local-APIC mode the machine runs.
    let apic_mode = LocalApicMode::probe();
    let io_apic = crate::ioapic::discover(&tables, physical_memory_offset, apic_mode);
    let platform = AcpiPlatform::new(tables, handler.clone())
        .unwrap_or_else(|error| panic!("failed to construct ACPI platform info: {error:?}"));
    let processor_info = platform
        .processor_info
        .as_ref()
        .unwrap_or_else(|| panic!("ACPI platform info did not expose processor topology"));

    let mut processors =
        alloc::vec::Vec::with_capacity(1 + processor_info.application_processors.len());
    processors.push(ProcessorSlot {
        apic_id: processor_info.boot_processor.local_apic_id,
        runtime: ProcessorRuntime {
            anchor: ProcessorAnchor::new(0),
            physical_memory_offset,
            tsc_hz,
            apic_mode,
            wasmtime_tls: WasmtimeTlsSlots::new(),
            exception_idt: ProcessorIdt::new(),
            segments: ProcessorSegments::new(
                exception_stack(),
                exception_stack(),
                exception_stack(),
            ),
            interrupt_in_progress: AtomicBool::new(false),
            probe_fault: AtomicUsize::new(0),
            probe_frame: AtomicUsize::new(0),
            watchdog: watchdog.clone(),
            timer: Once::new(),
            program_service: Once::new(),
            device_interrupts: Once::new(),
            local_timer_ready: AtomicBool::new(false),
            instruction_sampler: debug_state.instruction_sampler(),
            scheduler_tick_count: AtomicU32::new(0),
            timer_sampling: AtomicBool::new(false),
            samples_since_scheduler_tick: AtomicU32::new(0),
            local_apic_attached: AtomicBool::new(false),
            started: AtomicBool::new(false),
            wake_pending: AtomicBool::new(false),
            stop_state: AtomicU8::new(STOP_RUNNING),
            saved_stack: AtomicUsize::new(0),
        },
        stack_top: 0,
    });
    for (index, processor) in processor_info.application_processors.iter().enumerate() {
        assert!(
            processor.state != ProcessorState::Disabled,
            "ACPI exposed disabled application processor apic_id={}",
            processor.local_apic_id
        );
        let stack = allocate_aligned_zeroed(KERNEL_STACK_BYTES, 16);
        processors.push(ProcessorSlot {
            apic_id: processor.local_apic_id,
            runtime: ProcessorRuntime {
                anchor: ProcessorAnchor::new((index + 1) as u16),
                physical_memory_offset,
                tsc_hz,
                apic_mode,
                wasmtime_tls: WasmtimeTlsSlots::new(),
                exception_idt: ProcessorIdt::new(),
                segments: ProcessorSegments::new(
                    exception_stack(),
                    exception_stack(),
                    exception_stack(),
                ),
                interrupt_in_progress: AtomicBool::new(false),
                probe_fault: AtomicUsize::new(0),
                probe_frame: AtomicUsize::new(0),
                watchdog: watchdog.clone(),
                timer: Once::new(),
                program_service: Once::new(),
                device_interrupts: Once::new(),
                local_timer_ready: AtomicBool::new(false),
                instruction_sampler: debug_state.instruction_sampler(),
                scheduler_tick_count: AtomicU32::new(0),
                timer_sampling: AtomicBool::new(false),
                samples_since_scheduler_tick: AtomicU32::new(0),
                local_apic_attached: AtomicBool::new(false),
                started: AtomicBool::new(false),
                wake_pending: AtomicBool::new(false),
                stop_state: AtomicU8::new(STOP_RUNNING),
                saved_stack: AtomicUsize::new(0),
            },
            stack_top: stack + KERNEL_STACK_BYTES,
        });
    }
    let wakeup_page = if processors.len() > 1 {
        let wakeup_page =
            wakeup_page.unwrap_or_else(|| panic!("x86 SMP startup requires a wakeup page"));
        Some(WakeupPage {
            physical_start: wakeup_page.start,
            virtual_start: wakeup_page
                .start
                .checked_add(physical_memory_offset)
                .unwrap_or_else(|| panic!("x86 wakeup page virtual address overflow")),
        })
    } else {
        None
    };
    let mut processors = processors.into_boxed_slice();
    publish_anchor_identities(&mut processors);
    let platform = Arc::new(X86PlatformState {
        tsc_base,
        timebase,
        physical_memory_offset,
        debug_state,
        watchdog,
        io_apic,
        processors,
        wakeup_page,
        boot_context: AtomicPtr::new(core::ptr::null_mut()),
        wakeup_lock: spin::Mutex::new(()),
    });
    if let Some(wakeup_page) = platform.wakeup_page.as_ref() {
        prepare_wakeup_page(&platform, wakeup_page);
    }
    let context = Box::leak(Box::new(BootContext {
        platform,
        xcr0,
        isa_answers: crate::probe_isa_answers(),
    }));
    context
        .platform
        .boot_context
        .store(context as *const _ as *mut _, Ordering::Release);
    context
}

/// Records each runtime's own address in its anchor, once the boxed slice has
/// put every runtime where it will stay.
///
/// The identity has to be the runtime's final address — it is what
/// [`current_runtime`] dereferences and what the critical section uses as its
/// owner key — and a runtime that is still inside the `Vec` is one
/// `into_boxed_slice` away from moving.
fn publish_anchor_identities(processors: &mut [ProcessorSlot]) {
    for slot in processors {
        let address = core::ptr::from_ref(&slot.runtime) as usize;
        *slot.runtime.anchor.identity.get_mut() = address;
    }
}

/// Points the bootstrap processor's `IA32_FS_BASE` at [`BOOTSTRAP_ANCHOR`].
///
/// Called as the first statement of the kernel's Rust entry point, because
/// every line after it may read `fs:0`. The anchor carries a tagged local-APIC
/// id rather than a runtime address: the ACPI tables that name this processor,
/// and the heap the runtimes live in, do not exist yet, and a bootstrapping
/// identity is exactly what [`ProcessorIdentity`] has for that.
pub(crate) fn install_bootstrap_anchor() {
    let identity = ProcessorIdentity::bootstrapping(current_apic_id() as usize);
    BOOTSTRAP_ANCHOR
        .identity
        .store(identity.raw(), Ordering::Relaxed);
    unsafe {
        wrmsr(IA32_FS_BASE, core::ptr::from_ref(&BOOTSTRAP_ANCHOR) as u64);
    }
}

pub(crate) fn activate_runtime(runtime: &ProcessorRuntime) {
    unsafe {
        wrmsr(IA32_FS_BASE, runtime as *const _ as u64);
    }
    runtime.started.store(true, Ordering::Release);
}

/// Adds the calling processor to the set every TLB shootdown waits on.
///
/// Called immediately before the processor enables interrupts, once it
/// can take the shootdown IPI, and never earlier; the contract is on the
/// statics above. A processor has touched no user page it did not unmap
/// itself before this point, so joining late leaves no translation
/// unflushed.
pub(crate) fn join_shootdown_targets() {
    let bit = processor_bit(usize::from(current_processor().id()));
    ONLINE_PROCESSOR_MASK.fetch_or(bit, Ordering::AcqRel);
}

// Stopping and restarting a processor.
//
// A processor's `stop_state` moves through three values, each transition
// written by exactly one side:
//
//   STOP_RUNNING    --(departing, `halt_stopped`)-->  STOP_STOPPED
//   STOP_STOPPED    --(restarter, compare-exchange)-> STOP_RESTARTING
//   STOP_RESTARTING --(restarted, `resume_current_rust`)--> STOP_RUNNING
//
// `stop_current_processor` runs on the departing processor with the
// kernel's teardown done. It masks the local APIC timer, leaves the
// shootdown target set under `TLB_SHOOTDOWN_LOCK` (with interrupts still
// enabled, so a broadcast that already counts it as a target is
// acknowledged while the lock is awaited), masks interrupts, pushes the
// callee-saved registers, records the stack pointer in `saved_stack`, and
// only then publishes `STOP_STOPPED` and halts with interrupts masked.
// Everything the resumed processor needs is therefore in memory before a
// restarter can observe `STOP_STOPPED`, and a restart that arrives
// earlier waits for it (`X86PlatformState::restart_processor`); one that
// arrives between the publication and the `hlt` is harmless, because the
// processor executes nothing but the halt loop by then and INIT resets it
// wherever it is.
//
// The restarter sends INIT/SIPI through the same wakeup page a first
// start uses, entering `resume_current_rust` on the stack just below the
// saved context. That function re-seeds what INIT reset — XCR0, the local
// APIC, GDT, TSS and IDT (`IA32_FS_BASE` was written by the trampoline) —
// rejoins the shootdown set after a full local TLB flush, publishes
// `STOP_RUNNING` and restores the saved MXCSR, x87 control word and
// registers, which returns from
// `stop_current_processor` on the original stack. The local APIC timer is
// left disarmed: the kernel's `set_deadline` arms it again, the way it
// armed it the first time.

/// The processor is running kernel code, or has not stopped yet.
const STOP_RUNNING: u8 = 0;
/// The processor saved its context and is, or is about to be, halted.
const STOP_STOPPED: u8 = 1;
/// A restarter claimed the stop and has sent, or is sending, INIT/SIPI.
const STOP_RESTARTING: u8 = 2;

global_asm!(include_str!("stop.S"));

unsafe extern "C" {
    /// Saves the callee-saved context, stores its stack pointer through
    /// the first argument and halts through [`helios_x86_halt_stopped`];
    /// returns when [`helios_x86_resume_current`] resumes it.
    fn helios_x86_stop_current(saved_stack: *mut usize, stop_state: *const AtomicU8);
    /// Pops the context saved at the given stack pointer and returns into
    /// the caller of [`helios_x86_stop_current`].
    fn helios_x86_resume_current(saved_stack: usize) -> !;
}

/// Takes the executing processor out of service until
/// [`X86PlatformState::restart_processor`] restarts it, and returns once
/// it has, on the same stack. Interrupts are enabled again on return.
///
/// # Concurrency
///
/// Runs on the departing processor only, with interrupts enabled and no
/// lock held. The handshake is described above; the shootdown set is the
/// only state shared with other processors besides `stop_state`.
pub(crate) fn stop_current_processor() {
    assert!(
        x86_64::instructions::interrupts::are_enabled(),
        "x86 stop_current was entered with interrupts masked: leaving the shootdown set \
         would not be able to acknowledge a broadcast that already counts this processor"
    );
    let runtime = current_runtime();
    assert_eq!(
        runtime.stop_state.load(Ordering::Relaxed),
        STOP_RUNNING,
        "x86 processor {} stopped while already stopped",
        runtime.logical_id()
    );
    runtime.mask_local_timer();
    leave_shootdown_targets();
    x86_64::instructions::interrupts::disable();
    compiler_fence(Ordering::SeqCst);
    // SAFETY: the context saved here is popped by `resume_current_rust` on
    // this same processor, on this same stack, which nothing else touches
    // while the processor is stopped.
    unsafe {
        helios_x86_stop_current(runtime.saved_stack.as_ptr(), &runtime.stop_state);
    }
    x86_64::instructions::interrupts::enable();
}

/// Publishes the stop and halts, on the stack under the saved context.
/// Never returns: a restart resets the processor and enters
/// [`resume_current_rust`] instead.
#[unsafe(no_mangle)]
extern "C" fn helios_x86_halt_stopped(stop_state: *const AtomicU8) -> ! {
    // SAFETY: `stop_current_processor` passes its own runtime's stop word,
    // which lives as long as the processor table.
    let stop_state = unsafe { &*stop_state };
    stop_state.store(STOP_STOPPED, Ordering::Release);
    loop {
        // SAFETY: interrupts are masked, so only NMI, SMI and INIT end the
        // halt, and INIT is the restart.
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack));
        }
    }
}

/// The entry a restarted processor runs, called by the wakeup trampoline
/// on the stack under the saved context, with interrupts masked and
/// `IA32_FS_BASE` already addressing this processor's anchor.
pub(crate) extern "C" fn resume_current_rust(
    boot: *const BootContext,
    runtime: *const ProcessorRuntime,
) -> ! {
    let boot = unsafe { &*boot };
    let runtime = unsafe { &*runtime };
    crate::extended_state::enable_on_secondary_processor(boot.xcr0());
    // The trampoline has read every patched word by now, so the wakeup
    // page is free for the next launch.
    assert_eq!(
        runtime.stop_state.swap(STOP_RUNNING, Ordering::AcqRel),
        STOP_RESTARTING,
        "x86 processor {} resumed without a restart claiming its stop",
        runtime.logical_id()
    );
    runtime.reset_local_apic_state();
    runtime.attach_local_apic();
    crate::exceptions::install_for_current_processor();
    rejoin_shootdown_targets();
    let saved_stack = runtime.saved_stack.load(Ordering::Acquire);
    // SAFETY: `saved_stack` is where `helios_x86_stop_current` pushed this
    // processor's context; nothing has run on the stack above it since.
    unsafe { helios_x86_resume_current(saved_stack) }
}

/// Removes the calling processor from the set every TLB shootdown waits
/// on, under the lock a broadcast holds from publication to its last
/// acknowledgement. Taking the lock waits out a broadcast that already
/// counted this processor, which is acknowledged meanwhile because the
/// caller's interrupts are enabled; once it is held, no broadcast is in
/// flight and none that starts later targets this processor.
fn leave_shootdown_targets() {
    let bit = processor_bit(usize::from(current_processor().id()));
    let _broadcast = TLB_SHOOTDOWN_LOCK.lock();
    ONLINE_PROCESSOR_MASK.fetch_and(!bit, Ordering::AcqRel);
}

/// Joins the shootdown set again after a stop, with a full local TLB
/// flush done under the lock: translations this processor cached while
/// it was stopped or restarting were not covered by any broadcast, and
/// every broadcast that starts after the lock is released counts it.
/// The processor is not a target while it takes the lock with interrupts
/// masked, so it cannot hold up a broadcaster.
fn rejoin_shootdown_targets() {
    let bit = processor_bit(usize::from(current_processor().id()));
    let _broadcast = TLB_SHOOTDOWN_LOCK.lock();
    tlb::flush_all();
    ONLINE_PROCESSOR_MASK.fetch_or(bit, Ordering::AcqRel);
}

/// The identity word at `fs:0`.
///
/// # Safety of the load
///
/// `IA32_FS_BASE` addresses a [`ProcessorAnchor`] on every processor from
/// before its first Rust instruction: the bootstrap processor installs
/// [`BOOTSTRAP_ANCHOR`] in [`install_bootstrap_anchor`], and an application
/// processor's runtime address is written by the wakeup trampoline
/// (`secondary_wakeup.S`) before it calls into Rust at all.
#[inline(always)]
fn anchor_identity_word() -> usize {
    let word: usize;
    unsafe {
        core::arch::asm!(
            "mov {word}, fs:[{offset}]",
            word = out(reg) word,
            offset = const ANCHOR_IDENTITY_OFFSET,
            options(nostack, preserves_flags, readonly),
        );
    }
    word
}

/// The logical processor index at `fs:8`; see [`anchor_identity_word`] for why
/// the load is always addressable.
#[inline(always)]
fn anchor_logical_id() -> u16 {
    let logical_id: u16;
    unsafe {
        core::arch::asm!(
            "mov {logical_id:x}, fs:[{offset}]",
            logical_id = out(reg) logical_id,
            offset = const ANCHOR_LOGICAL_ID_OFFSET,
            options(nostack, preserves_flags, readonly),
        );
    }
    logical_id
}

pub(crate) fn current_runtime() -> &'static ProcessorRuntime {
    let runtime = current_identity()
        .runtime_address()
        .unwrap_or_else(|| panic!("x86 processor runtime was not installed before use"));
    unsafe { &*(runtime.get() as *const ProcessorRuntime) }
}

/// The executing processor's guest stack-limit word.
///
/// Read through the runtime rather than `fs` because Wasmtime needs the
/// word's address, which an `fs`-relative load cannot produce. A guest only
/// runs once this processor's runtime is installed.
pub(crate) fn current_guest_stack_limit() -> &'static AtomicUsize {
    &current_runtime().anchor.guest_stack_limit
}

pub(crate) fn current_processor() -> ProcessorId {
    let logical_id = anchor_logical_id();
    assert!(
        logical_id != BOOTSTRAP_ANCHOR_LOGICAL_ID,
        "x86 processor runtime was not installed before its logical id was asked for"
    );
    ProcessorId::new(logical_id)
}

/// The identity this processor answers critical-section acquires with.
///
/// A processor takes critical sections from its first instruction, well before
/// [`activate_runtime`] publishes its runtime address, and several application
/// processors run that prologue concurrently. Every processor therefore carries
/// an anchor from the start: the bootstrap processor's holds a tagged local-APIC
/// id until its runtime exists, and an application processor's is its runtime,
/// installed by the wakeup trampoline. Sharing one value across processors would
/// make a second processor's acquire look like the first processor's nested
/// re-acquire.
pub(crate) fn current_identity() -> ProcessorIdentity {
    let word = NonZeroUsize::new(anchor_identity_word())
        .unwrap_or_else(|| panic!("x86 processor anchor was read before it carried an identity"));
    ProcessorIdentity::from_raw(word)
}

/// The local APIC id of the executing processor, from the topology leaf when
/// the CPU publishes one and the legacy 8-bit initial APIC id otherwise.
fn current_apic_id() -> u32 {
    let max_basic_leaf = __cpuid(0).eax;
    if max_basic_leaf >= 0x0b {
        let topology = __cpuid(0x0b);
        return topology.edx;
    }

    __cpuid(1).ebx >> 24
}

impl ProcessorRuntime {
    pub(crate) fn install_timer(&self, timer: Timer<crate::X86Cpu>) {
        assert!(
            self.timer.get().is_none(),
            "x86 processor timer was installed more than once"
        );
        self.timer.call_once(|| timer);
    }

    pub(crate) fn timer(&self) -> &Timer<crate::X86Cpu> {
        self.timer
            .get()
            .expect("x86 processor timer was read before it was installed")
    }

    pub(crate) fn install_program_service(&self, service: debug_state::ProgramService) {
        assert!(
            self.program_service.get().is_none(),
            "x86 processor program service was installed more than once"
        );
        self.program_service.call_once(|| service);
    }

    pub(crate) fn ensure_local_timer(&self, vector: u8) {
        if self
            .local_timer_ready
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            enable_local_scheduler_timer(vector);
        }
    }

    /// The instruction-sampling half of a timer interrupt. Answers whether
    /// the interrupt is also a scheduler tick.
    ///
    /// Outside sampling every interrupt is a scheduler tick. A scheduler
    /// tick that finds sampling started shortens the period to
    /// [`SAMPLES_PER_SCHEDULER_TICK`] interrupts per tick; from then on
    /// each interrupt records the interrupted address, and every
    /// [`SAMPLES_PER_SCHEDULER_TICK`]th is the scheduler tick, which
    /// restores the period once sampling has stopped. The period changes
    /// only on a scheduler tick, so the scheduler's cadence never shifts.
    fn sample_timer_tick(&self, interrupted: usize) -> bool {
        if !self.timer_sampling.load(Ordering::Relaxed) {
            if self.instruction_sampler.sampling() {
                self.samples_since_scheduler_tick
                    .store(0, Ordering::Relaxed);
                self.timer_sampling.store(true, Ordering::Relaxed);
                self.set_timer_count(
                    self.scheduler_tick_count.load(Ordering::Relaxed) / SAMPLES_PER_SCHEDULER_TICK,
                );
            }
            return true;
        }
        self.instruction_sampler.record(interrupted);
        let samples = self.samples_since_scheduler_tick.load(Ordering::Relaxed) + 1;
        if samples < SAMPLES_PER_SCHEDULER_TICK {
            self.samples_since_scheduler_tick
                .store(samples, Ordering::Relaxed);
            return false;
        }
        self.samples_since_scheduler_tick
            .store(0, Ordering::Relaxed);
        if !self.instruction_sampler.sampling() {
            self.timer_sampling.store(false, Ordering::Relaxed);
            self.set_timer_count(self.scheduler_tick_count.load(Ordering::Relaxed));
        }
        true
    }

    /// Rewrites this processor's periodic initial count, which restarts
    /// the countdown at `count`.
    fn set_timer_count(&self, count: u32) {
        assert!(count != 0, "x86 local APIC timer period would be zero");
        match self.apic_mode {
            LocalApicMode::X2Apic => unsafe {
                wrmsr(IA32_X2APIC_INIT_COUNT, u64::from(count));
            },
            LocalApicMode::XApic { physical_base } => {
                let apic_region = xapic_mmio_region(physical_base, self.physical_memory_offset);
                write_xapic_register(apic_region, XAPIC_TIMER_INIT_COUNT, count);
            }
        }
    }

    /// Masks this processor's timer interrupt and stops its countdown.
    /// The calibrated period survives in `scheduler_tick_count`, so
    /// [`enable_local_scheduler_timer`] re-arms without calibrating again.
    fn mask_local_timer(&self) {
        match self.apic_mode {
            LocalApicMode::X2Apic => unsafe {
                wrmsr(
                    IA32_X2APIC_LVT_TIMER,
                    rdmsr(IA32_X2APIC_LVT_TIMER) | APIC_TIMER_MASKED,
                );
                wrmsr(IA32_X2APIC_INIT_COUNT, 0);
            },
            LocalApicMode::XApic { physical_base } => {
                let apic_region = xapic_mmio_region(physical_base, self.physical_memory_offset);
                let lvt = u64::from(read_xapic_register(apic_region, XAPIC_LVT_TIMER))
                    | APIC_TIMER_MASKED;
                write_xapic_register(apic_region, XAPIC_LVT_TIMER, lvt as u32);
                write_xapic_register(apic_region, XAPIC_TIMER_INIT_COUNT, 0);
            }
        }
    }

    /// Forgets the local APIC and timer state a stop invalidated, so the
    /// next [`Self::attach_local_apic`] and `set_deadline` bring both up
    /// again. Runs on this processor, after its restart.
    fn reset_local_apic_state(&self) {
        self.local_apic_attached.store(false, Ordering::Relaxed);
        self.local_timer_ready.store(false, Ordering::Relaxed);
        self.timer_sampling.store(false, Ordering::Relaxed);
        self.samples_since_scheduler_tick
            .store(0, Ordering::Relaxed);
        self.wake_pending.store(false, Ordering::Relaxed);
    }

    fn platform_tsc_hz(&self) -> u64 {
        self.tsc_hz
    }

    fn apic_mode(&self) -> LocalApicMode {
        self.apic_mode
    }

    /// Enables this processor's local APIC, once. Every later ICR
    /// write, EOI and timer program assumes it; a processor that sends
    /// a wake before this ran would write an ICR the hardware has not
    /// enabled, which is why the entry paths call it before interrupts
    /// are enabled rather than leaving it to the first timer arm.
    pub(crate) fn attach_local_apic(&self) {
        if self
            .local_apic_attached
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        match self.apic_mode {
            LocalApicMode::X2Apic => {
                let mut apic = X2APIC::new();
                apic.attach();
                unsafe {
                    wrmsr(IA32_X2APIC_ESR, 0);
                    wrmsr(
                        IA32_X2APIC_LVT_ERROR,
                        u64::from(crate::exceptions::APIC_ERROR_INTERRUPT_VECTOR),
                    );
                }
            }
            LocalApicMode::XApic { physical_base } => {
                let apic_region = xapic_mmio_region(physical_base, self.physical_memory_offset);
                attach_xapic(apic_region);
            }
        }
    }
}

impl BootContext {
    pub(crate) fn platform(&self) -> Arc<X86PlatformState> {
        self.platform.clone()
    }

    /// The XCR0 the bootstrap processor programmed; see
    /// [`crate::extended_state::enable_on_secondary_processor`].
    pub(crate) fn xcr0(&self) -> Xcr0 {
        self.xcr0
    }

    /// The bootstrap processor's ISA level feature answers; see
    /// [`crate::probe_isa_answers`].
    pub(crate) fn isa_answers(&self) -> ProbeAnswers {
        self.isa_answers
    }

    pub(crate) fn bootstrap_runtime(&self) -> &ProcessorRuntime {
        &self.platform.processors[0].runtime
    }
}

impl X86PlatformState {
    pub(crate) fn tsc_base(&self) -> u64 {
        self.tsc_base
    }

    pub(crate) fn timebase(&self) -> Timebase {
        self.timebase
    }

    pub(crate) fn debug_state(&self) -> debug_state::RuntimeState {
        self.debug_state.clone()
    }

    pub(crate) fn watchdog(&self) -> X86Watchdog {
        self.watchdog.clone()
    }

    pub(crate) fn processor_count(&self) -> usize {
        self.processors.len()
    }

    /// Publishes the device interrupt routes every processor dispatches
    /// MSI-X vectors through.
    ///
    /// One table, shared: a device's configuration message is addressed
    /// to the bootstrap processor, but a multi-queue network device
    /// steers each queue pair's message to the processor whose shard
    /// drains that pair, so any processor can be the one an MSI-X
    /// vector lands on. Which queue a vector belongs to does not depend
    /// on who fields it, so every processor reads the same table.
    ///
    /// # SMP contract
    ///
    /// The table is built once, while the bootstrap processor is the
    /// only one online and its own interrupts are still masked, and it
    /// is borrowed for the life of the machine. Every processor slot
    /// the ACPI topology described already exists by then, so a
    /// processor started later reads the table its slot was handed
    /// before it ran its first instruction; the interrupt path only
    /// loads a pointer, taking no lock and no allocation.
    pub(crate) fn install_device_interrupts(&self, routes: DeviceInterruptRoutes) {
        let secondaries_online = ONLINE_PROCESSOR_MASK.load(Ordering::Acquire)
            & !processor_bit(usize::from(self.bootstrap_processor().id()));
        assert!(
            secondaries_online == 0,
            "x86 device interrupt routes must be published before any secondary \
             processor is online; online mask {secondaries_online:#b}"
        );
        let routes: &'static DeviceInterruptRoutes = Box::leak(Box::new(routes));
        for slot in self.processors.iter() {
            assert!(
                slot.runtime.device_interrupts.get().is_none(),
                "x86 device interrupt routes were installed more than once"
            );
            slot.runtime.device_interrupts.call_once(|| routes);
        }
    }

    /// The I/O APIC carrying COM1's line, located from the MADT at boot.
    pub(crate) fn io_apic(&self) -> &IoApic {
        &self.io_apic
    }

    pub(crate) fn bootstrap_processor(&self) -> ProcessorId {
        ProcessorId::new(0)
    }

    /// Look up the local-APIC id of the processor with the given
    /// logical id. Used by the wake / TLB-shootdown IPI dispatch path
    /// to address a remote core through the local APIC ICR.
    pub(crate) fn apic_id_of(&self, processor: ProcessorId) -> Option<u32> {
        self.processors
            .get(processor.id() as usize)
            .map(|slot| slot.apic_id)
    }

    /// Publishes a wake for `processor` and answers with the local-APIC
    /// id to signal, or `None` when no such processor is configured.
    ///
    /// The store lands before the caller sends the IPI. A target on its
    /// way into `park_current` either observes the flag in the masked
    /// test there, or takes an interrupt that is already pending when
    /// its halt begins; either way the wake is not lost.
    pub(crate) fn publish_wake(&self, processor: ProcessorId) -> Option<u32> {
        let slot = self.processors.get(processor.id() as usize)?;
        slot.runtime.wake_pending.store(true, Ordering::Release);
        Some(slot.apic_id)
    }

    pub(crate) fn start_processor(&self, processor: ProcessorId, entry: usize) {
        let slot = self.processor_slot(processor);
        assert!(
            processor != self.bootstrap_processor(),
            "bootstrap processor cannot be started twice"
        );
        let _launch = self.wakeup_lock.lock();
        assert!(
            !slot.runtime.started.load(Ordering::Acquire),
            "processor {} was started more than once",
            processor.id()
        );
        self.launch_processor(processor, slot.stack_top, entry, || {
            slot.runtime.started.load(Ordering::Acquire)
        });
    }

    /// Restarts a processor [`stop_current_processor`] took out of
    /// service, so that its `stop_current` returns.
    ///
    /// Waits for the processor to publish [`STOP_STOPPED`], which closes
    /// the window between the kernel's "stopping" and the hardware halt:
    /// until then the processor is still running the stop path, and an
    /// INIT would reset it mid-way. Past that point it only executes
    /// `cli; hlt`, so the INIT/SIPI lands harmlessly whenever it arrives.
    /// The transition to [`STOP_RESTARTING`] is a compare-exchange, so a
    /// second restart for the same stop panics instead of sending a
    /// second SIPI. The wakeup page is entered on the stack just below
    /// the saved context, which is where [`resume_current_rust`] finds it.
    pub(crate) fn restart_processor(&self, processor: ProcessorId, entry: usize) {
        let slot = self.processor_slot(processor);
        assert!(
            processor != self.bootstrap_processor(),
            "bootstrap processor cannot be restarted: it never stops"
        );
        let _launch = self.wakeup_lock.lock();
        let deadline = read_tsc()
            .checked_add(self.timebase.frequency() / 2)
            .unwrap_or_else(|| panic!("x86 AP restart deadline overflow"));
        while slot.runtime.stop_state.load(Ordering::Acquire) != STOP_STOPPED {
            assert!(
                slot.runtime.stop_state.load(Ordering::Acquire) != STOP_RESTARTING
                    && read_tsc() <= deadline,
                "x86 processor {} was restarted twice for one stop, or never reached its halt",
                processor.id()
            );
            core::hint::spin_loop();
        }
        slot.runtime
            .stop_state
            .compare_exchange(
                STOP_STOPPED,
                STOP_RESTARTING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .unwrap_or_else(|state| {
                panic!(
                    "x86 processor {} was restarted twice for one stop (state {state})",
                    processor.id()
                )
            });
        // The context starts at `saved_stack`, which `stop.S` leaves
        // 16-aligned; the trampoline's stack grows down from there, so
        // its `call` is aligned and every saved word stays untouched.
        let stack_top = slot.runtime.saved_stack.load(Ordering::Acquire);
        assert!(
            stack_top.is_multiple_of(16),
            "x86 processor {} saved its stop context at {stack_top:#x}, not 16-aligned",
            processor.id()
        );
        self.launch_processor(processor, stack_top, entry, || {
            slot.runtime.stop_state.load(Ordering::Acquire) == STOP_RUNNING
        });
    }

    /// Patches the wakeup page and sends INIT/SIPI to `processor`, then
    /// waits until `reached_rust` says it has run its entry point, which
    /// is after the trampoline read every patched word. The caller holds
    /// `wakeup_lock`.
    fn launch_processor(
        &self,
        processor: ProcessorId,
        stack_top: usize,
        entry: usize,
        reached_rust: impl Fn() -> bool,
    ) {
        let slot = self.processor_slot(processor);
        let wakeup_page = self
            .wakeup_page
            .as_ref()
            .unwrap_or_else(|| panic!("x86 startup state is missing the AP wakeup page"));

        patch_wakeup_page(
            wakeup_page.virtual_start,
            current_cr3(),
            stack_top,
            self.boot_context_ptr(),
            &slot.runtime as *const _ as usize,
            entry,
        );

        unsafe {
            wake_application_processor(
                slot.apic_id,
                wakeup_page.physical_start,
                self.physical_memory_offset,
                self.timebase.frequency(),
            )
            .unwrap_or_else(|message| {
                panic!(
                    "failed to wake x86 processor {} apic_id={}: {message}",
                    processor.id(),
                    slot.apic_id
                )
            });
        }

        let deadline = read_tsc()
            .checked_add(self.timebase.frequency() / 2)
            .unwrap_or_else(|| panic!("x86 AP startup deadline overflow"));
        while !reached_rust() {
            assert!(
                read_tsc() <= deadline,
                "x86 processor {} did not reach Rust startup",
                processor.id()
            );
            core::hint::spin_loop();
        }
    }

    fn processor_slot(&self, processor: ProcessorId) -> &ProcessorSlot {
        self.processors
            .get(processor.id() as usize)
            .unwrap_or_else(|| panic!("x86 processor {} is out of range", processor.id()))
    }

    fn boot_context_ptr(&self) -> usize {
        let pointer = self.boot_context.load(Ordering::Acquire);
        assert!(
            !pointer.is_null(),
            "x86 boot context pointer was not installed before SMP startup"
        );
        pointer as usize
    }
}

/// How this machine's local APICs are addressed.
///
/// Decided once, on the bootstrap processor, from CPUID and the APIC
/// base MSR, and carried in every processor's runtime: both reads are
/// VM exits under KVM, and the answer cannot change after boot. Every
/// interrupt end and every wake IPI used to re-ask — a `cpuid`, an MSR
/// read and, for a send, a full re-attach of the local APIC (two more
/// MSR writes) — so a cross-processor wake cost five exits where the
/// ICR write is the one that does anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocalApicMode {
    XApic { physical_base: usize },
    X2Apic,
}

impl LocalApicMode {
    /// Probes the mode of the local APIC this processor runs on. The
    /// mode is a property of the machine, so the bootstrap processor
    /// probes it once and every runtime carries the answer.
    fn probe() -> Self {
        let cpuid = __cpuid(1);
        if cpuid.ecx & (1 << 21) != 0 {
            return Self::X2Apic;
        }
        let base = unsafe { rdmsr(x86::msr::IA32_APIC_BASE) };
        if base & (1 << 10) != 0 {
            return Self::X2Apic;
        }
        Self::XApic {
            physical_base: (base as usize) & 0xffff_f000,
        }
    }

    /// The xAPIC destination for `apic_id`, which the 8-bit destination
    /// field of an xAPIC ICR has to be able to name.
    pub(crate) fn xapic_target(apic_id: u32) -> u8 {
        u8::try_from(apic_id)
            .unwrap_or_else(|_| panic!("x86 target apic id {apic_id} requires x2APIC support"))
    }
}

pub(crate) fn ensure_local_scheduler_timer(vector: u8) {
    let runtime = current_runtime();
    runtime.ensure_local_timer(vector);
}

/// What a processor's timer captures, for the kernel's instruction
/// sampler: one sample per sampling-period interrupt.
pub(crate) fn instruction_sample_capture(kernel_image_base: u64) -> SampleCapture {
    SampleCapture {
        period_nanos: APIC_TIMER_CALIBRATION_MICROS * 1_000 / u64::from(SAMPLES_PER_SCHEDULER_TICK),
        kernel_image_base,
    }
}

/// Handles this processor's timer interrupt, which interrupted the
/// instruction at `interrupted`.
pub(crate) fn handle_local_timer_interrupt(interrupted: usize) {
    let runtime = current_runtime();
    if !runtime.sample_timer_tick(interrupted) {
        local_apic_eoi();
        return;
    }
    runtime.watchdog.pet();
    // Every processor's tick ends the slice of every guest on the machine
    // that has run one, so the slice does not need its own interrupt.
    if let Some(service) = runtime.program_service.get() {
        service.preempt_overdue_guests();
    }
    runtime
        .timer
        .get()
        .unwrap_or_else(|| panic!("x86 local timer interrupted before kernel timer installation"))
        .handle_interrupt();
    local_apic_eoi();
}

/// Dispatches an MSI-X device interrupt to the driver bound to `vector`.
///
/// Runs on whichever processor the message was steered to; the routes
/// its runtime holds are the one table every processor was handed
/// during device bring-up.
pub(crate) fn handle_device_interrupt(vector: u8) {
    let runtime = current_runtime();
    let routes = runtime.device_interrupts.get().unwrap_or_else(|| {
        panic!("x86 device interrupt vector {vector:#x} fired before routes were installed")
    });
    assert!(
        routes.route(vector),
        "x86 device interrupt vector {vector:#x} has no registered handler"
    );
    local_apic_eoi();
}

pub(crate) fn handle_wake_interrupt() {
    // The wake carries no payload: returning from HLT is the whole
    // message, and `park_current` owns the flag the sender published in
    // `X86PlatformState::publish_wake`. Just ack and return.
    local_apic_eoi();
}

pub(crate) fn shootdown_tlb_range(start: usize, byte_len: usize) {
    if byte_len == 0 {
        return;
    }
    let current = usize::from(current_processor().id());
    let current_bit = processor_bit(current);
    if ONLINE_PROCESSOR_MASK.load(Ordering::Acquire) & !current_bit == 0 {
        return;
    }
    assert!(
        ONLINE_PROCESSOR_MASK.load(Ordering::Acquire) & current_bit == 0
            || x86_64::instructions::interrupts::are_enabled(),
        "x86 TLB shootdown broadcast by processor {current} with interrupts masked while \
         it is itself a shootdown target: it could not acknowledge another processor's \
         broadcast, and the two would wait on each other"
    );

    let _broadcast = TLB_SHOOTDOWN_LOCK.lock();
    let targets = ONLINE_PROCESSOR_MASK.load(Ordering::Acquire) & !current_bit;
    if targets == 0 {
        return;
    }
    TLB_SHOOTDOWN_START.store(start, Ordering::Release);
    TLB_SHOOTDOWN_LEN.store(byte_len, Ordering::Release);
    TLB_SHOOTDOWN_ACK_MASK.store(0, Ordering::Release);
    send_tlb_shootdown_ipi_all_excluding_self();
    while TLB_SHOOTDOWN_ACK_MASK.load(Ordering::Acquire) & targets != targets {
        core::hint::spin_loop();
    }
}

/// Invalidates `byte_len` bytes from `start` on this processor, one
/// `INVLPG` per page.
///
/// Always per page, never a `CR3` reload: an unmap is one task's
/// memory going away while the other processors run other tasks, and a
/// whole-TLB flush on them evicts those tasks' translations too. Under
/// nested paging a refill is a two-dimensional walk, and the paired
/// lane priced it — a reload past a 33-page ceiling (Linux's
/// `tlb_single_page_flush_ceiling`, tuned for native refills on the
/// same address space) cost `fs-smallfiles` its stability and
/// `stdio-pipe` 17 % on run 35384279049, while the per-page loop keeps
/// the cost inside the range being torn down.
pub(crate) fn flush_tlb_range_local(start: usize, byte_len: usize) {
    for offset in (0..byte_len).step_by(PAGE_BYTES) {
        tlb::flush(VirtAddr::new((start + offset) as u64));
    }
}

pub(crate) fn handle_tlb_shootdown_interrupt() {
    let start = TLB_SHOOTDOWN_START.load(Ordering::Acquire);
    let byte_len = TLB_SHOOTDOWN_LEN.load(Ordering::Acquire);
    flush_tlb_range_local(start, byte_len);
    let bit = processor_bit(usize::from(current_processor().id()));
    TLB_SHOOTDOWN_ACK_MASK.fetch_or(bit, Ordering::AcqRel);
    local_apic_eoi();
}

/// Sends `icr` through the x2APIC ICR, which is the whole of an x2APIC
/// IPI: x2APIC mode has no delivery-status bit to poll, and APIC errors
/// arrive through the LVT error interrupt
/// ([`handle_apic_error_interrupt`]) rather than by polling the ESR.
/// Under a hypervisor every x2APIC MSR write is a VM exit, so the
/// `x86` crate's `X2APIC::send_ipi`, which writes the ESR twice first,
/// would triple the cost of each IPI.
///
/// A WRMSR to an x2APIC register is not serializing, so the fence makes
/// the stores the receiver reads (the shootdown range, a woken task)
/// visible before the interrupt is sent.
unsafe fn send_x2apic_ipi(icr: x86::apic::Icr) {
    let value = (u64::from(icr.upper()) << 32) | u64::from(icr.lower());
    unsafe {
        core::arch::asm!("mfence", "lfence", options(nostack, preserves_flags));
        wrmsr(IA32_X2APIC_ICR, value);
    }
}

/// Send a wake IPI to the processor with the given local-APIC id.
///
/// The receiver runs the wake interrupt handler (`local_apic_eoi`-only)
/// and falls out of HLT in `Cpu::park_current`, ready to pick up
/// newly runnable kernel tasks. The function picks the appropriate
/// APIC mode (X2APIC / XAPIC) and constructs the corresponding ICR
/// internally.
pub(crate) fn send_wake_ipi(target_apic_id: u32) {
    use x86::apic::{
        ApicControl, ApicId, DeliveryMode, DeliveryStatus, DestinationMode, DestinationShorthand,
        Icr, Level, TriggerMode,
    };
    let runtime = current_runtime();
    debug_assert!(
        runtime.local_apic_attached.load(Ordering::Acquire),
        "x86 wake IPI sent before this processor attached its local APIC"
    );
    match runtime.apic_mode() {
        LocalApicMode::X2Apic => {
            let icr = Icr::for_x2apic(
                crate::exceptions::WAKE_INTERRUPT_VECTOR,
                ApicId::X2Apic(target_apic_id),
                DestinationShorthand::NoShorthand,
                DeliveryMode::Fixed,
                DestinationMode::Physical,
                DeliveryStatus::Idle,
                Level::Assert,
                TriggerMode::Edge,
            );
            unsafe { send_x2apic_ipi(icr) };
        }
        LocalApicMode::XApic { physical_base } => {
            let apic_region = xapic_mmio_region(physical_base, runtime.physical_memory_offset);
            let mut apic = XAPIC::new(apic_region);
            let icr = Icr::for_xapic(
                crate::exceptions::WAKE_INTERRUPT_VECTOR,
                ApicId::XApic(LocalApicMode::xapic_target(target_apic_id)),
                DestinationShorthand::NoShorthand,
                DeliveryMode::Fixed,
                DestinationMode::Physical,
                DeliveryStatus::Idle,
                Level::Assert,
                TriggerMode::Edge,
            );
            unsafe { apic.send_ipi(icr) };
        }
    }
}

fn send_tlb_shootdown_ipi_all_excluding_self() {
    use x86::apic::{
        ApicControl, ApicId, DeliveryMode, DeliveryStatus, DestinationMode, DestinationShorthand,
        Icr, Level, TriggerMode,
    };
    let runtime = current_runtime();
    match runtime.apic_mode() {
        LocalApicMode::X2Apic => {
            let icr = Icr::for_x2apic(
                crate::exceptions::TLB_SHOOTDOWN_INTERRUPT_VECTOR,
                ApicId::X2Apic(0),
                DestinationShorthand::AllExcludingSelf,
                DeliveryMode::Fixed,
                DestinationMode::Physical,
                DeliveryStatus::Idle,
                Level::Assert,
                TriggerMode::Edge,
            );
            unsafe { send_x2apic_ipi(icr) };
        }
        LocalApicMode::XApic { physical_base } => {
            let apic_region = xapic_mmio_region(physical_base, runtime.physical_memory_offset);
            let mut apic = XAPIC::new(apic_region);
            let icr = Icr::for_xapic(
                crate::exceptions::TLB_SHOOTDOWN_INTERRUPT_VECTOR,
                ApicId::XApic(0),
                DestinationShorthand::AllExcludingSelf,
                DeliveryMode::Fixed,
                DestinationMode::Physical,
                DeliveryStatus::Idle,
                Level::Assert,
                TriggerMode::Edge,
            );
            unsafe { apic.send_ipi(icr) };
        }
    }
}

fn processor_bit(processor: usize) -> usize {
    assert!(
        processor < usize::BITS as usize,
        "x86 TLB shootdown supports at most {} online processors; got processor {}",
        usize::BITS,
        processor
    );
    1usize << processor
}

fn enable_local_scheduler_timer(vector: u8) {
    let runtime = current_runtime();
    runtime.attach_local_apic();
    // A processor restarted after a stop knows its period already; the
    // calibration also programs the divide configuration INIT reset.
    let calibrated = runtime.scheduler_tick_count.load(Ordering::Relaxed);
    match runtime.apic_mode() {
        LocalApicMode::X2Apic => {
            let initial_count = if calibrated != 0 {
                unsafe {
                    wrmsr(IA32_X2APIC_DIV_CONF, u64::from(APIC_TIMER_DIVIDE_BY_ONE));
                }
                calibrated
            } else {
                calibrate_x2apic_timer(runtime.platform_tsc_hz())
            };
            runtime
                .scheduler_tick_count
                .store(initial_count, Ordering::Relaxed);
            program_x2apic_periodic_timer(vector, initial_count);
        }
        LocalApicMode::XApic { physical_base } => {
            let apic_region = xapic_mmio_region(physical_base, runtime.physical_memory_offset);
            let initial_count = if calibrated != 0 {
                write_xapic_register(apic_region, XAPIC_TIMER_DIV_CONF, APIC_TIMER_DIVIDE_BY_ONE);
                calibrated
            } else {
                calibrate_xapic_timer(apic_region, runtime.platform_tsc_hz())
            };
            runtime
                .scheduler_tick_count
                .store(initial_count, Ordering::Relaxed);
            program_xapic_periodic_timer(apic_region, vector, initial_count);
        }
    }
}

fn attach_xapic(apic_region: &[u32]) {
    unsafe {
        let mut base = rdmsr(IA32_APIC_BASE);
        base |= 1 << 11;
        wrmsr(IA32_APIC_BASE, base);
    }
    write_xapic_register(apic_region, XAPIC_SVR, (1 << 8) | 15);
    write_xapic_register(apic_region, XAPIC_ESR, 0);
    write_xapic_register(
        apic_region,
        XAPIC_LVT_ERROR,
        u32::from(crate::exceptions::APIC_ERROR_INTERRUPT_VECTOR),
    );
}

fn calibrate_x2apic_timer(tsc_hz: u64) -> u32 {
    unsafe {
        wrmsr(IA32_X2APIC_DIV_CONF, u64::from(APIC_TIMER_DIVIDE_BY_ONE));
        wrmsr(
            IA32_X2APIC_LVT_TIMER,
            APIC_TIMER_MASKED | u64::from(crate::exceptions::TIMER_INTERRUPT_VECTOR),
        );
        wrmsr(IA32_X2APIC_INIT_COUNT, u64::from(u32::MAX));
        stall_microseconds(tsc_hz, APIC_TIMER_CALIBRATION_MICROS);
        let current = rdmsr(IA32_X2APIC_CUR_COUNT) as u32;
        wrmsr(IA32_X2APIC_INIT_COUNT, 0);
        timer_elapsed_count(current)
    }
}

fn calibrate_xapic_timer(apic_region: &[u32], tsc_hz: u64) -> u32 {
    write_xapic_register(apic_region, XAPIC_TIMER_DIV_CONF, APIC_TIMER_DIVIDE_BY_ONE);
    write_xapic_register(
        apic_region,
        XAPIC_LVT_TIMER,
        (APIC_TIMER_MASKED | u64::from(crate::exceptions::TIMER_INTERRUPT_VECTOR)) as u32,
    );
    write_xapic_register(apic_region, XAPIC_TIMER_INIT_COUNT, u32::MAX);
    stall_microseconds(tsc_hz, APIC_TIMER_CALIBRATION_MICROS);
    let current = read_xapic_register(apic_region, XAPIC_TIMER_CURRENT_COUNT);
    write_xapic_register(apic_region, XAPIC_TIMER_INIT_COUNT, 0);
    timer_elapsed_count(current)
}

fn timer_elapsed_count(current: u32) -> u32 {
    let elapsed = u32::MAX.saturating_sub(current);
    assert!(
        elapsed != 0,
        "x86 local APIC timer did not tick during calibration"
    );
    elapsed
}

fn program_x2apic_periodic_timer(vector: u8, initial_count: u32) {
    unsafe {
        let lvt = (rdmsr(IA32_X2APIC_LVT_TIMER)
            & !(APIC_TIMER_VECTOR_MASK | APIC_TIMER_MASKED | APIC_TIMER_MODE_MASK))
            | u64::from(vector)
            | APIC_TIMER_PERIODIC;
        wrmsr(IA32_X2APIC_LVT_TIMER, lvt);
        wrmsr(IA32_X2APIC_INIT_COUNT, u64::from(initial_count));
    }
}

fn program_xapic_periodic_timer(apic_region: &[u32], vector: u8, initial_count: u32) {
    let lvt = (u64::from(read_xapic_register(apic_region, XAPIC_LVT_TIMER))
        & !(APIC_TIMER_VECTOR_MASK | APIC_TIMER_MASKED | APIC_TIMER_MODE_MASK))
        | u64::from(vector)
        | APIC_TIMER_PERIODIC;
    write_xapic_register(apic_region, XAPIC_LVT_TIMER, lvt as u32);
    write_xapic_register(apic_region, XAPIC_TIMER_INIT_COUNT, initial_count);
}

bitflags::bitflags! {
    /// The local APIC error status register.
    #[derive(Debug, Clone, Copy)]
    struct ApicErrors: u32 {
        const SEND_CHECKSUM = 1 << 0;
        const RECEIVE_CHECKSUM = 1 << 1;
        const SEND_ACCEPT = 1 << 2;
        const RECEIVE_ACCEPT = 1 << 3;
        const REDIRECTABLE_IPI = 1 << 4;
        const SEND_ILLEGAL_VECTOR = 1 << 5;
        const RECEIVE_ILLEGAL_VECTOR = 1 << 6;
        const ILLEGAL_REGISTER_ADDRESS = 1 << 7;
    }
}

/// Reports local-APIC errors by latching and reading ESR; IPI sends do not poll it.
pub(crate) fn handle_apic_error_interrupt() -> ! {
    let runtime = current_runtime();
    let esr = match runtime.apic_mode() {
        LocalApicMode::X2Apic => unsafe {
            wrmsr(IA32_X2APIC_ESR, 0);
            rdmsr(IA32_X2APIC_ESR) as u32
        },
        LocalApicMode::XApic { physical_base } => {
            let apic_region = xapic_mmio_region(physical_base, runtime.physical_memory_offset);
            write_xapic_register(apic_region, XAPIC_ESR, 0);
            read_xapic_register(apic_region, XAPIC_ESR)
        }
    };
    let id = current_processor().id();
    panic!(
        "x86 local APIC error interrupt on processor {id}: ESR={esr:#x} {errors:?}",
        errors = ApicErrors::from_bits_retain(esr)
    );
}

fn read_xapic_register(apic_region: &[u32], offset: u32) -> u32 {
    let index = (offset / 4) as usize;
    unsafe { core::ptr::read_volatile(&apic_region[index]) }
}

fn write_xapic_register(apic_region: &[u32], offset: u32, value: u32) {
    let index = (offset / 4) as usize;
    unsafe {
        core::ptr::write_volatile(apic_region.as_ptr().add(index) as *mut u32, value);
    }
}

fn local_apic_eoi() {
    let runtime = current_runtime();
    match runtime.apic_mode() {
        LocalApicMode::X2Apic => {
            let mut apic = X2APIC::new();
            apic.eoi();
        }
        LocalApicMode::XApic { physical_base } => {
            let apic_region = xapic_mmio_region(physical_base, runtime.physical_memory_offset);
            let mut apic = XAPIC::new(apic_region);
            apic.eoi();
        }
    }
}

unsafe fn wake_application_processor(
    apic_id: u32,
    wakeup_page_physical: usize,
    physical_memory_offset: usize,
    tsc_hz: u64,
) -> Result<(), &'static str> {
    assert_eq!(
        wakeup_page_physical & (WAKEUP_PAGE_BYTES - 1),
        0,
        "x86 wakeup page must stay 4KiB aligned"
    );
    assert!(
        wakeup_page_physical < SIPI_MAX_PHYSICAL_ADDRESS,
        "x86 wakeup page must stay below 1MiB for SIPI"
    );
    let startup_vector = u8::try_from(wakeup_page_physical >> 12)
        .unwrap_or_else(|_| panic!("x86 SIPI vector exceeded 8 bits"));

    let runtime = current_runtime();
    runtime.attach_local_apic();
    match runtime.apic_mode() {
        LocalApicMode::X2Apic => {
            let mut apic = X2APIC::new();
            send_startup_ipis(&mut apic, ApicId::X2Apic(apic_id), startup_vector, tsc_hz);
        }
        LocalApicMode::XApic { physical_base } => {
            let apic_region = xapic_mmio_region(physical_base, physical_memory_offset);
            let mut apic = XAPIC::new(apic_region);
            let apic_id = u8::try_from(apic_id).map_err(|_| "xAPIC target id exceeded 8 bits")?;
            send_startup_ipis(&mut apic, ApicId::XApic(apic_id), startup_vector, tsc_hz);
        }
    }

    Ok(())
}

fn send_startup_ipis(
    apic: &mut impl ApicControl,
    apic_id: ApicId,
    startup_vector: u8,
    tsc_hz: u64,
) {
    unsafe {
        apic.ipi_init(apic_id);
    }
    stall_microseconds(tsc_hz, AP_STARTUP_INIT_ASSERT_MICROS);
    unsafe {
        apic.ipi_init_deassert();
    }
    stall_microseconds(tsc_hz, AP_STARTUP_INTER_IPI_MICROS);
    unsafe {
        apic.ipi_startup(apic_id, startup_vector);
    }
    stall_microseconds(tsc_hz, AP_STARTUP_INTER_IPI_MICROS);
    unsafe {
        apic.ipi_startup(apic_id, startup_vector);
    }
}

fn stall_microseconds(tsc_hz: u64, microseconds: u64) {
    let ticks = tsc_hz.saturating_mul(microseconds) / 1_000_000;
    let deadline = read_tsc().saturating_add(ticks);
    while read_tsc() < deadline {
        core::hint::spin_loop();
    }
}

fn xapic_mmio_region(physical_base: usize, physical_memory_offset: usize) -> &'static mut [u32] {
    // The local APIC page is MMIO, not RAM: it is absent from the boot
    // memory map, so the HHDM does not cover it until it is mapped here.
    let virtual_base = map_mmio_window(physical_memory_offset, physical_base, WAKEUP_PAGE_BYTES);
    unsafe { core::slice::from_raw_parts_mut(virtual_base as *mut u32, WAKEUP_PAGE_BYTES / 4) }
}

impl PhysicalOffsetAcpiHandler {
    fn physical_to_virtual<T>(&self, physical_address: usize) -> NonNull<T> {
        let virtual_address = physical_address
            .checked_add(self.physical_memory_offset)
            .unwrap_or_else(|| panic!("physical mapping overflow at {physical_address:#x}"));
        NonNull::new(virtual_address as *mut T)
            .unwrap_or_else(|| panic!("physical mapping produced a null virtual pointer"))
    }
}

impl Handler for PhysicalOffsetAcpiHandler {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> RawPhysicalMapping<T> {
        ensure_hhdm_range_mapped(self.physical_memory_offset, physical_address, size);
        // SAFETY: the virtual address is the direct-map alias of the
        // physical range, which no other mapping of this handler aliases
        // mutably, and `unmap_physical_region` has nothing to undo.
        unsafe {
            RawPhysicalMapping::new(
                physical_address,
                self.physical_to_virtual(physical_address),
                size,
                size,
            )
        }
    }

    unsafe fn unmap_physical_region<T>(&self, _region: RawPhysicalMapping<T>) {}

    fn read_u8(&self, address: usize) -> u8 {
        unsafe { (address as *const u8).read_volatile() }
    }

    fn read_u16(&self, address: usize) -> u16 {
        unsafe { (address as *const u16).read_volatile() }
    }

    fn read_u32(&self, address: usize) -> u32 {
        unsafe { (address as *const u32).read_volatile() }
    }

    fn read_u64(&self, address: usize) -> u64 {
        unsafe { (address as *const u64).read_volatile() }
    }

    fn write_u8(&self, address: usize, value: u8) {
        unsafe { (address as *mut u8).write_volatile(value) }
    }

    fn write_u16(&self, address: usize, value: u16) {
        unsafe { (address as *mut u16).write_volatile(value) }
    }

    fn write_u32(&self, address: usize, value: u32) {
        unsafe { (address as *mut u32).write_volatile(value) }
    }

    fn write_u64(&self, address: usize, value: u64) {
        unsafe { (address as *mut u64).write_volatile(value) }
    }

    fn read_io_u8(&self, port: u16) -> u8 {
        unsafe { x86_64::instructions::port::PortReadOnly::<u8>::new(port).read() }
    }

    fn read_io_u16(&self, port: u16) -> u16 {
        unsafe { x86_64::instructions::port::PortReadOnly::<u16>::new(port).read() }
    }

    fn read_io_u32(&self, port: u16) -> u32 {
        unsafe { x86_64::instructions::port::PortReadOnly::<u32>::new(port).read() }
    }

    fn write_io_u8(&self, port: u16, value: u8) {
        unsafe { x86_64::instructions::port::PortWriteOnly::<u8>::new(port).write(value) }
    }

    fn write_io_u16(&self, port: u16, value: u16) {
        unsafe { x86_64::instructions::port::PortWriteOnly::<u16>::new(port).write(value) }
    }

    fn write_io_u32(&self, port: u16, value: u32) {
        unsafe { x86_64::instructions::port::PortWriteOnly::<u32>::new(port).write(value) }
    }

    fn read_pci_u8(&self, address: PciAddress, offset: u16) -> u8 {
        LegacyPciConfigAccess::new().read_u8(address, offset)
    }

    fn read_pci_u16(&self, address: PciAddress, offset: u16) -> u16 {
        LegacyPciConfigAccess::new().read_u16(address, offset)
    }

    fn read_pci_u32(&self, address: PciAddress, offset: u16) -> u32 {
        unsafe { LegacyPciConfigAccess::new().read(address, offset) }
    }

    fn write_pci_u8(&self, address: PciAddress, offset: u16, value: u8) {
        LegacyPciConfigAccess::new().write_u8(address, offset, value)
    }

    fn write_pci_u16(&self, address: PciAddress, offset: u16, value: u16) {
        LegacyPciConfigAccess::new().write_u16(address, offset, value)
    }

    fn write_pci_u32(&self, address: PciAddress, offset: u16, value: u32) {
        unsafe { LegacyPciConfigAccess::new().write(address, offset, value) }
    }

    fn nanos_since_boot(&self) -> u64 {
        let ticks = read_tsc().saturating_sub(self.tsc_base);
        self.timebase.ticks_to_nanos(ticks)
    }

    fn stall(&self, microseconds: u64) {
        let ticks = self.timebase.frequency().saturating_mul(microseconds) / 1_000_000;
        let deadline = read_tsc().saturating_add(ticks);
        while read_tsc() < deadline {
            core::hint::spin_loop();
        }
    }

    fn sleep(&self, milliseconds: u64) {
        self.stall(milliseconds.saturating_mul(1_000));
    }

    /// AML mutexes need no state here, and the handle is the same for
    /// every one of them.
    ///
    /// A mutex keeps two threads of control out of the same AML region.
    /// The only AML interpreter this backend builds (`vmgenid`) is
    /// created, used and dropped on the bootstrap processor during
    /// bring-up, before any secondary starts and before the executor
    /// exists, so it has one thread of control: every acquire succeeds at
    /// once, recursive acquires included, and a release has nothing to
    /// hand over. [`Self::acquire`] asserts that rather than assuming it,
    /// so an interpreter that ever ran elsewhere fails here instead of
    /// silently losing mutual exclusion.
    fn create_mutex(&self) -> acpi::Handle {
        acpi::Handle(0)
    }

    fn acquire(&self, _mutex: acpi::Handle, _timeout: u16) -> Result<(), acpi::aml::AmlError> {
        assert!(
            current_processor() == ProcessorId::new(0),
            "the AML interpreter ran outside the bootstrap processor, where its mutexes would \
             no longer provide mutual exclusion"
        );
        Ok(())
    }

    fn release(&self, _mutex: acpi::Handle) {}
}

fn prepare_wakeup_page(platform: &X86PlatformState, wakeup_page: &WakeupPage) {
    identity_map_wakeup_page(platform.physical_memory_offset, wakeup_page.physical_start);
    let bytes = wakeup_image();
    assert!(
        bytes.len() <= WAKEUP_PAGE_BYTES,
        "x86 wakeup image exceeded one page: {} bytes",
        bytes.len()
    );
    unsafe {
        core::ptr::write_bytes(wakeup_page.virtual_start as *mut u8, 0, WAKEUP_PAGE_BYTES);
        core::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            wakeup_page.virtual_start as *mut u8,
            bytes.len(),
        );
    }
}

fn patch_wakeup_page(
    wakeup_virtual_start: usize,
    cr3: u64,
    stack_top: usize,
    boot_context: usize,
    runtime: usize,
    entry: usize,
) {
    write_u64_patch(wakeup_virtual_start, wakeup_cr3_offset(), cr3);
    write_u64_patch(
        wakeup_virtual_start,
        wakeup_stack_top_offset(),
        stack_top as u64,
    );
    write_u64_patch(
        wakeup_virtual_start,
        wakeup_context_offset(),
        boot_context as u64,
    );
    write_u64_patch(
        wakeup_virtual_start,
        wakeup_runtime_offset(),
        runtime as u64,
    );
    write_u64_patch(wakeup_virtual_start, wakeup_entry_offset(), entry as u64);
}

fn write_u64_patch(image_start: usize, offset: usize, value: u64) {
    let address = image_start
        .checked_add(offset)
        .unwrap_or_else(|| panic!("x86 wakeup image patch overflow at offset {offset:#x}"));
    unsafe {
        (address as *mut u64).write_volatile(value);
    }
}

fn current_cr3() -> u64 {
    let (frame, _) = Cr3::read();
    frame.start_address().as_u64()
}

fn identity_map_wakeup_page(physical_memory_offset: usize, wakeup_physical: usize) {
    let page = Page::<Size4KiB>::from_start_address(VirtAddr::new(wakeup_physical as u64))
        .unwrap_or_else(|error| panic!("invalid x86 wakeup page address: {error:?}"));
    let frame = PhysFrame::<Size4KiB>::from_start_address(PhysAddr::new(wakeup_physical as u64))
        .unwrap_or_else(|error| panic!("invalid x86 wakeup frame address: {error:?}"));
    let mut mapper = unsafe { current_mapper(physical_memory_offset) };
    if mapper.translate_page(page).is_ok() {
        return;
    }
    let mut frame_allocator = DirectMappedFrameAllocator {
        physical_memory_offset,
    };
    unsafe {
        mapper
            .map_to(
                page,
                frame,
                PageTableFlags::PRESENT | PageTableFlags::WRITABLE,
                &mut frame_allocator,
            )
            .unwrap_or_else(|error| panic!("failed to identity-map x86 wakeup page: {error:?}"))
            .flush();
    }
}

pub(crate) fn map_mmio_window(
    physical_memory_offset: usize,
    physical_start: usize,
    bytes: usize,
) -> usize {
    let virtual_start = physical_memory_offset
        .checked_add(physical_start)
        .unwrap_or_else(|| panic!("x86 MMIO virtual address overflow at {physical_start:#x}"));
    let page_offset = virtual_start & (PAGE_BYTES - 1);
    let map_start = align_down(virtual_start, PAGE_BYTES);
    let map_bytes = align_up(
        page_offset
            .checked_add(bytes)
            .unwrap_or_else(|| panic!("x86 MMIO mapping size overflow")),
        PAGE_BYTES,
    );
    let mut frame_allocator = DirectMappedFrameAllocator {
        physical_memory_offset,
    };
    let mut mapper = unsafe { current_mapper(physical_memory_offset) };

    for virtual_address in (map_start..map_start + map_bytes).step_by(PAGE_BYTES) {
        let page = Page::<Size4KiB>::from_start_address(VirtAddr::new(virtual_address as u64))
            .unwrap_or_else(|error| panic!("invalid x86 MMIO virtual page: {error:?}"));
        if mapper.translate_page(page).is_ok() {
            let entry =
                ensure_direct_mapped_leaf_entry(physical_memory_offset, page, &mut frame_allocator);
            entry.set_flags(
                entry.flags() | PageTableFlags::NO_CACHE | PageTableFlags::WRITE_THROUGH,
            );
            tlb::flush(VirtAddr::new(virtual_address as u64));
            continue;
        }

        let physical_address = virtual_address
            .checked_sub(physical_memory_offset)
            .unwrap_or_else(|| {
                panic!("x86 MMIO virtual address {virtual_address:#x} was outside the HHDM")
            });
        let frame =
            PhysFrame::<Size4KiB>::from_start_address(PhysAddr::new(physical_address as u64))
                .unwrap_or_else(|error| panic!("invalid x86 MMIO physical frame: {error:?}"));
        unsafe {
            mapper
                .map_to(
                    page,
                    frame,
                    PageTableFlags::PRESENT
                        | PageTableFlags::WRITABLE
                        | PageTableFlags::NO_CACHE
                        | PageTableFlags::WRITE_THROUGH,
                    &mut frame_allocator,
                )
                .unwrap_or_else(|error| panic!("failed to map x86 MMIO page: {error:?}"))
                .flush();
        }
    }

    virtual_start
}

pub(crate) fn ensure_hhdm_range_mapped(
    physical_memory_offset: usize,
    physical_start: usize,
    bytes: usize,
) {
    let page_offset = physical_start & (PAGE_BYTES - 1);
    let map_start = align_down(physical_start, PAGE_BYTES);
    let map_bytes = align_up(
        page_offset
            .checked_add(bytes.max(1))
            .unwrap_or_else(|| panic!("x86 HHDM mapping size overflow")),
        PAGE_BYTES,
    );
    let mut mapper = unsafe { current_mapper(physical_memory_offset) };
    let mut frame_allocator = DirectMappedFrameAllocator {
        physical_memory_offset,
    };

    for physical_address in (map_start..map_start + map_bytes).step_by(PAGE_BYTES) {
        let virtual_address = physical_memory_offset
            .checked_add(physical_address)
            .unwrap_or_else(|| {
                panic!("x86 HHDM virtual address overflow at {physical_address:#x}")
            });
        let page = Page::<Size4KiB>::from_start_address(VirtAddr::new(virtual_address as u64))
            .unwrap_or_else(|error| panic!("invalid x86 HHDM virtual page: {error:?}"));
        if mapper.translate_page(page).is_ok() {
            continue;
        }
        let frame =
            PhysFrame::<Size4KiB>::from_start_address(PhysAddr::new(physical_address as u64))
                .unwrap_or_else(|error| panic!("invalid x86 HHDM physical frame: {error:?}"));
        unsafe {
            mapper
                .map_to(
                    page,
                    frame,
                    PageTableFlags::PRESENT | PageTableFlags::WRITABLE,
                    &mut frame_allocator,
                )
                .unwrap_or_else(|error| panic!("failed to map x86 HHDM page: {error:?}"))
                .flush();
        }
    }
}

fn ensure_direct_mapped_leaf_entry(
    physical_memory_offset: usize,
    page: Page<Size4KiB>,
    frame_allocator: &mut DirectMappedFrameAllocator,
) -> &mut PageTableEntry {
    let p4 = current_level_4_table(physical_memory_offset);
    let p3 = next_table_mut(
        physical_memory_offset,
        &mut p4[page.p4_index()],
        "x86 MMIO direct-map P4 entry",
    );
    let p2 = next_or_split_p3_table(
        physical_memory_offset,
        &mut p3[page.p3_index()],
        frame_allocator,
    );
    let p1 = next_or_split_p2_table(
        physical_memory_offset,
        &mut p2[page.p2_index()],
        frame_allocator,
    );
    let entry = &mut p1[page.p1_index()];
    assert!(
        !entry.is_unused(),
        "x86 MMIO direct-map leaf entry was unexpectedly unmapped"
    );
    entry
}

fn current_level_4_table(physical_memory_offset: usize) -> &'static mut PageTable {
    let (level_4, _) = Cr3::read();
    let virtual_address = level_4
        .start_address()
        .as_u64()
        .checked_add(physical_memory_offset as u64)
        .unwrap_or_else(|| panic!("x86 level-4 page table virtual address overflow"));
    unsafe { &mut *(virtual_address as *mut PageTable) }
}

fn next_table_mut<'a>(
    physical_memory_offset: usize,
    entry: &'a mut PageTableEntry,
    context: &str,
) -> &'a mut PageTable {
    assert!(!entry.is_unused(), "{context} was unexpectedly unmapped");
    assert!(
        !entry.flags().contains(PageTableFlags::HUGE_PAGE),
        "{context} unexpectedly mapped a huge page"
    );
    let virtual_address = entry
        .addr()
        .as_u64()
        .checked_add(physical_memory_offset as u64)
        .unwrap_or_else(|| panic!("{context} virtual address overflow"));
    unsafe { &mut *(virtual_address as *mut PageTable) }
}

fn next_or_split_p3_table<'a>(
    physical_memory_offset: usize,
    entry: &'a mut PageTableEntry,
    frame_allocator: &mut DirectMappedFrameAllocator,
) -> &'a mut PageTable {
    if entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return split_p3_huge_page(physical_memory_offset, entry, frame_allocator);
    }
    next_table_mut(
        physical_memory_offset,
        entry,
        "x86 MMIO direct-map P3 entry",
    )
}

fn next_or_split_p2_table<'a>(
    physical_memory_offset: usize,
    entry: &'a mut PageTableEntry,
    frame_allocator: &mut DirectMappedFrameAllocator,
) -> &'a mut PageTable {
    if entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return split_p2_huge_page(physical_memory_offset, entry, frame_allocator);
    }
    next_table_mut(
        physical_memory_offset,
        entry,
        "x86 MMIO direct-map P2 entry",
    )
}

fn split_p3_huge_page<'a>(
    physical_memory_offset: usize,
    entry: &'a mut PageTableEntry,
    frame_allocator: &mut DirectMappedFrameAllocator,
) -> &'a mut PageTable {
    let original_flags = entry.flags();
    let base = entry.addr().as_u64();
    let (frame, table) = allocate_page_table(physical_memory_offset, frame_allocator);
    for (index, child_entry) in table.iter_mut().enumerate() {
        let address = base
            .checked_add((index as u64) * Size2MiB::SIZE)
            .unwrap_or_else(|| panic!("x86 split 1GiB direct-map overflow"));
        child_entry.set_addr(
            PhysAddr::new(address),
            original_flags | PageTableFlags::HUGE_PAGE,
        );
    }
    entry.set_frame(frame, parent_table_flags(original_flags));
    table
}

fn split_p2_huge_page<'a>(
    physical_memory_offset: usize,
    entry: &'a mut PageTableEntry,
    frame_allocator: &mut DirectMappedFrameAllocator,
) -> &'a mut PageTable {
    let original_flags = entry.flags();
    let leaf_flags = original_flags & !PageTableFlags::HUGE_PAGE;
    let base = entry.addr().as_u64();
    let (frame, table) = allocate_page_table(physical_memory_offset, frame_allocator);
    for (index, child_entry) in table.iter_mut().enumerate() {
        let address = base
            .checked_add((index as u64) * Size4KiB::SIZE)
            .unwrap_or_else(|| panic!("x86 split 2MiB direct-map overflow"));
        let frame = PhysFrame::<Size4KiB>::from_start_address(PhysAddr::new(address))
            .unwrap_or_else(|error| panic!("invalid x86 split 2MiB frame address: {error:?}"));
        child_entry.set_frame(frame, leaf_flags);
    }
    entry.set_frame(frame, parent_table_flags(original_flags));
    table
}

fn allocate_page_table(
    physical_memory_offset: usize,
    frame_allocator: &mut DirectMappedFrameAllocator,
) -> (PhysFrame<Size4KiB>, &'static mut PageTable) {
    let frame = frame_allocator
        .allocate_frame()
        .unwrap_or_else(|| panic!("failed to allocate x86 page-table frame for MMIO split"));
    let virtual_address = frame
        .start_address()
        .as_u64()
        .checked_add(physical_memory_offset as u64)
        .unwrap_or_else(|| panic!("x86 page-table frame virtual address overflow"));
    let table = unsafe { &mut *(virtual_address as *mut PageTable) };
    table.zero();
    (frame, table)
}

fn parent_table_flags(flags: PageTableFlags) -> PageTableFlags {
    let mut parent = PageTableFlags::PRESENT;
    if flags.contains(PageTableFlags::WRITABLE) {
        parent |= PageTableFlags::WRITABLE;
    }
    if flags.contains(PageTableFlags::USER_ACCESSIBLE) {
        parent |= PageTableFlags::USER_ACCESSIBLE;
    }
    if flags.contains(PageTableFlags::WRITE_THROUGH) {
        parent |= PageTableFlags::WRITE_THROUGH;
    }
    if flags.contains(PageTableFlags::NO_CACHE) {
        parent |= PageTableFlags::NO_CACHE;
    }
    parent
}

pub(crate) unsafe fn current_mapper(physical_memory_offset: usize) -> OffsetPageTable<'static> {
    let (level_4, _) = Cr3::read();
    let virtual_address = level_4
        .start_address()
        .as_u64()
        .checked_add(physical_memory_offset as u64)
        .unwrap_or_else(|| panic!("x86 level-4 page table virtual address overflow"));
    let table = unsafe { &mut *(virtual_address as *mut PageTable) };
    unsafe { OffsetPageTable::new(table, VirtAddr::new(physical_memory_offset as u64)) }
}

pub(crate) struct DirectMappedFrameAllocator {
    pub(crate) physical_memory_offset: usize,
}

unsafe impl FrameAllocator<Size4KiB> for DirectMappedFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        let layout = Layout::from_size_align(Size4KiB::SIZE as usize, Size4KiB::SIZE as usize)
            .unwrap_or_else(|_| panic!("failed to build x86 page-table frame layout"));
        let pointer = unsafe { alloc_zeroed(layout) };
        let pointer = NonNull::new(pointer)
            .unwrap_or_else(|| panic!("failed to allocate x86 page-table frame"));
        let virtual_address = pointer.as_ptr() as usize;
        assert!(
            virtual_address >= self.physical_memory_offset,
            "page-table frame was allocated outside the physical-memory direct map"
        );
        let physical_address = virtual_address - self.physical_memory_offset;
        Some(
            PhysFrame::from_start_address(PhysAddr::new(physical_address as u64)).unwrap_or_else(
                |error| {
                    panic!("allocated x86 page-table frame had invalid physical address: {error:?}")
                },
            ),
        )
    }
}

/// One exception or interrupt stack for one processor, as the byte
/// range the TSS names the top of.
fn exception_stack() -> Range<usize> {
    let base = allocate_aligned_zeroed(EXCEPTION_STACK_BYTES, 16);
    base..base + EXCEPTION_STACK_BYTES
}

fn allocate_aligned_zeroed(size: usize, align: usize) -> usize {
    let layout = Layout::from_size_align(size, align).unwrap_or_else(|_| {
        panic!("failed to build aligned allocation layout size={size} align={align}")
    });
    let pointer = unsafe { alloc_zeroed(layout) };
    NonNull::new(pointer)
        .unwrap_or_else(|| panic!("aligned allocation of {size} bytes failed"))
        .as_ptr() as usize
}

use helios_hal::{align_down, align_up};

fn wakeup_image() -> &'static [u8] {
    unsafe {
        let start = &helios_x86_secondary_wakeup_start as *const u8 as usize;
        let end = &helios_x86_secondary_wakeup_end as *const u8 as usize;
        core::slice::from_raw_parts(start as *const u8, end - start)
    }
}

fn wakeup_cr3_offset() -> usize {
    unsafe { wakeup_symbol_offset(&helios_x86_secondary_wakeup_cr3) }
}

fn wakeup_stack_top_offset() -> usize {
    unsafe { wakeup_symbol_offset(&helios_x86_secondary_wakeup_stack_top) }
}

fn wakeup_context_offset() -> usize {
    unsafe { wakeup_symbol_offset(&helios_x86_secondary_wakeup_context) }
}

fn wakeup_runtime_offset() -> usize {
    unsafe { wakeup_symbol_offset(&helios_x86_secondary_wakeup_runtime) }
}

fn wakeup_entry_offset() -> usize {
    unsafe { wakeup_symbol_offset(&helios_x86_secondary_wakeup_entry) }
}

unsafe fn wakeup_symbol_offset(symbol: &'static u8) -> usize {
    (symbol as *const u8 as usize)
        .checked_sub(unsafe { &helios_x86_secondary_wakeup_start as *const u8 as usize })
        .unwrap_or_else(|| panic!("x86 wakeup symbol precedes image start"))
}

unsafe extern "C" {
    static helios_x86_secondary_wakeup_start: u8;
    static helios_x86_secondary_wakeup_end: u8;
    static helios_x86_secondary_wakeup_cr3: u8;
    static helios_x86_secondary_wakeup_stack_top: u8;
    static helios_x86_secondary_wakeup_context: u8;
    static helios_x86_secondary_wakeup_runtime: u8;
    static helios_x86_secondary_wakeup_entry: u8;
}
