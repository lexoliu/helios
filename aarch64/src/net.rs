use arm_gic::{IntId, Trigger};
use fdt::Fdt;
use helios_hal::io::IoError;
use helios_hal::watchdog::Watchdog;
use helios_kernel::{
    ExternalInterruptHandler, InterfaceCapabilities, Kernel, LinkState, NetworkDevice,
    NetworkService, PacketBuffer,
};
use triomphe::Arc;

type Aarch64VirtioNetTransport =
    helios_virtio::VirtioMmioTransport<helios_virtio::MmioBus<helios_virtio::OffsetDmaPool>>;
type Aarch64VirtioNetDevice = helios_virtio::VirtioNetDevice<Aarch64VirtioNetTransport>;
/// Frames submitted to the device in one descriptor-chain batch. This
/// bounds the stack space the scatter path uses, independent of the
/// receive poll budget the interrupt-driven capabilities advertise.
const TX_BATCH_FRAMES: usize = 32;

#[derive(Clone)]
pub(crate) struct VirtioNetworkDevice {
    inner: Arc<Aarch64VirtioNetDevice>,
}

impl ExternalInterruptHandler for VirtioNetworkDevice {
    fn handle_interrupt(&self) {
        self.inner.handle_interrupt();
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
    fdt: &Fdt<'_>,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
    debug_state: &crate::debug_state::RuntimeState,
) -> Option<NetworkInterrupt>
where
    WatchdogImpl: Watchdog + Clone,
{
    let Some(network) = discover_network_device(fdt, physical_memory_offset, handoff) else {
        tracing::warn!("virtio network device was not discovered on the platform bus");
        return None;
    };
    let service = NetworkService::new(
        *cpu,
        debug_state.clone(),
        kernel.timer(),
        network.device.clone(),
    );
    let packet_pump = service.clone();
    debug_state.install_network_service(helios_kernel::ComponentHostNetworkService::from_service(
        service,
    ));
    kernel.spawn_detached(async move {
        packet_pump.run_packet_pump().await;
    });
    tracing::info!("virtio network online interrupt={:?}", network.interrupt);
    Some(network)
}

pub(crate) fn has_network_device(fdt: &Fdt<'_>) -> bool {
    crate::count_virtio_mmio_devices(fdt, helios_virtio::DeviceType::Network) != 0
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

    async fn transmit_batch<'a>(&'a self, frames: &'a [&'a [u8]]) -> Result<(), IoError> {
        self.inner.transmit_batch(frames).await
    }

    async fn transmit_packet_batch<'a>(
        &'a self,
        frames: &'a [PacketBuffer],
    ) -> Result<(), IoError> {
        for chunk in frames.chunks(TX_BATCH_FRAMES) {
            self.inner
                .transmit_frames_with_wait(chunk, || self.inner.wait_for_interrupt())
                .await?;
        }
        Ok(())
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
        let mut submitted = 0usize;
        for chunk in frames.chunks(TX_BATCH_FRAMES) {
            let accepted = self
                .inner
                .try_transmit_frames_on_pair(queue_idx, chunk)
                .await?;
            submitted += accepted;
            if accepted < chunk.len() {
                break;
            }
        }
        Ok(submitted)
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
        tokens: &mut [Option<u16>],
    ) -> Result<Option<usize>, IoError> {
        let mut descriptors = [helios_virtio::TxScatterFrame {
            headers: &[],
            payload: &[],
            checksum: None,
        }; TX_BATCH_FRAMES];
        let count = frames.len().min(TX_BATCH_FRAMES);
        for (descriptor, frame) in descriptors.iter_mut().zip(frames.iter().take(count)) {
            *descriptor = helios_virtio::TxScatterFrame {
                headers: frame.bytes,
                payload: frame.payload.map(|payload| payload.as_ref()).unwrap_or(&[]),
                checksum: frame
                    .checksum
                    .map(|checksum| helios_virtio::TxChecksumMeta {
                        start: checksum.start,
                        offset: checksum.offset,
                    }),
            };
        }
        self.inner
            .try_transmit_scatter_immediate_on_pair(queue_idx, &descriptors[..count], tokens)
    }

    fn reclaim_transmit_tokens_immediate_on(
        &self,
        queue_idx: usize,
        tokens: &mut [Option<u16>],
    ) -> Result<Option<usize>, IoError> {
        self.inner
            .reclaim_transmit_tokens_immediate_on_pair(queue_idx, tokens)
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
        let mut submitted = 0usize;
        for chunk in frames.chunks(TX_BATCH_FRAMES) {
            let Some(accepted) = self
                .inner
                .try_transmit_trusted_frames_immediate_on_pair(queue_idx, chunk)?
            else {
                return Ok((submitted != 0).then_some(submitted));
            };
            submitted += accepted;
            if accepted < chunk.len() {
                break;
            }
        }
        Ok(Some(submitted))
    }

    async fn reclaim_transmit_completions(&self, budget: usize) -> Result<usize, IoError> {
        self.reclaim_transmit_completions_on(0, budget).await
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

fn discover_network_device(
    fdt: &Fdt<'_>,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
) -> Option<NetworkInterrupt> {
    let candidate = helios_virtio::mmio_candidates(fdt).find(|candidate| {
        crate::matches_virtio_mmio_device(
            candidate.base,
            physical_memory_offset,
            handoff,
            helios_virtio::DeviceType::Network,
        )
    })?;
    let (interrupt, trigger) = crate::gic::device_interrupt(candidate.interrupt, candidate.base);
    Some(NetworkInterrupt {
        interrupt,
        trigger,
        device: init_network_device(
            candidate.base,
            candidate.size,
            physical_memory_offset,
            handoff,
        ),
    })
}

fn init_network_device(
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
    }
}
