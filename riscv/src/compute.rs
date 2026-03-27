use core::arch::asm;
use core::ptr::NonNull;

use trapframe::TrapFrame;

const SSTATUS_SIE_BIT: usize = 1 << 1;
const SSTATUS_SPIE_BIT: usize = 1 << 5;
const SSTATUS_SPP_BIT: usize = 1 << 8;

pub type ComputeTaskEntry = extern "C" fn(usize) -> !;

/// Register state that must be inherited by every supervisor-mode compute task.
///
/// `gp` and `tp` are not task-local in this kernel:
/// - `gp` must keep pointing at the kernel's global pointer window
/// - `tp` carries the current hart runtime pointer
///
/// The scheduler captures this template once per hart and stamps it into every
/// newly created compute task context.
#[derive(Clone, Copy)]
pub struct ComputeContextTemplate {
    gp: usize,
    tp: usize,
    sstatus: usize,
}

impl ComputeContextTemplate {
    pub fn capture_current() -> Self {
        let gp: usize;
        let tp: usize;
        unsafe {
            asm!("mv {}, gp", out(reg) gp, options(nomem, nostack, preserves_flags));
            asm!("mv {}, tp", out(reg) tp, options(nomem, nostack, preserves_flags));
        }

        Self {
            gp,
            tp,
            sstatus: riscv::register::sstatus::read().bits(),
        }
    }

    fn task_sstatus(self) -> usize {
        // Compute threads always resume in supervisor mode with interrupts
        // enabled after `sret`, but not while the trap handler is still
        // mutating the frame.
        (self.sstatus | SSTATUS_SPP_BIT | SSTATUS_SPIE_BIT) & !SSTATUS_SIE_BIT
    }
}

/// Saved execution context for one preemptible compute task.
///
/// The timer interrupt handler will save the interrupted trap frame here, and
/// the scheduler will later restore it into the live trap frame of a compute
/// hart before returning from the interrupt.
#[derive(Clone, Copy)]
pub struct ComputeTaskContext {
    trap_frame: TrapFrame,
}

impl ComputeTaskContext {
    /// Builds the initial supervisor context for a fresh compute task.
    ///
    /// The returned frame is ready to be copied into the live trap frame of a
    /// compute hart. After `sret`, execution will begin at `entry(arg)` on the
    /// provided stack.
    pub fn start(
        stack: NonNull<[u8]>,
        entry: ComputeTaskEntry,
        arg: usize,
        template: ComputeContextTemplate,
    ) -> Self {
        let mut trap_frame = TrapFrame::default();
        trap_frame.general.sp = aligned_stack_top(stack);
        trap_frame.general.gp = template.gp;
        trap_frame.general.tp = template.tp;
        trap_frame.general.a0 = arg;
        trap_frame.general.ra = compute_task_returned as *const () as usize;
        trap_frame.sstatus = template.task_sstatus();
        trap_frame.sepc = entry as usize;
        Self { trap_frame }
    }

    pub fn save_from_trap(&mut self, trap_frame: &TrapFrame) {
        self.trap_frame = *trap_frame;
    }

    pub fn restore_into_trap(self, trap_frame: &mut TrapFrame) {
        *trap_frame = self.trap_frame;
    }

    pub fn trap_frame(&self) -> &TrapFrame {
        &self.trap_frame
    }
}

fn aligned_stack_top(stack: NonNull<[u8]>) -> usize {
    let stack = stack.as_ptr();
    let len = unsafe { (&*stack).len() };
    let top = stack.cast::<u8>() as usize + len;
    top & !0xF
}

extern "C" fn compute_task_returned() -> ! {
    panic!("compute task returned unexpectedly");
}
