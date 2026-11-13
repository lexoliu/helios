use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
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
        self.machine.install_timer(current_processor(), timer);
    }
}

/// The wake-only mask of a hosted processor: nothing to restore, and
/// bound to the thread that took it like the register state it stands for.
pub struct HostedWakeOnlyMask {
    _processor_local: PhantomData<*mut ()>,
}

thread_local! {
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
    /// returns. The timer thread raises the interrupt before it unparks,
    /// so a raise between the check and the park leaves its unpark
    /// token, the park returns at once and the interrupt is taken on the
    /// way out. Either way the interrupt step runs here, on the owner.
    fn park_current(&self) {
        let processor = current_processor();
        self.machine.take_timer_interrupt(processor);
        thread::park();
        self.machine.take_timer_interrupt(processor);
    }

    type WakeOnlyMask = HostedWakeOnlyMask;

    fn mask_to_wake_only(&self) -> HostedWakeOnlyMask {
        // A hosted processor is a thread whose only interrupt sources are
        // unparks, and none of them runs device code on a thread that is
        // not parked: the timer thread's expiry sets a flag that the
        // owner's `park_current` acts on. There is no register to narrow.
        HostedWakeOnlyMask {
            _processor_local: PhantomData,
        }
    }

    fn park_wake_only(&self, word: &AtomicU8, held: u8) {
        // A hosted processor is a thread and its only interrupt sources
        // are unparks: the timer thread's and `wake_processor`'s. Nothing
        // here delivers device code to a parked thread, so "every source
        // but the wake" is the parked thread itself, which runs nothing
        // when an unpark ends the park and the word is unchanged. A
        // store made before the unpark that ends the wait is seen
        // through the park's own acquire, and an unpark that lands
        // before the park leaves a token the park consumes.
        while word.load(Ordering::Acquire) == held {
            thread::park();
        }
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
        Some(detect_host_native_feature)
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

/// Answers the feature names Wasmtime asks about from the host process's own
/// CPU detection, which is the standard library's view of what the host
/// operating system lets this process execute.
///
/// A name the host architecture does not have is unknown, not absent.
#[cfg(target_arch = "x86_64")]
pub(crate) fn detect_host_native_feature(feature: &str) -> Option<bool> {
    use std::arch::is_x86_feature_detected as detected;
    Some(match feature {
        "cmpxchg16b" => detected!("cmpxchg16b"),
        "sse3" => detected!("sse3"),
        "ssse3" => detected!("ssse3"),
        "sse4.1" => detected!("sse4.1"),
        "sse4.2" => detected!("sse4.2"),
        "popcnt" => detected!("popcnt"),
        "avx" => detected!("avx"),
        "avx2" => detected!("avx2"),
        "fma" => detected!("fma"),
        "bmi1" => detected!("bmi1"),
        "bmi2" => detected!("bmi2"),
        "lzcnt" => detected!("lzcnt"),
        "avx512f" => detected!("avx512f"),
        "avx512vl" => detected!("avx512vl"),
        "avx512dq" => detected!("avx512dq"),
        "avx512vbmi" => detected!("avx512vbmi"),
        "avx512bitalg" => detected!("avx512bitalg"),
        _ => return None,
    })
}

/// See the x86-64 variant.
#[cfg(target_arch = "aarch64")]
pub(crate) fn detect_host_native_feature(feature: &str) -> Option<bool> {
    use std::arch::is_aarch64_feature_detected as detected;
    Some(match feature {
        "lse" => detected!("lse"),
        "fp16" => detected!("fp16"),
        "dotprod" => detected!("dotprod"),
        _ => return None,
    })
}

/// A host architecture with no cwasm ISA levels answers no feature.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub(crate) fn detect_host_native_feature(_feature: &str) -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use helios_artifact::cwasm_target_isa_levels;

    use super::detect_host_native_feature;

    /// The probe has to answer every feature of every level of the host's
    /// own target, or the level selection stage 3 adds fails at boot.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn host_probe_answers_every_level_feature() {
        let target = if cfg!(target_arch = "x86_64") {
            "x86_64-unknown-linux-gnu"
        } else {
            "aarch64-unknown-linux-gnu"
        };
        let levels = cwasm_target_isa_levels(target);
        for level in levels.iter() {
            for feature in level.features() {
                assert!(
                    detect_host_native_feature(feature.probe_name()).is_some(),
                    "hosted probe does not answer {} for {}",
                    feature.probe_name(),
                    feature.cranelift_flag()
                );
            }
        }
        levels
            .select(Some(detect_host_native_feature))
            .expect("the host satisfies the floor level of its own target");
    }
}
