use core::ptr::NonNull;

use crate::entropy::{EntropyQuality, EntropyUnavailable};

/// Logical processor identifier.
///
/// This is the architecture-neutral execution slot identifier exposed to the
/// kernel. Backends map it onto their native concept: a RISC-V hart id, an
/// x86 APIC CPU id, an ARM PE index, or a synthetic hosted test CPU slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProcessorId(u16);

impl ProcessorId {
    pub const fn new(id: u16) -> Self {
        Self(id)
    }

    pub const fn id(&self) -> u16 {
        self.0
    }
}

/// Monotonic platform time expressed in backend-defined timer ticks.
///
/// `Instant` is intentionally opaque at the HAL boundary: the kernel may compare
/// values or add tick deltas, but only the platform knows how those ticks map to
/// real hardware time.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant(u64);

impl Instant {
    pub const fn new(ticks: u64) -> Self {
        Self(ticks)
    }

    pub const fn ticks(&self) -> u64 {
        self.0
    }

    pub const fn saturating_add(self, ticks: u64) -> Self {
        Self(self.0.saturating_add(ticks))
    }
}

/// Converts a timer-tick count to nanoseconds against a platform
/// timebase `frequency`.
///
/// The multiplication is widened to `u128` on purpose: `ticks * 1e9`
/// overflows a `u64` once the tick count passes `u64::MAX / 1e9`
/// (about 1.845e10 ticks). On a TSC timebase that ceiling is only a
/// handful of seconds of uptime, and a saturating `u64` product pins
/// the result to a constant — every wall-clock deadline derived from
/// it (TCP pacing and retransmission timers, operation timeouts, WASI
/// clocks) then stops advancing and the waiting tasks park forever.
/// Timebases with a lower frequency merely hit the same cliff later,
/// which is why the conversion lives here and is shared by every
/// backend rather than being rewritten per platform.
pub const fn ticks_to_nanos(ticks: u64, frequency: u64) -> u64 {
    assert!(frequency != 0, "platform timer frequency must be non-zero");
    let nanos = (ticks as u128 * 1_000_000_000u128) / frequency as u128;
    if nanos > u64::MAX as u128 {
        u64::MAX
    } else {
        nanos as u64
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HardwarePerfCounters {
    pub reference_cycles: Option<u64>,
    pub cpu_cycles: Option<u64>,
    pub instructions_retired: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HardwarePerfCounterDelta {
    pub reference_cycles: u64,
    pub cpu_cycles: u64,
    pub instructions_retired: u64,
}

impl HardwarePerfCounters {
    pub fn delta_since(self, start: Self) -> HardwarePerfCounterDelta {
        HardwarePerfCounterDelta {
            reference_cycles: wrapping_option_delta(self.reference_cycles, start.reference_cycles),
            cpu_cycles: wrapping_option_delta(self.cpu_cycles, start.cpu_cycles),
            instructions_retired: wrapping_option_delta(
                self.instructions_retired,
                start.instructions_retired,
            ),
        }
    }
}

const fn wrapping_option_delta(end: Option<u64>, start: Option<u64>) -> u64 {
    match (end, start) {
        (Some(end), Some(start)) => end.wrapping_sub(start),
        _ => 0,
    }
}

pub trait Cpu: Send + Sync + 'static {
    /// Returns the processor currently executing this code path.
    ///
    /// This must be cheap and stable for the lifetime of the `Cpu` value because
    /// the kernel queries it during boot, scheduling, and panic reporting.
    fn current_processor(&self) -> ProcessorId;

    /// Returns the number of processors the platform exposes to the kernel.
    ///
    /// The kernel uses this to decide which secondary processors to start during SMP
    /// bootstrap. This is a platform capability, not a scheduler state query.
    fn processor_count(&self) -> usize;

    /// Returns the processor designated as the bootstrap processor.
    ///
    /// Exactly one processor performs one-time global initialization such as heap and
    /// logger setup; all other processors wait until that work is complete.
    fn bootstrap_processor(&self) -> ProcessorId;

    /// Parks the current processor until some external event makes forward progress
    /// possible again.
    ///
    /// Typical implementations are `wfi` on bare metal and `thread::park()` in
    /// hosted mode. The contract is only "stop burning CPU until woken", not any
    /// stronger fairness guarantee.
    fn park_current(&self);

    /// Starts execution on a secondary processor.
    ///
    /// The kernel calls this during bootstrap for every processor other than the
    /// bootstrap processor. Implementations may panic if the platform cannot start the
    /// requested processor, because that is a real bring-up failure.
    fn start_processor(&self, processor: ProcessorId);

    /// Nudges a processor that may currently be parked so it can observe newly
    /// runnable work.
    ///
    /// This is the wakeup primitive used for cross-processor scheduling. On RISC-V
    /// this maps naturally to an IPI; in hosted mode it maps to unparking the
    /// backing thread.
    fn wake_processor(&self, processor: ProcessorId);

    /// Returns the current monotonic time in platform timer ticks.
    fn now(&self) -> Instant;

    /// Returns the number of timer ticks that elapse per second.
    ///
    /// The async timer layer uses this to translate Rust `Duration` values into
    /// backend timer ticks without hardcoding any platform frequency in kernel
    /// code.
    fn timer_frequency(&self) -> u64;

    /// Snapshots hardware performance counters visible to the current
    /// privilege level on the current processor.
    ///
    /// Backends fill only counters that are genuinely backed by CPU
    /// hardware on that target. Unsupported counters stay `None`;
    /// the kernel records them as absent rather than inventing software
    /// substitutes.
    fn hardware_perf_counters(&self) -> HardwarePerfCounters {
        HardwarePerfCounters::default()
    }

    /// Programs the next wakeup deadline for the current processor.
    ///
    /// The deadline is absolute, in the same tick domain as [`Cpu::now`]. Calling
    /// this again replaces the previous deadline.
    fn set_deadline(&self, deadline: Instant);

    /// Publishes freshly loaded executable code so that it becomes visible to
    /// instruction fetches on all processors.
    ///
    /// Architectures that require explicit instruction-cache synchronisation
    /// or page-table permission updates implement the required sequence here.
    fn publish_executable(&self, ptr: *const u8, len: usize);

    /// Reverts executable code back to writable code-memory storage after all
    /// processors have stopped executing from the range.
    fn unpublish_executable(&self, ptr: *const u8, len: usize);

    /// Returns an optional native ISA feature probe for consumers that need
    /// target-feature decisions.
    fn native_feature_probe(&self) -> Option<fn(&str) -> Option<bool>>;

    /// Fills `buffer` from a platform entropy source.
    ///
    /// Backends that expose a hardware or host operating-system source return
    /// the quality of the provided bytes. Backends without such a source return
    /// `EntropyUnavailable`; higher layers decide whether a caller can proceed
    /// without cryptographic entropy.
    fn fill_entropy(&self, _buffer: &mut [u8]) -> Result<EntropyQuality, EntropyUnavailable> {
        Err(EntropyUnavailable)
    }

    /// Whether this backend's virtual-memory subsystem supports
    /// petabyte-scale lazy-commit reservations: reserving a large
    /// virtual range and only materialising physical backing on access.
    /// Hosted satisfies this through `mmap(PROT_NONE)`. Bare-metal
    /// targets satisfy it once their `hal::vmm::AddressSpace`
    /// implementation reaches a full reserve/commit/decommit
    /// surface. The default `false` keeps backends that have not yet
    /// wired this up on a conservative eager-backing strategy.
    fn has_lazy_commit_virtual_memory(&self) -> bool {
        false
    }

    /// Zero-initializes `size` bytes starting at `ptr`.
    ///
    /// The default implementation falls through to
    /// [`core::ptr::write_bytes`], which lowers to the platform memset.
    /// Architectures with hardware-assisted block clear instructions —
    /// most notably AArch64 `dc zva` — override this to clear at
    /// cache-line granularity without polluting the data caches.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a region of at least `size` writable bytes
    /// that is unaliased for the duration of the call.
    unsafe fn zero_memory(&self, ptr: NonNull<u8>, size: usize) {
        // SAFETY: caller has guaranteed `ptr` is writable for `size`
        // bytes; `write_bytes` performs a typed-memory store of zeros.
        unsafe {
            core::ptr::write_bytes(ptr.as_ptr(), 0, size);
        }
    }

    /// Powers the machine off and never returns.
    fn shutdown(&self) -> !;

    /// Reboots the machine and never returns.
    fn reboot(&self) -> !;
}
