use crate::platform::PlatformDescription;
use arm_gic::{IntId, Trigger};
use helios_hal::io::IoError;
use helios_hal::watchdog::Watchdog;
use helios_kernel::{
    ExternalInterruptHandler, InterfaceCapabilities, InterfaceEventMark, Kernel, LinkState,
    NetworkDevice, PacketBuffer,
};
use triomphe::Arc;

type Aarch64VirtioNetTransport =
    helios_virtio::VirtioMmioTransport<helios_virtio::MmioBus<helios_virtio::OffsetDmaPool>>;
type Aarch64VirtioNetDevice = helios_virtio::VirtioNetDevice<Aarch64VirtioNetTransport>;

#[derive(Clone)]
pub(crate) struct VirtioNetworkDevice {
    inner: Arc<Aarch64VirtioNetDevice>,
    /// Held so the interrupt handler can steer by IPI.
    ///
    /// The device tree routes this device's single SPI to one
    /// processor, so every queue pair's completions are noticed there
    /// whichever processor owns them. Waking the owners is the only
    /// steering a single-line transport can do.
    cpu: crate::Aarch64Cpu,
}

impl ExternalInterruptHandler for VirtioNetworkDevice {
    fn handle_interrupt(&self) {
        helios_kernel::wake_queue_owners(&self.cpu, self.inner.handle_interrupt().iter());
    }
}

/// The network device the bootstrap processor brought up, together with
/// the interrupt the device tree routes it to.
pub(crate) struct NetworkInterrupt {
    pub(crate) interrupt: IntId,
    pub(crate) trigger: Trigger,
    pub(crate) device: VirtioNetworkDevice,
}

pub(crate) fn install<WatchdogImpl>(
    cpu: &crate::Aarch64Cpu,
    kernel: &Kernel<crate::Aarch64Cpu, WatchdogImpl>,
    platform: &PlatformDescription,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
    debug_state: &crate::debug_state::RuntimeState,
) -> Option<NetworkInterrupt>
where
    WatchdogImpl: Watchdog + Clone,
{
    let Some(network) = discover_network_device(cpu, platform, physical_memory_offset, handoff)
    else {
        tracing::warn!("virtio network device was not discovered on the platform bus");
        return None;
    };
    kernel.install_network_interface(debug_state, network.device.clone());
    tracing::info!("virtio network online interrupt={:?}", network.interrupt);
    Some(network)
}

pub(crate) fn has_network_device(platform: &PlatformDescription) -> bool {
    crate::count_virtio_mmio_devices(platform, helios_virtio::DeviceType::Network) != 0
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

    fn event_mark(&self, queue_idx: usize) -> InterfaceEventMark {
        self.inner.interrupt_mark(queue_idx)
    }

    fn wait_for_event_since(
        &self,
        queue_idx: usize,
        mark: InterfaceEventMark,
    ) -> impl core::future::Future<Output = ()> + Send + '_ {
        // Not an `async fn`: the driver arms both listeners against
        // `mark` when this is called, and an `async fn` body would defer
        // that to the first poll, which is exactly the window the mark
        // exists to close.
        self.inner.wait_for_interrupt_since(queue_idx, mark)
    }

    fn queue_interrupts(&self, queue_idx: usize) -> u64 {
        self.inner.queue_interrupts(queue_idx)
    }
}

fn discover_network_device(
    cpu: &crate::Aarch64Cpu,
    platform: &PlatformDescription,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
) -> Option<NetworkInterrupt> {
    let candidate = crate::virtio_slots(
        platform,
        physical_memory_offset,
        handoff,
        helios_virtio::DeviceType::Network,
    )
    .next()?;
    let (interrupt, trigger) = (candidate.interrupt.intid(), candidate.interrupt.trigger);
    Some(NetworkInterrupt {
        interrupt,
        trigger,
        device: init_network_device(
            cpu,
            candidate.region.base,
            candidate.region.size,
            physical_memory_offset,
            handoff,
        ),
    })
}

fn init_network_device(
    cpu: &crate::Aarch64Cpu,
    physical_base: usize,
    size: usize,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
) -> VirtioNetworkDevice {
    assert!(size != 0, "AArch64 virtio-net node has zero MMIO size");
    crate::map_mmio_page(physical_base, physical_memory_offset, handoff);
    let virtual_base = crate::mmio_virtual_base(physical_base, physical_memory_offset);
    let header = core::ptr::NonNull::new(virtual_base as *mut u8)
        .unwrap_or_else(|| panic!("virtio MMIO base {virtual_base:#x} was unexpectedly null"));
    let dma = helios_virtio::OffsetDmaPool::new(physical_memory_offset);
    let device = unsafe { helios_virtio::net_from_mmio_with_dma(header, size, dma) }
        .unwrap_or_else(|error| {
            panic!("failed to initialize virtio-net device at {physical_base:#x}: {error}")
        });
    VirtioNetworkDevice {
        inner: Arc::new(device),
        cpu: *cpu,
    }
}
