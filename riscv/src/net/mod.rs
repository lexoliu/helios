extern crate alloc;

use alloc::sync::Arc;
use core::num::NonZeroU32;

use fdt::Fdt;
use fdt::node::FdtNode;
use helios_hal::io::IoError;
use helios_hal::watchdog::Watchdog;
use helios_kernel::{
    ExternalInterruptHandler, ExternalInterruptRoutes, InterfaceCapabilities, Kernel, LinkState,
    NetworkDevice, NetworkService, PacketBuffer,
};
use plic::Plic;

use crate::RiscvCpu;
use crate::debug_state::RuntimeState;

const SUPERVISOR_EXTERNAL_INTERRUPT: u32 = 9;

type RiscvVirtioNetTransport = helios_virtio::VirtioMmioTransport<helios_virtio::MmioBus>;
type RiscvVirtioNetDevice = helios_virtio::VirtioNetDevice<RiscvVirtioNetTransport>;

#[derive(Clone)]
struct VirtioNetworkDevice {
    inner: Arc<RiscvVirtioNetDevice>,
    /// Held so the interrupt handler can steer by IPI.
    ///
    /// The PLIC source is enabled on the bootstrap hart's context
    /// alone, so every queue pair's completions are noticed there
    /// whichever hart owns them. Waking the owners is the only steering
    /// a single-source transport can do.
    cpu: RiscvCpu,
}

pub(crate) struct ExternalInterrupts {
    plic: &'static Plic,
    context: PlicContext,
    routes: ExternalInterruptRoutes<
        InterruptSourceId,
        VirtioNetworkDevice,
        crate::host_fs::HostFsTransportService,
        crate::entropy::VirtioEntropyDevice,
        crate::balloon::VirtioBalloonInterrupt,
        crate::vsock::VirtioVsockDevice,
        crate::block::VirtioBlockDevice,
    >,
}

impl ExternalInterruptHandler for VirtioNetworkDevice {
    fn handle_interrupt(&self) {
        helios_kernel::wake_queue_owners(&self.cpu, self.inner.handle_interrupt().iter());
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PlicContext(pub(crate) usize);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct InterruptSourceId(pub(crate) NonZeroU32);

/// The network device the bootstrap hart brought up, together with the
/// PLIC source the device tree routes it to.
pub(crate) struct NetworkInterrupt {
    source: InterruptSourceId,
    device: VirtioNetworkDevice,
}

pub(crate) fn install_network_service<WatchdogImpl>(
    cpu: &RiscvCpu,
    kernel: &Kernel<RiscvCpu, WatchdogImpl>,
    fdt: &Fdt<'_>,
    debug_state: &RuntimeState,
) -> Option<NetworkInterrupt>
where
    WatchdogImpl: Watchdog + Clone,
{
    let Some((device, source)) = discover_network_device(cpu, fdt) else {
        tracing::warn!("virtio network device was not discovered on the platform bus");
        return None;
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
    tracing::info!("virtio network online irq={}", source.0.get());
    Some(NetworkInterrupt { source, device })
}

impl ExternalInterrupts {
    /// Opens the bootstrap hart's PLIC context. Devices attach to it as
    /// they come up; a source with no route reaching `handle` is a
    /// bring-up bug and panics there.
    pub(crate) fn new(plic: &'static Plic, context: PlicContext) -> Self {
        Self {
            plic,
            context,
            routes: ExternalInterruptRoutes::new(),
        }
    }

    pub(crate) fn attach_network(&mut self, interrupt: NetworkInterrupt) {
        self.enable_source(interrupt.source);
        self.routes.add_network(interrupt.source, interrupt.device);
    }

    pub(crate) fn attach_host_fs(&mut self, interrupt: crate::host_fs::HostFsInterrupt) {
        self.enable_source(interrupt.source);
        self.routes
            .set_host_fs(interrupt.source, interrupt.transport);
    }

    pub(crate) fn attach_vsock(&mut self, interrupt: crate::vsock::VsockInterrupt) {
        self.enable_source(interrupt.source);
        self.routes.set_vsock(interrupt.source, interrupt.device);
    }

    pub(crate) fn attach_entropy(&mut self, interrupt: crate::entropy::EntropyInterrupt) {
        self.enable_source(interrupt.source);
        self.routes.set_entropy(interrupt.source, interrupt.device);
    }

    pub(crate) fn attach_balloon(&mut self, interrupt: crate::balloon::BalloonInterrupt) {
        self.enable_source(interrupt.source);
        self.routes.set_balloon(interrupt.source, interrupt.handler);
    }

    pub(crate) fn attach_block(&mut self, interrupt: crate::block::BlockInterrupt) {
        self.enable_source(interrupt.source);
        self.routes.add_block(interrupt.source, interrupt.device);
    }

    fn enable_source(&self, source: InterruptSourceId) {
        self.plic.set_priority(source, 1);
        self.plic.enable(source, self.context);
        self.plic.set_threshold(self.context, 0);
    }

    pub(crate) fn handle(&self) {
        while let Some(source) = self.plic.claim(self.context) {
            let source = InterruptSourceId(source);
            assert!(
                self.routes.route(source),
                "unexpected external interrupt source={} on PLIC context={}",
                source.0.get(),
                self.context.0
            );
            self.plic.complete(self.context, source);
        }
    }
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

impl plic::HartContext for PlicContext {
    fn index(self) -> usize {
        self.0
    }
}

impl plic::InterruptSource for InterruptSourceId {
    fn id(self) -> NonZeroU32 {
        self.0
    }
}

fn discover_network_device(
    cpu: &RiscvCpu,
    fdt: &Fdt<'_>,
) -> Option<(VirtioNetworkDevice, InterruptSourceId)> {
    let candidate = helios_virtio::mmio_candidates(fdt).find(|candidate| {
        crate::matches_virtio_mmio_device(candidate.base, helios_virtio::DeviceType::Network)
    })?;
    let base = candidate.base;
    let header = core::ptr::NonNull::new(base as *mut u8)
        .unwrap_or_else(|| panic!("virtio MMIO base {base:#x} was unexpectedly null"));
    let irq_source = candidate
        .interrupt
        .and_then(|interrupt| NonZeroU32::new(interrupt.number))
        .map(InterruptSourceId)
        .unwrap_or_else(|| panic!("virtio-net node at {base:#x} has no valid interrupt source"));
    let device =
        unsafe { helios_virtio::net_from_mmio(header, candidate.size) }.unwrap_or_else(|error| {
            panic!("failed to initialize virtio-net device at {base:#x}: {error}")
        });
    Some((
        VirtioNetworkDevice {
            inner: Arc::new(device),
            cpu: cpu.clone(),
        },
        irq_source,
    ))
}

pub(crate) fn discover_plic_context(
    fdt: &Fdt<'_>,
    hart_id: u16,
) -> Option<(&'static Plic, PlicContext)> {
    let node = fdt.find_compatible(&["sifive,plic-1.0.0", "riscv,plic0"])?;
    let base = node.reg()?.next()?.starting_address as usize;
    let plic = unsafe { Plic::from_addr(base) };
    let context = parse_plic_context(fdt, node, hart_id)?;
    Some((plic, context))
}

fn parse_plic_context(fdt: &Fdt<'_>, node: FdtNode<'_, '_>, hart_id: u16) -> Option<PlicContext> {
    let interrupts_extended = node.property("interrupts-extended")?;
    let mut cells = interrupts_extended.value.chunks_exact(4);
    let mut index = 0usize;

    while let Some(phandle) = cells.next().map(read_be_u32) {
        let controller = fdt.find_phandle(phandle).unwrap_or_else(|| {
            panic!("PLIC references unknown interrupt controller phandle {phandle}")
        });
        let interrupt_cells = controller.interrupt_cells().unwrap_or_else(|| {
            panic!("interrupt controller phandle {phandle} is missing #interrupt-cells")
        });
        let spec =
            read_be_u32(cells.next().unwrap_or_else(|| {
                panic!("PLIC interrupts-extended truncated for phandle {phandle}")
            }));
        for _ in 1..interrupt_cells {
            let _ = cells.next().unwrap_or_else(|| {
                panic!("PLIC interrupts-extended truncated extra cells for phandle {phandle}")
            });
        }
        if controller_hart_id(fdt, phandle) == Some(hart_id)
            && spec == SUPERVISOR_EXTERNAL_INTERRUPT
        {
            return Some(PlicContext(index));
        }
        index = index.saturating_add(1);
    }

    None
}

fn controller_hart_id(fdt: &Fdt<'_>, phandle: u32) -> Option<u16> {
    let cpus = fdt.find_node("/cpus")?;
    let address_cells = cpus.cell_sizes().address_cells;

    for cpu in cpus.children() {
        if cpu
            .property("device_type")
            .is_none_or(|property| property.value != b"cpu\0")
        {
            continue;
        }

        let hart = parse_cpu_hart_id(cpu, address_cells)?;
        for child in cpu.children() {
            if child.property("interrupt-controller").is_none() {
                continue;
            }
            if node_phandle(child) == Some(phandle) {
                return Some(hart);
            }
        }
    }
    None
}

fn node_phandle(node: FdtNode<'_, '_>) -> Option<u32> {
    node.property("phandle")
        .or_else(|| node.property("linux,phandle"))
        .map(|property| read_be_u32(property.value))
}

fn parse_cpu_hart_id(node: FdtNode<'_, '_>, address_cells: usize) -> Option<u16> {
    let reg = node.property("reg")?;
    let hart = match address_cells {
        1 => read_be_u32(reg.value) as usize,
        2 => {
            let high = read_be_u32(&reg.value[..4]) as u64;
            let low = read_be_u32(&reg.value[4..8]) as u64;
            ((high << 32) | low) as usize
        }
        cells => panic!("unsupported CPU address-cells value {cells}"),
    };
    Some(hart as u16)
}

fn read_be_u32(bytes: &[u8]) -> u32 {
    let bytes: [u8; 4] = bytes
        .get(..4)
        .unwrap_or_else(|| panic!("expected a 4-byte device-tree cell"))
        .try_into()
        .unwrap_or_else(|_| panic!("expected a 4-byte device-tree cell"));
    u32::from_be_bytes(bytes)
}
