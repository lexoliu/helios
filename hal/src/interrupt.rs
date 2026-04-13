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
