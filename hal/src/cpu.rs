/// Logical hardware thread identifier.
///
/// In the RISC-V backend this maps to a hart id. In hosted mode it is the
/// synthetic CPU slot used by the test runtime.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HartId(u16);

impl HartId {
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

pub trait Cpu {
    /// Returns the hart currently executing this code path.
    ///
    /// This must be cheap and stable for the lifetime of the `Cpu` value because
    /// the kernel queries it during boot, scheduling, and panic reporting.
    fn current_hart(&self) -> HartId;

    /// Returns the number of harts the platform exposes to the kernel.
    ///
    /// The kernel uses this to decide which secondary harts to start during SMP
    /// bootstrap. This is a platform capability, not a scheduler state query.
    fn hart_count(&self) -> usize;

    /// Returns the hart designated as the bootstrap hart.
    ///
    /// Exactly one hart performs one-time global initialization such as heap and
    /// logger setup; all other harts wait until that work is complete.
    fn bootstrap_hart(&self) -> HartId;

    /// Parks the current hart until some external event makes forward progress
    /// possible again.
    ///
    /// Typical implementations are `wfi` on bare metal and `thread::park()` in
    /// hosted mode. The contract is only "stop burning CPU until woken", not any
    /// stronger fairness guarantee.
    fn park_current(&self);

    /// Starts execution on a secondary hart.
    ///
    /// The kernel calls this during bootstrap for every hart other than the
    /// bootstrap hart. Implementations may panic if the platform cannot start the
    /// requested hart, because that is a real bring-up failure.
    fn start_hart(&self, hart: HartId);

    /// Returns the current monotonic time in platform timer ticks.
    fn now(&self) -> Instant;

    /// Returns the number of timer ticks that elapse per second.
    ///
    /// The async timer layer uses this to translate Rust `Duration` values into
    /// backend timer ticks without hardcoding any platform frequency in kernel
    /// code.
    fn timer_frequency(&self) -> u64;

    /// Programs the next wakeup deadline for the current hart.
    ///
    /// The deadline is absolute, in the same tick domain as [`Cpu::now`]. Calling
    /// this again replaces the previous deadline.
    fn set_deadline(&self, deadline: Instant);

    /// Powers the machine off and never returns.
    fn shutdown(&self) -> !;

    /// Reboots the machine and never returns.
    fn reboot(&self) -> !;
}
