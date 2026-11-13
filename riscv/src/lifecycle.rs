//! Taking a hart out of service and bringing it back.
//!
//! # The handshake
//!
//! `Cpu::stop_current` runs on the departing hart. It masks the hart's
//! interrupts and timer, leaves [`ONLINE_HARTS`] (so neither a wake nor a
//! remote `sfence.vma` addresses it from then on), saves the state a call
//! must preserve into a [`SavedContext`] in its own stack frame, publishes
//! that context's address in the hart's [`PARKED`] slot, and issues SBI HSM
//! `hart_stop`, which never returns.
//!
//! `Cpu::restart_processor` runs on another hart. The kernel calls it once
//! the departing hart has published that it is stopping, which can be
//! before the hart reached [`PARKED`], and before `hart_stop` took effect:
//! HSM reports the hart `STARTED`, then `STOP_PENDING`, then `STOPPED`, and
//! `hart_start` is only valid on a `STOPPED` hart. The restarter therefore
//! waits for the slot to be published and for the state to reach `STOPPED`.
//! Both waits are over a handful of instructions of the departing hart
//! (interrupts masked) and the firmware's own transition, hardware
//! synchronisation and not software readiness, so they spin.
//!
//! The restarted hart enters `_hart_restart` (`secondary_entry.S`), which
//! restores `gp`, `tp` and the saved `sp`, enables floating point as the
//! first start does, and calls [`helios_hart_resume`]. That runs below the
//! saved `sp`, in stack the departed hart no longer uses, so the frame
//! `stop_current` suspended in is intact. It rejoins [`ONLINE_HARTS`]
//! first and flushes the whole local TLB second, through the same
//! `activate_paging` a first start uses: a remote fence that read the mask
//! before the hart rejoined is ordered before its PTE writes, and one that
//! reads it after targets the hart. It then re-seeds `stvec`, `sscratch`
//! and a far timer deadline, and returns into `stop_current`.
//!
//! # Concurrency
//!
//! Every [`PARKED`] slot is written by its own hart and read by the one
//! restarter; [`ONLINE_HARTS`] is the machine-wide running mask and is only
//! ever changed with atomic read-modify-writes.

use core::arch::{asm, global_asm};
use core::mem::offset_of;
use core::sync::atomic::{AtomicUsize, Ordering};

use helios_hal::cpu::ProcessorId;

use super::{
    _hart_restart, MAX_TRACKED_HARTS, ONLINE_HARTS, hart_bit, hart_status,
    helios_current_processor, mark_hart_online, trap, vmm,
};

global_asm!(include_str!("hart_lifecycle.S"));

/// What `stop_current` must carry across the stop, in the layout
/// `hart_lifecycle.S` and `secondary_entry.S` index.
#[repr(C)]
struct SavedContext {
    ra: usize,
    sp: usize,
    tp: usize,
    s: [usize; 12],
    fs: [u64; 12],
    fcsr: usize,
    /// `sscratch` at the call: the trap stack top the entry and exit own.
    sscratch: usize,
    /// `sie` at the call.
    sie: usize,
    /// Whether `sstatus.SIE` was set at the call.
    interrupts_enabled: usize,
}

const _: () = {
    assert!(offset_of!(SavedContext, ra) == 0x00);
    assert!(offset_of!(SavedContext, sp) == 0x08);
    assert!(offset_of!(SavedContext, tp) == 0x10);
    assert!(offset_of!(SavedContext, s) == 0x18);
    assert!(offset_of!(SavedContext, fs) == 0x78);
    assert!(offset_of!(SavedContext, fcsr) == 0xd8);
};

/// Per hart, the address of the [`SavedContext`] it stopped with, or zero
/// while it is running (or has not reached its stop yet).
static PARKED: [AtomicUsize; MAX_TRACKED_HARTS] =
    [const { AtomicUsize::new(0) }; MAX_TRACKED_HARTS];

unsafe extern "C" {
    fn __helios_hart_suspend(context: *mut SavedContext);
    fn __helios_hart_resume_return(context: *const SavedContext) -> !;
}

fn parked_slot(hart: ProcessorId) -> &'static AtomicUsize {
    let index = usize::from(hart.id());
    PARKED
        .get(index)
        .unwrap_or_else(|| panic!("hart {index} is outside the {MAX_TRACKED_HARTS} harts tracked"))
}

/// Refuses a first start of a hart that has been stopped: that hart is
/// restarted, not started, because its stack and state are live.
pub(super) fn assert_never_stopped(hart: ProcessorId) {
    debug_assert!(
        parked_slot(hart).load(Ordering::Acquire) == 0,
        "hart {} was stopped; it is restarted, not started",
        hart.id()
    );
}

pub(super) fn stop_current() {
    let hart = helios_current_processor();
    let mut context = SavedContext {
        ra: 0,
        sp: 0,
        tp: 0,
        s: [0; 12],
        fs: [0; 12],
        fcsr: 0,
        sscratch: 0,
        sie: 0,
        interrupts_enabled: 0,
    };
    // SAFETY: reading `sscratch` and `sie` has no side effects.
    unsafe {
        asm!("csrr {}, sscratch", out(reg) context.sscratch, options(nomem, nostack));
        asm!("csrr {}, sie", out(reg) context.sie, options(nomem, nostack));
    }
    context.interrupts_enabled = usize::from(riscv::register::sstatus::read().sie());

    // SAFETY: the departing hart answers no interrupt from here until it
    // is restarted, and nothing it could take needs servicing: the kernel
    // has torn its per-processor owners down.
    unsafe {
        riscv::interrupt::supervisor::disable();
        asm!("csrw sie, zero", options(nomem, nostack));
    }
    // A far deadline withdraws the pending timer interrupt and leaves the
    // comparator quiet until the kernel programs a new one.
    sbi_rt::set_timer(u64::MAX);
    ONLINE_HARTS.fetch_and(!hart_bit(hart), Ordering::SeqCst);

    // SAFETY: `context` outlives the stop, in this frame, above the `sp`
    // the call saves. The call returns here once the hart is restarted.
    unsafe {
        __helios_hart_suspend(&raw mut context);
    }

    // SAFETY: restoring the interrupt state the caller ran with.
    unsafe {
        asm!("csrw sie, {}", in(reg) context.sie, options(nomem, nostack));
        if context.interrupts_enabled != 0 {
            riscv::interrupt::supervisor::enable();
        }
    }
}

/// Publishes the stop and hands the hart to the firmware.
#[unsafe(no_mangle)]
extern "C" fn helios_hart_stop(context: *const SavedContext) -> ! {
    let hart = helios_current_processor();
    parked_slot(hart).store(context as usize, Ordering::Release);
    let ret = sbi_rt::hart_stop();
    panic!(
        "SBI HSM refused to stop hart {}: error={} value={}",
        hart.id(),
        ret.error,
        ret.value
    );
}

/// The restarted hart's first Rust, entered from `_hart_restart` with `tp`
/// and `sp` as the hart stopped with them.
#[unsafe(no_mangle)]
extern "C" fn helios_hart_resume(hart_id: usize, context: &SavedContext) -> ! {
    let hart = helios_current_processor();
    debug_assert_eq!(usize::from(hart.id()), hart_id);
    // Rejoin the running mask before the flush: see the module docs.
    mark_hart_online(hart);
    // SAFETY: the root table was installed at boot, before any hart
    // stopped.
    unsafe {
        vmm::activate_paging();
        trap::install_trap_vector();
        asm!("csrw sscratch, {}", in(reg) context.sscratch, options(nomem, nostack));
        asm!("csrc sip, {}", in(reg) 1usize << 1, options(nomem, nostack));
    }
    sbi_rt::set_timer(u64::MAX);
    parked_slot(hart).store(0, Ordering::Release);
    // SAFETY: `context` is the one `__helios_hart_suspend` filled for
    // this hart.
    unsafe { __helios_hart_resume_return(context) }
}

pub(super) fn restart_processor(hart: ProcessorId) {
    let slot = parked_slot(hart);
    let context = loop {
        match slot.load(Ordering::Acquire) {
            0 => core::hint::spin_loop(),
            context => break context,
        }
    };
    loop {
        match hart_status(hart) {
            sbi_spec::hsm::hart_state::STOPPED => break,
            sbi_spec::hsm::hart_state::STARTED | sbi_spec::hsm::hart_state::STOP_PENDING => {
                core::hint::spin_loop();
            }
            state => panic!(
                "hart {} reported unexpected SBI HSM state {state} while being restarted",
                hart.id()
            ),
        }
    }
    let ret = sbi_rt::hart_start(
        usize::from(hart.id()),
        core::ptr::addr_of!(_hart_restart).addr(),
        context,
    );
    assert!(
        ret.is_ok(),
        "failed to restart hart {} via SBI HSM: error={} value={}",
        hart.id(),
        ret.error,
        ret.value
    );
}

/// Flushes `start..start + len` from the TLB of every running hart but
/// the caller, and returns once they have.
///
/// The mask is [`ONLINE_HARTS`], not every hart: SBI refuses a fence that
/// names a hart that is not started, and a stopped hart flushes its whole
/// TLB before rejoining. A hart can leave between reading the mask and the
/// call, which SBI reports as an invalid parameter; the mask is then read
/// again, and an error that survives an unchanged mask is a real one.
pub(super) fn remote_sfence_vma(start: usize, len: usize) {
    let own = hart_bit(helios_current_processor());
    let mut previous = 0;
    loop {
        let mask = ONLINE_HARTS.load(Ordering::SeqCst) & !own;
        if mask == 0 {
            return;
        }
        let ret = sbi_rt::remote_sfence_vma(sbi_rt::HartMask::from_mask_base(mask, 0), start, len);
        if ret.is_ok() {
            return;
        }
        assert!(
            ret.error == sbi_spec::binary::RET_ERR_INVALID_PARAM && mask != previous,
            "SBI remote_sfence_vma failed for user VA {start:#x}+{len:#x} on harts {mask:#x}: \
             error={} value={}",
            ret.error,
            ret.value
        );
        previous = mask;
    }
}
