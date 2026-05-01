#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;

mod block;
mod bus;
mod console;
mod mmio;
mod net;
mod notify;
mod p9;
mod queue;
mod rng;
mod transport;

pub use block::{
    VirtioBlockDevice, VirtioBlockResource, VirtioBlockSwapBackend, VirtioBlockSwapError,
    VirtioBlockSwapToken,
};
pub use bus::{
    DeviceBus, DmaBuffer, DmaPool, IdentityDmaBuffer, IdentityDmaPool, MmioBus, OffsetDmaBuffer,
    OffsetDmaPool,
};
pub use console::VirtioConsoleDevice;
pub use mmio::{
    VirtioMmio9pDevice, VirtioMmioBlockDevice, VirtioMmioConsoleDevice, VirtioMmioNetDevice,
    VirtioMmioRngDevice, block_from_mmio, console_from_mmio, net_from_mmio, p9_from_mmio,
    p9_from_mmio_with_dma, rng_from_mmio,
};
pub use net::VirtioNetDevice;
pub use p9::Virtio9pDevice;
pub use rng::VirtioRngDevice;
pub use transport::{
    DeviceStatus, DeviceType, VirtioFeatures, VirtioMmioTransport, VirtioTransport,
};
