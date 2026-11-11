use std::cell::RefCell;
use std::sync::Arc;
use std::thread;

use helios_hal::cpu::{Cpu, Instant, ProcessorId, Timebase, TimerInterrupt, current_processor};
use helios_hal::entropy::{EntropyQuality, EntropyUnavailable};
use helios_kernel::Timer;

use crate::runtime::HostedMachine;

/// Hosted CPU adapter that exposes one OS thread as one logical processor.
///
/// A handle names the machine, not a processor: like a register read on
/// bare metal, every per-processor operation acts on the thread that
/// calls it, whose identity [`HostedCpu::new`] seeded.
#[derive(Clone)]
pub struct HostedCpu {
    machine: Arc<HostedMachine>,
}

impl HostedCpu {
    /// The handle for the calling thread, which stands for `processor`
    /// from here on.
    pub fn new(processor: ProcessorId, machine: Arc<HostedMachine>) -> Self {
        set_current_processor(processor);
        Self { machine }
    }

    /// Installs the calling processor's timer, which runs its interrupt
    /// step whenever the machine's timer thread has raised this
    /// processor's timer interrupt. The processor calls this once, after
    /// `helios_kernel::init`, the way the bare-metal backends install
    /// theirs; an interrupt raised before that stays pending.
    pub fn install_timer(&self, timer: Timer<HostedCpu>) {
        PROCESSOR_TIMER.with(|slot| {
            let previous = slot.borrow_mut().replace(timer);
            assert!(
                previous.is_none(),
                "hosted processor timer was installed more than once"
            );
        });
    }

    /// Delivers a pending timer interrupt to the calling processor: runs
    /// the installed timer's interrupt step if the machine raised one.
    ///
    /// A hosted processor is a thread and cannot be preempted, so an
    /// interrupt is taken where the thread yields control to the
    /// machine, which is [`Cpu::park_current`].
    fn take_timer_interrupt(&self) {
        PROCESSOR_TIMER.with(|slot| {
            let slot = slot.borrow();
            let Some(timer) = slot.as_ref() else {
                return;
            };
            if self.machine.take_timer_interrupt(current_processor()) {
                timer.handle_interrupt();
            }
        });
    }
}

thread_local! {
    /// This thread's processor timer, installed by
    /// [`HostedCpu::install_timer`]. Only the processor's own thread runs
    /// its interrupt step, so it lives with the thread rather than in
    /// the shared machine.
    static PROCESSOR_TIMER: RefCell<Option<Timer<HostedCpu>>> = const { RefCell::new(None) };

    /// This thread's logical processor, seeded when the thread's
    /// [`HostedCpu`] is built and read by the identity contract below.
    static CURRENT_PROCESSOR: core::cell::Cell<Option<ProcessorId>> =
        const { core::cell::Cell::new(None) };
}

/// Declares which logical processor the calling thread stands in for.
///
/// The bare-metal backends seed a register in their boot path; a hosted
/// processor is an OS thread, so it seeds a thread-local instead. A
/// thread that runs kernel code without doing this is a bug in the
/// hosted machine's thread setup, and the contract below says so rather
/// than answering with processor zero.
pub fn set_current_processor(processor: ProcessorId) {
    CURRENT_PROCESSOR.with(|slot| slot.set(Some(processor)));
}

/// The processor identity `hal` publishes as a linkage contract.
#[unsafe(no_mangle)]
extern "Rust" fn helios_current_processor() -> ProcessorId {
    CURRENT_PROCESSOR.with(|slot| {
        slot.get()
            .expect("this thread runs kernel code without a hosted processor identity")
    })
}

impl Cpu for HostedCpu {
    // The timer thread wakes each processor at the deadline last set for it.
    const TIMER_INTERRUPT: TimerInterrupt = TimerInterrupt::Deadline;

    fn processor_count(&self) -> usize {
        self.machine.processor_count()
    }

    fn bootstrap_processor(&self) -> ProcessorId {
        self.machine.bootstrap_processor()
    }

    /// Parks until woken, taking the processor's timer interrupt on the
    /// way in and on the way out: one raised while it ran is handled
    /// before it sleeps, and one that woke it is handled before it
    /// returns. A raise between the check and the park leaves its unpark
    /// token, so the park returns at once and the interrupt is taken on
    /// the way out.
    fn park_current(&self) {
        self.take_timer_interrupt();
        thread::park();
        self.take_timer_interrupt();
    }

    fn start_processor(&self, processor: ProcessorId) {
        self.machine.start_processor(processor);
    }

    fn wake_processor(&self, processor: ProcessorId) {
        self.machine.wake_processor(processor);
    }

    fn stop_current(&self) {
        self.machine.stop_current(current_processor());
    }

    fn restart_processor(&self, processor: ProcessorId) {
        self.machine.restart_processor(processor);
    }

    fn now(&self) -> Instant {
        self.machine.now()
    }

    fn timebase(&self) -> Timebase {
        self.machine.timebase()
    }

    fn set_deadline(&self, deadline: Instant) {
        self.machine.set_deadline(current_processor(), deadline);
    }

    fn publish_executable(&self, _ptr: *const u8, _len: usize) {}

    fn unpublish_executable(&self, _ptr: *const u8, _len: usize) {}

    fn native_feature_probe(&self) -> Option<fn(&str) -> Option<bool>> {
        None
    }

    fn fill_entropy(&self, buffer: &mut [u8]) -> Result<EntropyQuality, EntropyUnavailable> {
        getrandom::fill(buffer)
            .unwrap_or_else(|error| panic!("host entropy source failed: {error}"));
        Ok(EntropyQuality::Cryptographic)
    }

    fn has_lazy_commit_virtual_memory(&self) -> bool {
        // Hosted runs as a regular host process: libc `mmap(PROT_
        // NONE)` reserves arbitrary virtual ranges with no physical
        // commit until the host kernel page-faults a touched page
        // back in. That is the exact "petabyte-scale lazy-commit"
        // capability the trait talks about — kernel-side runtimes
        // such as the Wasmtime pooling allocator can pre-reserve
        // 4 GiB+ per instance without burning RAM.
        true
    }

    fn shutdown(&self) -> ! {
        std::process::exit(0)
    }

    fn reboot(&self) -> ! {
        std::process::exit(1)
    }
}
