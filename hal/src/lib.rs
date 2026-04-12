#![no_std]
extern crate alloc;
use core::fmt::Write;

use crate::memory::MemoryRegion;

pub mod cpu;
pub mod device;
pub mod fs;
pub mod interrupt;
pub mod io;
pub mod memory;
pub mod net;
pub mod resource;
pub mod serial;

/// Aligns `value` up to the next multiple of `align`.
///
/// Panics if the alignment would overflow.
pub const fn align_up(value: usize, align: usize) -> usize {
    let mask = align - 1;
    match value.checked_add(mask) {
        Some(next) => next & !mask,
        None => panic!("alignment overflow"),
    }
}

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
