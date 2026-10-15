use core::ptr::NonNull;
use core::time::Duration;

use strength_reduce::StrengthReducedU64;

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

const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// A platform timebase: the tick frequency of [`Cpu::now`] together with
/// the precomputed reductions that convert between ticks and nanoseconds.
///
/// The frequency is fixed for the life of the machine, so the divisor of
/// every conversion is invariant. A backend builds its `Timebase` once at
/// bring-up, where it learns the frequency, and every conversion after that
/// divides by a precomputed exact reciprocal (a multiply-high) instead of
/// a hardware or software division.
///
/// Every conversion is exact and saturates at `u64::MAX`. A naive
/// `ticks * 1e9` product overflows a `u64` once the tick count passes
/// `u64::MAX / 1e9` (about 1.845e10 ticks), which on a TSC timebase is
/// a handful of seconds of uptime; a saturating product there pins every
/// derived deadline (TCP pacing and retransmission timers, operation
/// timeouts, guest clock reads) to a constant and parks the waiting tasks
/// forever. The conversions therefore split the count at the divisor
/// (`ticks = q * f + r`) and scale only the remainder, which stays below
/// `f * 1e9`. That product fits a `u64` only up to
/// [`Timebase::MAX_FREQUENCY`], which [`Timebase::new`] enforces.
///
/// The value is immutable and `Copy`, so any processor reads it without
/// synchronisation.
#[derive(Clone, Copy, Debug)]
pub struct Timebase {
    frequency: StrengthReducedU64,
}

impl Timebase {
    /// The highest frequency whose sub-second products (`r * 1e9` with
    /// `r < f`, and `ns * f` with `ns < 1e9`) fit a `u64`: about 18.4 GHz.
    pub const MAX_FREQUENCY: u64 = u64::MAX / NANOS_PER_SECOND;

    /// Builds the timebase for a timer that ticks `frequency` times a
    /// second.
    ///
    /// # Panics
    ///
    /// Panics when `frequency` is zero or above [`Timebase::MAX_FREQUENCY`]:
    /// either is a platform that misreported its timer, and no conversion
    /// against it could be exact.
    pub fn new(frequency: u64) -> Self {
        assert!(frequency != 0, "platform timer frequency must be non-zero");
        assert!(
            frequency <= Self::MAX_FREQUENCY,
            "platform timer frequency {frequency} Hz exceeds the {} Hz an exact tick conversion supports",
            Self::MAX_FREQUENCY
        );
        Self {
            frequency: StrengthReducedU64::new(frequency),
        }
    }

    /// The number of timer ticks that elapse per second.
    #[inline]
    pub fn frequency(&self) -> u64 {
        self.frequency.get()
    }

    /// Converts a tick count to nanoseconds, rounding down and saturating
    /// at `u64::MAX`.
    #[inline]
    pub fn ticks_to_nanos(&self, ticks: u64) -> u64 {
        let (seconds, remainder) = StrengthReducedU64::div_rem(ticks, self.frequency);
        // `remainder < frequency <= MAX_FREQUENCY`, so the product fits.
        let subsecond = (remainder * NANOS_PER_SECOND) / self.frequency;
        seconds
            .saturating_mul(NANOS_PER_SECOND)
            .saturating_add(subsecond)
    }

    /// Converts nanoseconds to ticks, rounding up and saturating at
    /// `u64::MAX`.
    ///
    /// Rounding up keeps a deadline derived from it from firing before the
    /// requested time has passed.
    #[inline]
    pub fn nanos_to_ticks_ceil_saturating(&self, nanos: u64) -> u64 {
        let frequency = self.frequency();
        let seconds = nanos / NANOS_PER_SECOND;
        let subsecond = nanos % NANOS_PER_SECOND;
        // `subsecond < 1e9` and `frequency <= MAX_FREQUENCY`, so the
        // product fits.
        seconds
            .saturating_mul(frequency)
            .saturating_add((subsecond * frequency).div_ceil(NANOS_PER_SECOND))
    }

    /// Converts a duration to ticks, rounding down and saturating at
    /// `u64::MAX`.
    ///
    /// Guests legitimately pass `u64::MAX` nanoseconds as an "infinite"
    /// timeout (futex and poll waits). On timebases at or above 1 GHz that
    /// exceeds `u64` ticks; a deadline centuries out is semantically
    /// "never", so the conversion saturates instead of treating guest input
    /// as a kernel invariant.
    #[inline]
    pub fn duration_to_ticks(&self, duration: Duration) -> u64 {
        let frequency = self.frequency();
        // `subsec_nanos() < 1e9` and `frequency <= MAX_FREQUENCY`, so the
        // product fits.
        let subsecond = u64::from(duration.subsec_nanos()) * frequency / NANOS_PER_SECOND;
        duration
            .as_secs()
            .saturating_mul(frequency)
            .saturating_add(subsecond)
    }
}

impl PartialEq for Timebase {
    fn eq(&self, other: &Self) -> bool {
        // The reductions are a function of the frequency alone.
        self.frequency() == other.frequency()
    }
}

impl Eq for Timebase {}

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

unsafe extern "Rust" {
    /// The identity of the processor executing this code path, defined by
    /// the backend the image links.
    ///
    /// This is published as a linkage contract rather than a trait method
    /// because the answer is a property of the executing hardware, not of
    /// any value the kernel holds. Every backend reads it in one
    /// instruction from a register its boot path seeds before the heap
    /// exists — `fs:[8]`, `tpidr_el1`, `tp` — so the identity is available
    /// strictly earlier than a [`Cpu`] handle can be constructed, and code
    /// that holds no state at all can still ask: a task future, a drop
    /// path, the global allocator.
    ///
    /// A backend defines it with `#[unsafe(no_mangle)]` under this exact
    /// name, the way an image supplies its global allocator and its panic
    /// handler. Calling it before the backend has seeded that register is
    /// the backend's own bug to fail on, not a case to answer with a
    /// default.
    fn helios_current_processor() -> ProcessorId;
}

/// The processor executing this code path.
pub fn current_processor() -> ProcessorId {
    // Safety: the image is required to define `helios_current_processor`,
    // and a Rust-ABI call to it carries `ProcessorId` unchanged.
    unsafe { helios_current_processor() }
}

pub trait Cpu: Send + Sync + 'static {
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

    /// Returns the timebase of [`Cpu::now`]: its tick frequency and the
    /// conversions between ticks and nanoseconds.
    ///
    /// The async timer layer uses this to translate Rust `Duration` values into
    /// backend timer ticks without hardcoding any platform frequency in kernel
    /// code. A backend builds the value once at bring-up and returns the stored
    /// copy; it never rebuilds it per call.
    fn timebase(&self) -> Timebase;

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
    /// terabyte-scale lazy-commit reservations: reserving a large
    /// virtual range and only materialising physical backing on access.
    ///
    /// Hosted satisfies this through `mmap(PROT_NONE)`; a bare-metal
    /// backend satisfies it once its `hal::vmm::AddressSpace`
    /// implementation reaches a full reserve/commit/decommit surface and
    /// its user-VA window is large enough to hold the reservations its
    /// runtime asks for. Every backend in this repository does.
    ///
    /// The default `false` is for a backend that has not wired that up
    /// yet: there is no weaker memory strategy for it to run on, so the
    /// consumer that needs the capability refuses to start rather than
    /// substituting one.
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

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::Timebase;

    const GIGAHERTZ: u64 = 1_000_000_000;

    #[test]
    fn infinite_guest_timeout_saturates_instead_of_panicking() {
        // Guests pass u64::MAX nanoseconds as an "infinite"
        // futex/poll timeout. On a 1 GHz timebase the tick product
        // exceeds u64; the conversion must saturate to a
        // never-expiring deadline rather than panic the kernel on
        // guest-controlled input. Below 1 GHz the product still fits
        // and must stay exact.
        let infinite = Duration::from_nanos(u64::MAX);
        assert_eq!(
            Timebase::new(GIGAHERTZ).duration_to_ticks(infinite),
            u64::MAX
        );
        assert_eq!(
            Timebase::new(3 * GIGAHERTZ).duration_to_ticks(infinite),
            u64::MAX
        );
        let arm_frequency = 62_500_000;
        assert_eq!(
            Timebase::new(arm_frequency).duration_to_ticks(infinite),
            ((u64::MAX as u128 / 1_000_000_000) * arm_frequency as u128) as u64
                + (u64::MAX as u128 % 1_000_000_000 * arm_frequency as u128 / 1_000_000_000) as u64
        );
        assert_eq!(
            Timebase::new(GIGAHERTZ).duration_to_ticks(Duration::from_secs(1)),
            GIGAHERTZ
        );
    }

    #[test]
    fn tick_conversion_keeps_advancing_past_the_u64_nanosecond_product() {
        // A 1 GHz TSC timebase crosses `u64::MAX / 1e9` ticks after
        // roughly 18.4 s of uptime. The naive `ticks * 1e9` product
        // saturates there and pins every derived deadline to a
        // constant, so the conversion has to stay monotonic well past
        // that point.
        let timebase = Timebase::new(GIGAHERTZ);
        let cliff_ticks = u64::MAX / GIGAHERTZ;

        assert_eq!(timebase.ticks_to_nanos(cliff_ticks), cliff_ticks);
        assert_eq!(
            timebase.ticks_to_nanos(cliff_ticks + 1),
            cliff_ticks + 1,
            "conversion must not saturate one tick past the u64 product limit"
        );

        let hour_ticks = 3_600 * GIGAHERTZ;
        assert_eq!(timebase.ticks_to_nanos(hour_ticks), hour_ticks);
        assert!(
            timebase.ticks_to_nanos(hour_ticks + GIGAHERTZ) > timebase.ticks_to_nanos(hour_ticks),
            "conversion must stay strictly monotonic across long uptimes"
        );

        // A slower timebase reaches the same product limit later, which
        // is why only the TSC-backed backend hit this in practice.
        assert_eq!(
            Timebase::new(62_500_000).ticks_to_nanos(cliff_ticks),
            295_147_905_168
        );
    }

    #[test]
    #[should_panic(expected = "platform timer frequency must be non-zero")]
    fn zero_frequency_is_refused() {
        Timebase::new(0);
    }

    #[test]
    #[should_panic(expected = "platform timer frequency 18446744074 Hz exceeds")]
    fn frequency_above_the_exact_bound_is_refused() {
        Timebase::new(Timebase::MAX_FREQUENCY + 1);
    }

    /// The `u128` formulas the conversions replaced, kept here as the
    /// reference they must match bit for bit.
    fn reference_ticks_to_nanos(ticks: u64, frequency: u64) -> u64 {
        let nanos = (ticks as u128 * 1_000_000_000u128) / frequency as u128;
        u64::try_from(nanos).unwrap_or(u64::MAX)
    }

    fn reference_nanos_to_ticks_ceil_saturating(nanos: u64, frequency: u64) -> u64 {
        let numerator = (nanos as u128) * (frequency as u128);
        let ticks = numerator.saturating_add(999_999_999) / 1_000_000_000u128;
        u64::try_from(ticks).unwrap_or(u64::MAX)
    }

    fn reference_duration_to_ticks(duration: Duration, frequency: u64) -> u64 {
        let seconds = (duration.as_secs() as u128) * (frequency as u128);
        let subsec = ((duration.subsec_nanos() as u128) * (frequency as u128)) / 1_000_000_000u128;
        u64::try_from(seconds.saturating_add(subsec)).unwrap_or(u64::MAX)
    }

    const FREQUENCIES: [u64; 6] = [
        1,
        62_500_000,
        GIGAHERTZ,
        2_345_678_901,
        3_000_000_000,
        Timebase::MAX_FREQUENCY,
    ];

    /// SplitMix64: a deterministic stream of well-mixed `u64`s, so a
    /// failure reproduces.
    struct SplitMix64(u64);

    impl SplitMix64 {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        /// Draws across magnitudes rather than only near `u64::MAX`.
        fn next_scaled(&mut self) -> u64 {
            let value = self.next();
            value >> (self.next() % 64)
        }
    }

    /// Inputs around every boundary the decomposition splits at, for a
    /// value `unit` that divides the input (`frequency` for ticks, 1e9 for
    /// nanoseconds) and the input `cliff` where the result saturates.
    fn edge_inputs(unit: u64, cliff: u128) -> impl Iterator<Item = u64> {
        let around = |center: u128| {
            (-3i128..=3).filter_map(move |delta| u64::try_from(center as i128 + delta).ok())
        };
        let multiples = [
            1u128,
            2,
            1_000,
            3_600,
            86_400,
            u64::MAX as u128 / unit as u128,
        ]
        .into_iter()
        .flat_map(move |k| around(k * unit as u128));
        [0u64, 1, u64::MAX / GIGAHERTZ, u64::MAX - 1, u64::MAX]
            .into_iter()
            .chain(around(unit as u128))
            .chain(multiples)
            .chain(around(cliff))
            .chain(around(u64::MAX as u128 / GIGAHERTZ as u128))
    }

    #[test]
    fn conversions_match_the_u128_reference() {
        let mut rng = SplitMix64(0x6865_6c69_6f73_0441);
        for frequency in FREQUENCIES {
            let timebase = Timebase::new(frequency);
            assert_eq!(timebase.frequency(), frequency);

            // The tick count at which `ticks * 1e9 / f` first exceeds u64.
            let ticks_cliff = (u64::MAX as u128 + 1) * frequency as u128 / GIGAHERTZ as u128;
            let ticks =
                edge_inputs(frequency, ticks_cliff).chain((0..20_000).map(|_| rng.next_scaled()));
            for ticks in ticks.collect::<alloc::vec::Vec<_>>() {
                assert_eq!(
                    timebase.ticks_to_nanos(ticks),
                    reference_ticks_to_nanos(ticks, frequency),
                    "ticks_to_nanos({ticks}) at {frequency} Hz"
                );
            }

            // The nanosecond count at which `ceil(nanos * f / 1e9)` first
            // exceeds u64.
            let nanos_cliff = u64::MAX as u128 * GIGAHERTZ as u128 / frequency as u128;
            let nanos =
                edge_inputs(GIGAHERTZ, nanos_cliff).chain((0..20_000).map(|_| rng.next_scaled()));
            for nanos in nanos.collect::<alloc::vec::Vec<_>>() {
                assert_eq!(
                    timebase.nanos_to_ticks_ceil_saturating(nanos),
                    reference_nanos_to_ticks_ceil_saturating(nanos, frequency),
                    "nanos_to_ticks_ceil_saturating({nanos}) at {frequency} Hz"
                );
                let duration = Duration::from_nanos(nanos);
                assert_eq!(
                    timebase.duration_to_ticks(duration),
                    reference_duration_to_ticks(duration, frequency),
                    "duration_to_ticks({duration:?}) at {frequency} Hz"
                );
            }

            // Durations whose whole seconds exceed what nanoseconds in a
            // u64 can express, up to `Duration::MAX`.
            let seconds_cliff = u64::MAX / frequency;
            let seconds = [
                0,
                1,
                seconds_cliff,
                seconds_cliff.saturating_add(1),
                u64::MAX,
            ]
            .into_iter()
            .chain((0..20_000).map(|_| rng.next_scaled()))
            .collect::<alloc::vec::Vec<_>>();
            for seconds in seconds {
                let random_subsec =
                    u32::try_from(rng.next() % GIGAHERTZ).expect("a value below 1e9 fits u32");
                for subsec in [0, 1, 999_999_999, random_subsec] {
                    let duration = Duration::new(seconds, subsec);
                    assert_eq!(
                        timebase.duration_to_ticks(duration),
                        reference_duration_to_ticks(duration, frequency),
                        "duration_to_ticks({duration:?}) at {frequency} Hz"
                    );
                }
            }
        }
    }
}
