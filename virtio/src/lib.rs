#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;

mod block;
mod bus;
mod discovery;
mod features;
mod inflight;
mod iommu;
mod mmio;
mod net;
mod notify;
mod p9;
mod pci;
mod queue;
mod rng;
#[cfg(test)]
mod testing;
mod transport;

pub use block::{
    BlockRequestCounts, QueueAffinity, SECTOR_SIZE, VirtioBlockDevice, VirtioBlockResource,
    VirtioBlockSwapBackend, VirtioBlockSwapError, VirtioBlockSwapToken,
};
pub use bus::{
    DeviceBus, DmaAddressing, DmaBuffer, DmaPool, IdentityDmaBuffer, IdentityDmaPool, MmioBus,
    OffsetDmaBuffer, OffsetDmaPool, PlatformDmaBuffer, PlatformDmaPool,
};
pub use discovery::{
    InterruptTrigger, MmioCandidate, MmioInterrupt, mmio_candidates, mmio_device_matches,
};
pub use features::{NegotiatedFeatures, RING_FEATURES, negotiate, negotiate_with};
pub use iommu::{MAX_RESERVED_REGIONS, ReservedRegion, VirtioIommuDevice};
pub use mmio::{
    VirtioMmio9pDevice, VirtioMmioBlockDevice, VirtioMmioNetDevice, VirtioMmioRngDevice,
    block_from_mmio, block_from_mmio_with_dma, net_from_mmio, net_from_mmio_with_dma, p9_from_mmio,
    p9_from_mmio_with_dma, rng_from_mmio, rng_from_mmio_with_dma,
};
pub use net::{RxFrame, TxChecksumMeta, TxGsoMeta, VirtioNetDevice};
pub use p9::Virtio9pDevice;
pub use pci::{
    PciMmioMapper, VIRTIO_PCI_VENDOR_ID, VirtioPciBus, VirtioPciTransport, block_from_pci,
    iommu_from_pci, net_from_pci, p9_from_pci, rng_from_pci, virtio_pci_device_type,
};
pub use queue::{MAX_CHAIN_BUFFERS, VirtQueue, VirtqueueError};
pub use rng::VirtioRngDevice;
pub use transport::{
    DeviceStatus, DeviceType, VirtioFeatures, VirtioMmioTransport, VirtioTransport,
};
