#[derive(Clone, Copy)]
pub struct HartId(u16);

impl HartId {
    pub const fn new(id: u16) -> Self {
        Self(id)
    }

    pub const fn id(&self) -> u16 {
        self.0
    }
}

#[derive(Clone, Copy)]
pub struct Instant(u64);

impl Instant {
    pub const fn new(ticks: u64) -> Self {
        Self(ticks)
    }

    pub const fn ticks(&self) -> u64 {
        self.0
    }
}

pub trait Cpu {
    fn current_hart(&self) -> HartId;
    fn hart_count(&self) -> usize;
    fn bootstrap_hart(&self) -> HartId;

    fn park_current(&self);
    fn unpark(&self, hart: HartId);

    fn now(&self) -> Instant;
    fn set_deadline(&self, deadline: Instant);

    fn shutdown(&self) -> !;
    fn reboot(&self) -> !;
}
