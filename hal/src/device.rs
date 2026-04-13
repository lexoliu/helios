use crate::interrupt::InterruptSource;

pub trait MemoryMappedDevice: Send + Sync + 'static {
    fn base_address(&self) -> usize;

    fn span_bytes(&self) -> usize;
}

pub trait InterruptRoutedDevice: MemoryMappedDevice {
    type Interrupt: InterruptSource;

    fn interrupt_source(&self) -> Self::Interrupt;
}
