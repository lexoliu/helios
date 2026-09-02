//! virtio-net over PCI for the x86 backend.
//!
//! Discovery, BAR mapping and MSI-X binding live in [`crate::pci`]; this
//! module only adapts the shared `helios-virtio` driver onto the kernel
//! `NetworkDevice` contract and installs the network service.
//!
//! Concurrency contract: the device is brought up on the bootstrap
//! processor before interrupts are unmasked, and its MSI-X message
//! targets that processor's local APIC. Afterwards the driver is shared
//! across processors through `Arc` and is internally synchronised.

extern crate alloc;

use alloc::sync::Arc;

use helios_hal::io::IoError;
use helios_hal::watchdog::Watchdog;
use helios_kernel::{
    ExternalInterruptHandler, InterfaceCapabilities, Kernel, LinkState, NetworkDevice,
    NetworkService, PacketBuffer,
};
use helios_virtio::{DeviceType, OffsetDmaPool, VirtioNetDevice, VirtioPciTransport};
use pci_types::PciAddress;

use crate::X86Cpu;
use crate::debug_state::RuntimeState;
use crate::pci::PciRoot;

type X86VirtioNetDevice = VirtioNetDevice<VirtioPciTransport<OffsetDmaPool>>;

#[derive(Clone)]
pub(crate) struct VirtioNetworkDevice {
    inner: Arc<X86VirtioNetDevice>,
}

/// The PCI function that carries the platform's virtio-net device.
pub(crate) fn discover(pci: &PciRoot) -> Option<PciAddress> {
    pci.find_virtio_function(DeviceType::Network)
}

/// Brings up the virtio-net function at `address` and installs the
/// kernel network service on top of it.
pub(crate) fn install<WatchdogImpl>(
    cpu: &X86Cpu,
    kernel: &Kernel<X86Cpu, WatchdogImpl>,
    pci: &PciRoot,
    address: PciAddress,
    physical_memory_offset: usize,
    vector: u8,
    destination_apic_id: u32,
    debug_state: &RuntimeState,
) -> VirtioNetworkDevice
where
    WatchdogImpl: Watchdog + Clone,
{
    let msix_vector = pci.bind_msix_vector(address, vector, destination_apic_id);
    let device = helios_virtio::net_from_pci(
        &pci.access(),
        address,
        pci,
        OffsetDmaPool::new(physical_memory_offset),
        Some(msix_vector),
    )
    .unwrap_or_else(|error| {
        panic!("failed to initialize the virtio-net function at {address}: {error}")
    });
    let device = VirtioNetworkDevice {
        inner: Arc::new(device),
    };
    let service = NetworkService::new(
        cpu.clone(),
        debug_state.clone(),
        kernel.timer(),
        device.clone(),
    );
    debug_state.install_network_service(helios_kernel::ComponentHostNetworkService::from_service(
        service,
    ));
    tracing::info!(
        "virtio network online transport=pci function={address} msix_vector={vector:#x}"
    );
    device
}

impl ExternalInterruptHandler for VirtioNetworkDevice {
    fn handle_interrupt(&self) {
        self.inner.handle_interrupt();
    }
}

impl NetworkDevice for VirtioNetworkDevice {
    fn mac_address(&self) -> [u8; 6] {
        self.inner.mac_address()
    }

    fn max_frame_len(&self) -> usize {
        self.inner.max_frame_len()
    }

    fn queue_pair_count(&self) -> usize {
        self.inner.queue_pair_count()
    }

    fn capabilities(&self) -> InterfaceCapabilities {
        self.inner.interface_capabilities()
    }

    fn link_state(&self) -> LinkState {
        self.inner.link_state()
    }

    async fn try_receive(&self, buffer: &mut PacketBuffer) -> Result<bool, IoError> {
        buffer.clear();
        let Some(frame_len) = self
            .inner
            .try_receive_into(buffer.spare_capacity_mut())
            .await?
        else {
            return Ok(false);
        };
        buffer.set_len(frame_len);
        Ok(true)
    }

    async fn try_receive_frame(&self) -> Result<Option<helios_virtio::RxFrame>, IoError> {
        self.inner.try_receive_frame().await
    }

    fn try_receive_frames_immediate<'a, 'slots>(
        &'a self,
        frames: &'slots mut [Option<helios_virtio::RxFrame>],
    ) -> Result<Option<usize>, IoError>
    where
        'a: 'slots,
    {
        self.inner.try_receive_frames_immediate(frames)
    }

    async fn repost_rx_frame<'a>(&'a self, frame: helios_virtio::RxFrame) -> Result<(), IoError> {
        self.inner.repost_rx_frame(frame).await
    }

    fn repost_rx_frames_immediate<'a, 'slots>(
        &'a self,
        frames: &'slots mut [Option<helios_virtio::RxFrame>],
    ) -> Result<Option<()>, IoError>
    where
        'a: 'slots,
    {
        self.inner.repost_rx_frames_immediate(frames)
    }

    async fn transmit(&self, frame: &[u8]) -> Result<(), IoError> {
        self.inner.transmit(frame).await
    }

    async fn try_transmit_packet_batch<'a>(
        &'a self,
        frames: &'a [PacketBuffer],
    ) -> Result<usize, IoError> {
        self.try_transmit_packet_batch_on(0, frames).await
    }

    async fn try_transmit_packet_batch_on<'a>(
        &'a self,
        queue_idx: usize,
        frames: &'a [PacketBuffer],
    ) -> Result<usize, IoError> {
        self.inner
            .try_transmit_frames_on_pair(queue_idx, frames)
            .await
    }

    fn try_receive_frames_immediate_on<'a, 'slots>(
        &'a self,
        queue_idx: usize,
        frames: &'slots mut [Option<helios_virtio::RxFrame>],
    ) -> Result<Option<usize>, IoError>
    where
        'a: 'slots,
    {
        self.inner
            .try_receive_frames_immediate_on_pair(queue_idx, frames)
    }

    fn try_transmit_scatter_immediate_on(
        &self,
        queue_idx: usize,
        frames: &[helios_kernel::TxFrameRef<'_>],
    ) -> Result<Option<usize>, IoError> {
        self.inner
            .try_transmit_scatter_immediate_on_pair(queue_idx, frames)
    }

    fn try_transmit_slices_immediate(
        &self,
        frames: &[helios_kernel::TxFrameRef<'_>],
    ) -> Result<Option<usize>, IoError> {
        self.try_transmit_slices_immediate_on(0, frames)
    }

    fn try_transmit_slices_immediate_on(
        &self,
        queue_idx: usize,
        frames: &[helios_kernel::TxFrameRef<'_>],
    ) -> Result<Option<usize>, IoError> {
        self.inner
            .try_transmit_trusted_frames_immediate_on_pair(queue_idx, frames)
    }

    async fn reclaim_transmit_completions_on(
        &self,
        queue_idx: usize,
        budget: usize,
    ) -> Result<usize, IoError> {
        self.inner
            .reclaim_transmit_completions_on_pair(queue_idx, budget)
            .await
    }

    fn reclaim_transmit_completions_immediate(
        &self,
        budget: usize,
    ) -> Result<Option<usize>, IoError> {
        self.reclaim_transmit_completions_immediate_on(0, budget)
    }

    fn reclaim_transmit_completions_immediate_on(
        &self,
        queue_idx: usize,
        budget: usize,
    ) -> Result<Option<usize>, IoError> {
        self.inner
            .reclaim_transmit_completions_immediate_on_pair(queue_idx, budget)
    }

    async fn wait_for_event(&self) {
        self.inner.wait_for_interrupt().await;
    }
}
