use fdt::Fdt;
use helios_hal::io::IoError;
use helios_hal::watchdog::Watchdog;
use helios_kernel::{
    EventDeliveryCapabilities, InterfaceCapabilities, Kernel, NetworkDevice, NetworkService,
    PacketBuffer,
};
use triomphe::Arc;

type Aarch64VirtioNetTransport =
    helios_virtio::VirtioMmioTransport<helios_virtio::MmioBus<helios_virtio::OffsetDmaPool>>;
type Aarch64VirtioNetDevice = helios_virtio::VirtioNetDevice<Aarch64VirtioNetTransport>;
const VIRTIO_POLLING_RX_BUDGET: usize = 32;

#[derive(Clone)]
struct VirtioNetworkDevice {
    inner: Arc<Aarch64VirtioNetDevice>,
}

pub(crate) fn install<WatchdogImpl>(
    cpu: &crate::Aarch64Cpu,
    kernel: &Kernel<crate::Aarch64Cpu, WatchdogImpl>,
    fdt: Option<&Fdt<'_>>,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
    debug_state: &crate::debug_state::RuntimeState,
) where
    WatchdogImpl: Watchdog + Clone,
{
    let Some(device) = discover_network_device(fdt, physical_memory_offset, handoff) else {
        tracing::warn!("virtio network device was not discovered on the platform bus");
        return;
    };
    let service = NetworkService::new(cpu.clone(), debug_state.clone(), kernel.timer(), device);
    let packet_pump = service.clone();
    debug_state.install_network_service(helios_kernel::ComponentHostNetworkService::from_service(
        service,
    ));
    kernel.spawn_detached(async move {
        packet_pump.run_packet_pump().await;
    });
    tracing::info!("virtio network online");
}

pub(crate) fn has_network_device(
    fdt: Option<&Fdt<'_>>,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
) -> bool {
    if fdt.is_some_and(|fdt| {
        crate::count_virtio_mmio_devices(fdt, helios_virtio::DeviceType::Network) != 0
    }) {
        return true;
    }
    scan_qemu_virt_mmio().any(|physical_base| {
        crate::matches_virtio_mmio_device(
            physical_base,
            physical_memory_offset,
            handoff,
            helios_virtio::DeviceType::Network,
        )
    })
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
        InterfaceCapabilities {
            max_frame_len: self.max_frame_len(),
            events: EventDeliveryCapabilities {
                polling: true,
                interrupts: false,
                adaptive_moderation: false,
                rx_coalescing: false,
                tx_coalescing: false,
                rx_poll_budget: VIRTIO_POLLING_RX_BUDGET,
                tx_completion_budget: VIRTIO_POLLING_RX_BUDGET,
            },
            ..InterfaceCapabilities::default()
        }
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
        self.inner
            .transmit_with_wait(frame, helios_kernel::yield_now)
            .await
    }

    async fn transmit_batch<'a>(&'a self, frames: &'a [&'a [u8]]) -> Result<(), IoError> {
        self.inner
            .transmit_batch_with_wait(frames, helios_kernel::yield_now)
            .await
    }

    async fn transmit_packet_batch<'a>(
        &'a self,
        frames: &'a [PacketBuffer],
    ) -> Result<(), IoError> {
        for chunk in frames.chunks(VIRTIO_POLLING_RX_BUDGET) {
            self.inner
                .transmit_frames_with_wait(chunk, helios_kernel::yield_now)
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
        for chunk in frames.chunks(VIRTIO_POLLING_RX_BUDGET) {
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

    fn try_transmit_slices_immediate(&self, frames: &[&[u8]]) -> Result<Option<usize>, IoError> {
        self.try_transmit_slices_immediate_on(0, frames)
    }

    fn try_transmit_slices_immediate_on(
        &self,
        queue_idx: usize,
        frames: &[&[u8]],
    ) -> Result<Option<usize>, IoError> {
        let mut submitted = 0usize;
        for chunk in frames.chunks(VIRTIO_POLLING_RX_BUDGET) {
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
        helios_kernel::yield_now().await;
    }
}

fn discover_network_device(
    fdt: Option<&Fdt<'_>>,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
) -> Option<VirtioNetworkDevice> {
    if let Some(fdt) = fdt {
        for node in fdt.all_nodes() {
            if !node
                .compatible()
                .is_some_and(|compatible| compatible.all().any(|entry| entry == "virtio,mmio"))
            {
                continue;
            }

            let Some(region) = node.raw_reg().and_then(|mut regs| regs.next()) else {
                continue;
            };
            let physical_base =
                crate::fdt_cells_to_usize(region.address, "AArch64 virtio MMIO base");
            if !crate::matches_virtio_mmio_device(
                physical_base,
                physical_memory_offset,
                handoff,
                helios_virtio::DeviceType::Network,
            ) {
                continue;
            }
            let size = crate::fdt_cells_to_usize(region.size, "AArch64 virtio MMIO size");
            return Some(init_network_device(
                physical_base,
                size,
                physical_memory_offset,
                handoff,
            ));
        }
    }

    scan_qemu_virt_mmio()
        .find(|physical_base| {
            crate::matches_virtio_mmio_device(
                *physical_base,
                physical_memory_offset,
                handoff,
                helios_virtio::DeviceType::Network,
            )
        })
        .map(|physical_base| {
            init_network_device(
                physical_base,
                crate::QEMU_VIRT_MMIO_SIZE,
                physical_memory_offset,
                handoff,
            )
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

fn scan_qemu_virt_mmio() -> impl Iterator<Item = usize> {
    (0..crate::QEMU_VIRT_MMIO_SLOTS)
        .map(|slot| crate::QEMU_VIRT_MMIO_BASE + slot * crate::QEMU_VIRT_MMIO_STRIDE)
}
