#![no_std]
extern crate alloc;
use core::fmt::Write;

use crate::memory::MemoryRegion;

pub mod cpu;
pub mod fs;
pub mod io;
pub mod memory;
pub mod net;
pub mod resource;
pub mod serial;

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
