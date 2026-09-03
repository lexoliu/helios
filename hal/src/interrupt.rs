pub trait InterruptSource: Copy + Eq + Send + Sync + 'static {
    fn raw(self) -> u32;
}

pub trait InterruptContext: Copy + Send + Sync + 'static {
    fn raw(self) -> u32;
}

pub trait InterruptController: Send + Sync + 'static {
    type Source: InterruptSource;
    type Context: InterruptContext;

    fn set_priority(&self, source: Self::Source, priority: u32);

    fn enable(&self, source: Self::Source, context: Self::Context);

    fn set_threshold(&self, context: Self::Context, threshold: u32);

    fn claim(&self, context: Self::Context) -> Option<Self::Source>;

    fn complete(&self, context: Self::Context, source: Self::Source);
}

/// Per-source masking, for an interrupt whose handler does not run in
/// the kernel.
///
/// A kernel driver never needs this: its handler runs to completion
/// inside the interrupt, so the line is quiet again by the time the
/// processor returns. A driver that lives somewhere else does — the
/// kernel handler can only note that the interrupt arrived, and the
/// line stays asserted until the driver reaches its device, which
/// happens on some other schedule entirely. Masking is what stops the
/// processor spinning in the meantime.
///
/// Separate from [`InterruptController`] because it is a different
/// question: that trait is about claiming and completing an interrupt
/// the kernel is handling now, this one is about leaving a source off
/// until someone else has dealt with it.
pub trait MaskableInterrupts: Send + Sync + 'static {
    type Source: InterruptSource;

    /// Stop the controller signalling `source` to any processor.
    /// Idempotent: masking an already-masked source is not an error.
    fn mask(&self, source: Self::Source);

    /// Let `source` signal again.
    fn unmask(&self, source: Self::Source);
}
