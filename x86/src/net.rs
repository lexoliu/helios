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
use helios_virtio::{DeviceType, VirtioNetDevice, VirtioPciTransport};
use pci_types::PciAddress;

use helios_hal::cpu::{Cpu, ProcessorId};

use crate::X86Cpu;
use crate::debug_state::RuntimeState;
use crate::iommu::X86DmaPool;
use crate::pci::MsixMessage;
use crate::pci::PciRoot;

type X86VirtioNetDevice = VirtioNetDevice<VirtioPciTransport<X86DmaPool>>;

#[derive(Clone)]
pub(crate) struct VirtioNetworkDevice {
    inner: Arc<X86VirtioNetDevice>,
    /// Which of the device's MSI-X messages this handle answers.
    ///
    /// The device is shared; the role is not. One clone per message is
    /// registered in the interrupt routing table, so dispatch is a table
    /// lookup and the handler knows what it was woken for without
    /// scanning anything.
    role: InterruptRole,
}

/// What a network interrupt message means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InterruptRole {
    /// A configuration change — a carrier transition, in practice.
    Configuration,
    /// One queue pair's completions, delivered by MSI-X straight to the
    /// processor whose shard drains that pair. Nothing has to be woken:
    /// the message already arrived where the work is.
    QueuePair(usize),
}

/// The PCI function that carries the platform's virtio-net device.
pub(crate) fn discover(pci: &PciRoot) -> Option<PciAddress> {
    pci.find_virtio_function(DeviceType::Network)
}

/// The PCI function a virtio device is brought up on, and the DMA pool
/// its rings are allocated from.
pub(crate) struct PciFunction<'a> {
    pub(crate) root: &'a PciRoot,
    pub(crate) address: PciAddress,
    pub(crate) dma: X86DmaPool,
}

/// Where a device's fallback MSI-X message is delivered: the vector it
/// raises and the local APIC that takes it.
pub(crate) struct MsixDelivery {
    pub(crate) vector: u8,
    pub(crate) destination_apic_id: u32,
}

/// Brings up the virtio-net function and installs the kernel network
/// service on top of it.
pub(crate) fn install<WatchdogImpl>(
    cpu: &X86Cpu,
    kernel: &Kernel<X86Cpu, WatchdogImpl>,
    function: PciFunction<'_>,
    delivery: MsixDelivery,
    debug_state: &RuntimeState,
) -> NetworkInterrupts
where
    WatchdogImpl: Watchdog + Clone,
{
    // One message per queue pair, each delivered to the local APIC of
    // the processor whose shard drains that pair, plus one for
    // configuration changes. This is the point of the whole steering
    // path: a flow the device puts on pair `i` raises an interrupt on
    // the processor that already owns the socket.
    //
    // How many of those the machine actually gets is the device's
    // decision, not ours. QEMU sizes a virtio-net function's MSI-X
    // table from the queue pairs it was configured with — a
    // single-queue device offers four entries — so a host with more
    // processors than the device has table entries steers as far as the
    // table goes and no further. Sizing this from the processor count
    // alone is what made a default `virtio-net-pci` panic the kernel
    // during bring-up.
    let PciFunction {
        root: pci,
        address,
        dma,
    } = function;
    let MsixDelivery {
        vector,
        destination_apic_id,
    } = delivery;
    let queue_vectors = crate::exceptions::NETWORK_QUEUE_INTERRUPT_VECTORS;
    let steerable = usize::from(pci.msix_table_size(address)).saturating_sub(1);
    let steered = cpu
        .processor_count()
        .min(queue_vectors.len())
        .min(steerable);
    let mut messages = [MsixMessage {
        vector,
        destination_apic_id,
    }; crate::exceptions::MAX_NETWORK_QUEUE_VECTORS + 1];
    for (index, message) in messages[1..=steered].iter_mut().enumerate() {
        *message = MsixMessage {
            vector: queue_vectors[index],
            destination_apic_id: cpu.apic_id_of_processor(ProcessorId::new(index as u16)),
        };
    }
    let msix = pci.bind_msix_vectors(address, &messages[..=steered]);
    // A table with nothing left over after the configuration entry
    // cannot steer at all: every structure of the function shares the
    // one message, which is what `shared` describes.
    let binding = if steered == 0 {
        helios_virtio::MsixBinding::shared(msix)
    } else {
        helios_virtio::MsixBinding::per_queue(
            msix,
            msix + 1,
            u16::try_from(steered).unwrap_or_else(|_| panic!("{steered} queue vectors exceed u16")),
        )
    };
    let device = helios_virtio::net_from_pci(&pci.access(), address, pci, dma, Some(binding))
        .unwrap_or_else(|error| {
            panic!("failed to initialize the virtio-net function at {address}: {error}")
        });
    let device = Arc::new(device);
    let queue_pairs = device.queue_pair_count();
    let configuration = VirtioNetworkDevice {
        inner: device.clone(),
        role: InterruptRole::Configuration,
    };
    let service = NetworkService::new(
        cpu.clone(),
        debug_state.clone(),
        kernel.timer(),
        configuration.clone(),
    );
    debug_state.install_network_service(helios_kernel::ComponentHostNetworkService::from_service(
        service,
    ));
    tracing::info!(
        queue_pairs,
        steered_queue_vectors = steered,
        config_vector = vector,
        "virtio network online transport=pci function={address}"
    );
    NetworkInterrupts {
        // A pair past the vectors this backend hands out shares the last
        // one, so its route is the one already installed for that
        // vector; only the pairs with a vector of their own get a route.
        queues: (0..steered.min(queue_pairs))
            .map(|pair_idx| {
                (
                    queue_vectors[pair_idx],
                    VirtioNetworkDevice {
                        inner: device.clone(),
                        role: InterruptRole::QueuePair(pair_idx),
                    },
                )
            })
            .collect(),
        configuration,
    }
}

/// Every interrupt message the network device raises, with the IDT
/// vector each one arrives on.
pub(crate) struct NetworkInterrupts {
    pub(crate) queues: alloc::vec::Vec<(u8, VirtioNetworkDevice)>,
    pub(crate) configuration: VirtioNetworkDevice,
}

impl ExternalInterruptHandler for VirtioNetworkDevice {
    fn handle_interrupt(&self) {
        match self.role {
            InterruptRole::Configuration => self.inner.handle_configuration_interrupt(),
            InterruptRole::QueuePair(pair_idx) => self.inner.handle_interrupt_on(pair_idx),
        }
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

    async fn repost_rx_frame(&self, frame: helios_virtio::RxFrame) -> Result<(), IoError> {
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

    async fn wait_for_event_on(&self, queue_idx: usize) {
        self.inner.wait_for_interrupt_on(queue_idx).await;
    }

    fn queue_interrupts(&self, queue_idx: usize) -> u64 {
        self.inner.queue_interrupts(queue_idx)
    }
}
