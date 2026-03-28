#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;

mod block;
mod bus;
mod mmio;
mod notify;
mod queue;
mod transport;

pub use block::VirtioBlockDevice;
pub use block::VirtioBlockResource;
pub use bus::{DeviceBus, DmaBuffer, DmaPool, IdentityDmaBuffer, IdentityDmaPool, MmioBus};
pub use mmio::{VirtioMmioBlockDevice, block_from_mmio};
pub use transport::{
    DeviceStatus, DeviceType, VirtioFeatures, VirtioMmioTransport, VirtioTransport,
};
