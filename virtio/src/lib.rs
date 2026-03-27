#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;

mod block;
mod error;
mod mmio;

pub use block::VirtioBlockDevice;
pub use block::VirtioBlockResource;
pub use mmio::{VirtioMmioBlockDevice, block_from_mmio};
