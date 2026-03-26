#![no_std]

use core::fmt::Write;

use crate::memory::MemoryRegion;

pub mod cpu;
pub mod memory;
pub mod timer;
pub struct Platform<Console: Write + Send + 'static, Cpu: cpu::Cpu, Regions>
where
    Regions: IntoIterator<Item = MemoryRegion>,
{
    pub console: Console,
    pub cpu: Cpu,
    pub memory_regions: Regions,
}

impl<Console: Write + Send + 'static, Cpu: cpu::Cpu, Regions> Platform<Console, Cpu, Regions>
where
    Regions: IntoIterator<Item = MemoryRegion>,
{
    pub const fn new(console: Console, memory_regions: Regions, cpu: Cpu) -> Self {
        Self {
            console,
            memory_regions,
            cpu,
        }
    }
}
