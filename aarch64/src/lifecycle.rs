//! Taking an AArch64 processor out of service and putting it back,
//! through PSCI.
//!
//! A processor leaves with `CPU_OFF`, which never returns, and comes back
//! when another processor calls `CPU_ON`, which starts it from reset at
//! [`resume.S`](../resume.S) with the MMU off. To give the kernel the
//! contract `Cpu::stop_current` states (it returns on the same processor
//! and the same stack) the leaving processor first saves everything the
//! reset loses into a [`ResumeContext`], and the entry code puts it back:
//! the translation regime, the exception state and the callee-saved
//! registers. `stop_current` then simply returns, as `setjmp` would.
//!
//! Concurrency contract. [`ProcessorPower::stop`] runs on the departing
//! processor and [`ProcessorPower::restart`] on any other one; the two
//! meet in `state`, the only word both write. `Running` is the
//! processor in service, and `Stopping` is the departing processor having
//! filled the part of its context a restarter reads (the physical
//! addresses of the entry code and of the context). A restarter that
//! arrives in `Stopping` takes the processor to `Restarting` and polls
//! `AFFINITY_INFO` until the firmware reports it off before it issues
//! `CPU_ON`, because `CPU_ON` of a processor that has not finished
//! `CPU_OFF` fails with `ALREADY_ON`. A restarter that arrives before
//! `stop` took the processor to `Stopping` leaves `RestartPending`, and
//! the stop that finds it returns at once having stopped nothing. The
//! resumed processor moves `Restarting` back to `Running`. Nothing here
//! takes a lock, and the context is written by its own processor only
//! while the state says `Running` or `Stopping`.
//!
//! The first start of a secondary stays with Limine's MP protocol:
//! Limine's processors are already on when the kernel starts them, so
//! `CPU_ON` for one would answer `ALREADY_ON`, and its `goto_address`
//! mailbox is the only way to take them over. Only a processor the
//! kernel itself powered down is restarted through PSCI.

use alloc::alloc::{Layout, alloc_zeroed, handle_alloc_error};
use alloc::boxed::Box;
use core::arch::{asm, global_asm, naked_asm};
use core::cell::UnsafeCell;
use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use helios_hal::cpu::Timebase;
use smccc::psci::{self, AffinityState, LowestAffinityLevel};
use smccc::{Call, Hvc, Smc};

use crate::gic::Gic;
use crate::platform::PsciConduit;

const PAGE_BYTES: usize = 4096;
const BLOCK_BYTES: usize = 2 * 1024 * 1024;
const TABLE_ENTRIES: usize = 512;

/// `MPIDR_EL1` affinity fields as PSCI's `target_cpu` takes them: Aff0
/// to Aff2 in the low word, Aff3 in bits 32..40, everything else zero.
const PSCI_TARGET_MASK: u64 = 0x0000_00ff_00ff_ffff;

/// How long a restarter waits for a stopping processor to finish
/// `CPU_OFF`, in seconds. The window is a few instructions long, so
/// expiry means `CPU_OFF` failed on the other side.
const OFF_DEADLINE_SECONDS: u64 = 1;

/// A stage 1 block descriptor for the identity map: valid block, global,
/// inner shareable, access flag set, read-only at EL1 and not writable
/// from EL0, and executable.
const BLOCK_DESCRIPTOR_BASE: u64 = 0b01 | (0b10 << 6) | (0b11 << 8) | (1 << 10);
const TABLE_DESCRIPTOR: u64 = 0b11;

/// `TCR_EL1` fields of the TTBR0 half the identity map uses: a 48-bit
/// address space, 4 KiB granule, write-back write-allocate inner
/// shareable table walks, walks enabled.
const IDMAP_TCR_TTBR0_MASK: u64 = 0xffff;
const IDMAP_TCR_TTBR0: u64 = 16 | (0b01 << 8) | (0b01 << 10) | (0b11 << 12);

const RUNNING: u8 = 0;
const STOPPING: u8 = 1;
const RESTARTING: u8 = 2;
const RESTART_PENDING: u8 = 3;

global_asm!(include_str!("resume.S"));

unsafe extern "C" {
    static __helios_aarch64_cpu_resume: u8;
    static __helios_aarch64_cpu_resume_end: u8;
    static __helios_aarch64_cpu_resume_virtual: u8;
}

/// A page-sized, page-aligned translation table.
#[repr(C, align(4096))]
struct Table([u64; TABLE_ENTRIES]);

/// Everything a restart needs that `CPU_ON` does not carry.
///
/// The layout is read by `resume.S` and by [`save_and_power_off`] at the
/// offsets asserted below.
#[repr(C, align(4096))]
pub(crate) struct ResumeContext {
    /// `x19`-`x30`.
    callee_saved: [u64; 12],
    sp: u64,
    /// `d8`-`d15`, the callee-saved half of the FP/SIMD file.
    fp_callee_saved: [u64; 8],
    mair: u64,
    tcr: u64,
    ttbr0: u64,
    ttbr1: u64,
    sctlr: u64,
    cpacr: u64,
    vbar: u64,
    tpidr: u64,
    sp_el0: u64,
    fpcr: u64,
    conduit: u64,
    /// Physical address of this context, `CPU_ON`'s `context_id`.
    self_physical: u64,
    /// `TCR_EL1` the entry code runs under while TTBR0 is the identity map.
    idmap_tcr: u64,
    idmap_ttbr0: u64,
    self_virtual: u64,
    /// Virtual address of the second half of the entry code.
    continuation: u64,
    /// Physical address of the entry code, `CPU_ON`'s entry point.
    entry_physical: u64,
    idmap: [Table; 3],
}

const _: () = {
    assert!(offset_of!(ResumeContext, callee_saved) == 0);
    assert!(offset_of!(ResumeContext, sp) == 96);
    assert!(offset_of!(ResumeContext, fp_callee_saved) == 104);
    assert!(offset_of!(ResumeContext, mair) == 168);
    assert!(offset_of!(ResumeContext, tcr) == 176);
    assert!(offset_of!(ResumeContext, ttbr0) == 184);
    assert!(offset_of!(ResumeContext, ttbr1) == 192);
    assert!(offset_of!(ResumeContext, sctlr) == 200);
    assert!(offset_of!(ResumeContext, cpacr) == 208);
    assert!(offset_of!(ResumeContext, vbar) == 216);
    assert!(offset_of!(ResumeContext, tpidr) == 224);
    assert!(offset_of!(ResumeContext, sp_el0) == 232);
    assert!(offset_of!(ResumeContext, fpcr) == 240);
    assert!(offset_of!(ResumeContext, idmap_tcr) == 264);
    assert!(offset_of!(ResumeContext, idmap_ttbr0) == 272);
    assert!(offset_of!(ResumeContext, self_virtual) == 280);
    assert!(offset_of!(ResumeContext, continuation) == 288);
};

impl ResumeContext {
    /// Allocates a zeroed context straight on the heap: it is four pages
    /// and page aligned, which a by-value constructor would build on a
    /// stack first.
    fn boxed() -> Box<UnsafeCell<Self>> {
        let layout = Layout::new::<UnsafeCell<Self>>();
        // SAFETY: the layout has a non-zero size, and an all-zero bit
        // pattern is a valid `ResumeContext` because it holds integers only.
        let ptr = unsafe { alloc_zeroed(layout) }.cast::<UnsafeCell<Self>>();
        if ptr.is_null() {
            handle_alloc_error(layout);
        }
        assert!(
            ptr as usize % PAGE_BYTES == 0,
            "the AArch64 kernel heap did not honour the page alignment of a resume context"
        );
        // SAFETY: the pointer came from the global allocator with this
        // layout and holds a valid value.
        unsafe { Box::from_raw(ptr) }
    }
}

/// The PSCI conduit this platform calls through.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Psci {
    conduit: PsciConduit,
}

impl Psci {
    pub(crate) fn new(conduit: PsciConduit) -> Self {
        Self { conduit }
    }

    fn cpu_on(self, mpidr: u64, entry: u64, context: u64) -> Result<(), psci::Error> {
        fn call<C: Call>(mpidr: u64, entry: u64, context: u64) -> Result<(), psci::Error> {
            psci::cpu_on::<C>(mpidr, entry, context)
        }
        match self.conduit {
            PsciConduit::Hvc => call::<Hvc>(mpidr, entry, context),
            PsciConduit::Smc => call::<Smc>(mpidr, entry, context),
        }
    }

    fn affinity_info(self, mpidr: u64) -> Result<AffinityState, psci::Error> {
        match self.conduit {
            PsciConduit::Hvc => psci::affinity_info::<Hvc>(mpidr, LowestAffinityLevel::All),
            PsciConduit::Smc => psci::affinity_info::<Smc>(mpidr, LowestAffinityLevel::All),
        }
    }
}

/// One processor's power state and the context it resumes from.
pub(crate) struct ProcessorPower {
    state: AtomicU8,
    context: Box<UnsafeCell<ResumeContext>>,
}

// SAFETY: `state` hands the context between the processor it belongs to
// and a restarter: the restarter reads two words of it only after
// observing `Stopping`, which the owner publishes after writing them, and
// the entry code reads the rest only after the firmware reports the
// owner off.
unsafe impl Sync for ProcessorPower {}

impl ProcessorPower {
    pub(crate) fn new() -> Self {
        Self {
            state: AtomicU8::new(RUNNING),
            context: ResumeContext::boxed(),
        }
    }

    /// Powers the calling processor down and returns once it has been
    /// restarted, on the same stack.
    ///
    /// `mpidr` is the calling processor's own `MPIDR_EL1`. Returns at
    /// once, stopping nothing, when [`ProcessorPower::restart`] already
    /// ran for this stop.
    pub(crate) fn stop(&self, psci: Psci, mpidr: u64, gic: &Gic, wake_pending: &AtomicBool) {
        let daif = crate::read_daif();
        crate::mask_irq();
        let context = self.context.get();
        // SAFETY: the state is `Running`, so no restarter reads the
        // context, and this processor is its only writer.
        unsafe { prepare_context(context, psci) };
        match self
            .state
            .compare_exchange(RUNNING, STOPPING, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {}
            Err(RESTART_PENDING) => {
                self.state.store(RUNNING, Ordering::Release);
                // SAFETY: restores the mask state read above.
                unsafe { crate::write_daif(daif) };
                return;
            }
            Err(state) => panic!(
                "AArch64 processor {mpidr:#x} stopped while its power state was {state}, not running"
            ),
        }
        // Nothing of this processor's timer survives the power-down, and
        // an interrupt taken after the interface is disabled would be
        // lost with it.
        // SAFETY: disables this processor's virtual timer.
        unsafe {
            asm!(
                "msr cntv_ctl_el0, xzr",
                "isb",
                options(nomem, nostack, preserves_flags)
            );
        }
        gic.detach_current_processor(mpidr);
        // SAFETY: the context is this processor's own and filled in by
        // `prepare_context`, and `power_off` only reads it.
        let status = unsafe { save_and_power_off(context, power_off) };
        // Only a failed `CPU_OFF` gets here without having been restarted.
        assert!(
            status == 0,
            "PSCI CPU_OFF failed on processor {mpidr:#x}: {}",
            psci::Error::from(status as i32)
        );

        // Restarted: the entry code restored the translation regime, the
        // vectors, the runtime pointer and the callee-saved registers. What
        // is left is the processor's own hardware state.
        crate::enable_pmu_for_current_processor();
        gic.attach_current_processor(mpidr);
        wake_pending.store(false, Ordering::Release);
        let previous = self.state.swap(RUNNING, Ordering::AcqRel);
        assert!(
            previous == RESTARTING,
            "AArch64 processor {mpidr:#x} resumed while its power state was {previous}, not restarting"
        );
        // SAFETY: restores the mask state read above, now that the
        // interrupt interface is back.
        unsafe { crate::write_daif(daif) };
    }

    /// Restarts the processor with `mpidr` that [`ProcessorPower::stop`]
    /// took out of service, or is about to.
    pub(crate) fn restart(&self, psci: Psci, mpidr: u64, timebase: Timebase) {
        loop {
            match self.state.load(Ordering::Acquire) {
                RUNNING => {
                    if self
                        .state
                        .compare_exchange(
                            RUNNING,
                            RESTART_PENDING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return;
                    }
                }
                STOPPING => {
                    if self
                        .state
                        .compare_exchange(STOPPING, RESTARTING, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        break;
                    }
                }
                state => panic!(
                    "AArch64 processor {mpidr:#x} was restarted while its power state was {state}: \
                     restarted twice for one stop"
                ),
            }
            core::hint::spin_loop();
        }
        let target = mpidr & PSCI_TARGET_MASK;
        let deadline = crate::read_counter()
            .checked_add(timebase.frequency() * OFF_DEADLINE_SECONDS)
            .unwrap_or_else(|| panic!("AArch64 processor power-off deadline overflow"));
        loop {
            let state = psci.affinity_info(target).unwrap_or_else(|error| {
                panic!("PSCI AFFINITY_INFO for {mpidr:#x} failed: {error}")
            });
            if state == AffinityState::Off {
                break;
            }
            assert!(
                crate::read_counter() <= deadline,
                "AArch64 processor {mpidr:#x} did not power off after it announced its stop \
                 (PSCI AFFINITY_INFO reports {state:?})"
            );
            core::hint::spin_loop();
        }
        // SAFETY: `Stopping` was published after these two words were
        // written, and nothing writes them again until the processor runs.
        let (entry, context) = unsafe {
            let context = &*self.context.get();
            (context.entry_physical, context.self_physical)
        };
        psci.cpu_on(target, entry, context)
            .unwrap_or_else(|error| panic!("PSCI CPU_ON for {mpidr:#x} failed: {error}"));
    }
}

macro_rules! read_system_register {
    ($name:literal) => {{
        let value: u64;
        // SAFETY: reads a system register of the executing processor.
        unsafe {
            asm!(
                concat!("mrs {value}, ", $name),
                value = out(reg) value,
                options(nomem, nostack, preserves_flags),
            );
        }
        value
    }};
}

/// Translates a kernel virtual address to its physical address with the
/// hardware's own walk, which is the one answer that cannot disagree with
/// what the MMU does.
fn translate(virtual_address: usize) -> u64 {
    let par: u64;
    // SAFETY: `at` only performs a translation and reports it in `PAR_EL1`.
    unsafe {
        asm!(
            "at s1e1r, {address}",
            "isb",
            "mrs {par}, par_el1",
            address = in(reg) virtual_address,
            par = out(reg) par,
            options(nomem, nostack, preserves_flags),
        );
    }
    assert!(
        par & 1 == 0,
        "the MMU could not translate kernel address {virtual_address:#x} for a processor restart \
         (PAR_EL1 {par:#x})"
    );
    (par & 0x0000_ffff_ffff_f000) | (virtual_address as u64 & (PAGE_BYTES as u64 - 1))
}

/// Fills in everything of `context` that does not change while the
/// processor is stopped: the translation and exception registers, the
/// identity map of the entry code, and the addresses `CPU_ON` takes.
///
/// # Safety
///
/// The caller is the only writer of `context`, and interrupts are masked.
unsafe fn prepare_context(context: *mut ResumeContext, psci: Psci) {
    // SAFETY: the caller owns the context.
    let context = unsafe { &mut *context };
    let entry_virtual = &raw const __helios_aarch64_cpu_resume as usize;
    let entry_end = &raw const __helios_aarch64_cpu_resume_end as usize;
    let entry_physical = translate(entry_virtual);
    let block = entry_physical & !(BLOCK_BYTES as u64 - 1);
    assert!(
        entry_physical + (entry_end - entry_virtual) as u64 <= block + BLOCK_BYTES as u64,
        "the AArch64 restart entry code straddles two 2 MiB blocks, which its identity map cannot cover"
    );
    assert!(
        block >> 48 == 0,
        "the AArch64 restart entry code sits above 48 physical address bits, which its identity map cannot cover"
    );

    context.mair = read_system_register!("mair_el1");
    context.tcr = read_system_register!("tcr_el1");
    context.ttbr0 = read_system_register!("ttbr0_el1");
    context.ttbr1 = read_system_register!("ttbr1_el1");
    context.sctlr = read_system_register!("sctlr_el1");
    context.cpacr = read_system_register!("cpacr_el1");
    context.vbar = read_system_register!("vbar_el1");
    context.tpidr = read_system_register!("tpidr_el1");
    context.sp_el0 = read_system_register!("sp_el0");
    context.fpcr = read_system_register!("fpcr");
    context.conduit = psci.conduit as u64;

    // The identity map needs a Normal write-back attribute and the one
    // that is there is whichever index the live MAIR gives it.
    let attribute = (0..8u64)
        .find(|index| (context.mair >> (index * 8)) & 0xff == 0xff)
        .unwrap_or_else(|| {
            panic!(
                "MAIR_EL1 {:#x} has no Normal write-back attribute for the restart identity map",
                context.mair
            )
        });
    let tables = [
        translate(&raw const context.idmap[0] as usize),
        translate(&raw const context.idmap[1] as usize),
        translate(&raw const context.idmap[2] as usize),
    ];
    for table in &mut context.idmap {
        table.0.fill(0);
    }
    let index = |shift: u32| ((block >> shift) as usize) & (TABLE_ENTRIES - 1);
    context.idmap[0].0[index(39)] = tables[1] | TABLE_DESCRIPTOR;
    context.idmap[1].0[index(30)] = tables[2] | TABLE_DESCRIPTOR;
    context.idmap[2].0[index(21)] = block | BLOCK_DESCRIPTOR_BASE | (attribute << 2);
    context.idmap_ttbr0 = tables[0];
    context.idmap_tcr = (context.tcr & !IDMAP_TCR_TTBR0_MASK) | IDMAP_TCR_TTBR0;
    context.self_virtual = context as *const ResumeContext as u64;
    context.self_physical = translate(context as *const ResumeContext as usize);
    context.continuation = &raw const __helios_aarch64_cpu_resume_virtual as u64;
    context.entry_physical = entry_physical;
}

/// Cleans the context to the point of coherency, where the entry code
/// reads it with the MMU and caches off, then powers the processor down.
/// Returns only when the firmware refuses, with its status.
///
/// # Safety
///
/// `context` is the calling processor's prepared context.
unsafe extern "C" fn power_off(context: *const ResumeContext) -> u64 {
    let line = crate::cache_line_bytes();
    let start = context as usize;
    let end = start + size_of::<ResumeContext>();
    let mut address = start & !(line - 1);
    while address < end {
        // SAFETY: cleans a line of the context, which is mapped.
        unsafe {
            asm!("dc cvac, {address}", address = in(reg) address, options(nostack, preserves_flags));
        }
        address += line;
    }
    // SAFETY: orders the cleans before the power-down.
    unsafe {
        asm!("dsb sy", options(nostack, preserves_flags));
    }
    // SAFETY: `context` is valid for reads.
    let conduit = unsafe { (*context).conduit };
    let result = if conduit == PsciConduit::Hvc as u64 {
        psci::cpu_off::<Hvc>()
    } else {
        psci::cpu_off::<Smc>()
    };
    match result {
        Ok(()) => panic!("PSCI CPU_OFF returned success, which the specification forbids"),
        Err(error) => i64::from(error) as u64,
    }
}

/// Saves the callee-saved registers and stack pointer into `context`, then
/// calls `power_off(context)`.
///
/// Returns `power_off`'s value if it returns, which only a failed
/// `CPU_OFF` does; and returns 0 when the processor is restarted, because
/// `resume.S` restores these registers and returns from here as if
/// `power_off` had.
///
/// # Safety
///
/// `context` is the calling processor's prepared context.
#[unsafe(naked)]
unsafe extern "C" fn save_and_power_off(
    context: *mut ResumeContext,
    power_off: unsafe extern "C" fn(*const ResumeContext) -> u64,
) -> u64 {
    naked_asm!(
        "stp x19, x20, [x0, #0]",
        "stp x21, x22, [x0, #16]",
        "stp x23, x24, [x0, #32]",
        "stp x25, x26, [x0, #48]",
        "stp x27, x28, [x0, #64]",
        "stp x29, x30, [x0, #80]",
        "mov x2, sp",
        "str x2, [x0, #96]",
        "stp d8, d9, [x0, #104]",
        "stp d10, d11, [x0, #120]",
        "stp d12, d13, [x0, #136]",
        "stp d14, d15, [x0, #152]",
        "stp x29, x30, [sp, #-16]!",
        "blr x1",
        "ldp x29, x30, [sp], #16",
        "ret",
    )
}
