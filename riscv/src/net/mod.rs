extern crate alloc;

use alloc::sync::Arc;
use core::num::NonZeroU32;

use fdt::Fdt;
use fdt::node::FdtNode;
use helios_hal::cpu::Cpu;
use helios_hal::io::IoError;
use helios_hal::watchdog::Watchdog;
use helios_kernel::{
    DEFAULT_POLL_BUDGET, EventDeliveryCapabilities, ExternalInterruptHandler,
    ExternalInterruptRoutes, InterfaceCapabilities, Kernel, NetworkDevice, NetworkService,
    PacketBuffer,
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
}

pub(crate) struct ExternalInterrupts {
    plic: &'static Plic,
    context: PlicContext,
    routes: ExternalInterruptRoutes<
        InterruptSourceId,
        VirtioNetworkDevice,
        crate::host_fs::HostFsTransportService,
    >,
}

impl ExternalInterruptHandler for VirtioNetworkDevice {
    fn handle_interrupt(&self) {
        self.inner.handle_interrupt();
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PlicContext(pub(crate) usize);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct InterruptSourceId(pub(crate) NonZeroU32);

struct NetworkProbe {
    device: VirtioNetworkDevice,
    irq_source: InterruptSourceId,
    plic: &'static Plic,
    context: PlicContext,
}

pub(crate) fn install_network_service<WatchdogImpl>(
    cpu: &RiscvCpu,
    kernel: &Kernel<RiscvCpu, WatchdogImpl>,
    fdt: &Fdt<'_>,
    debug_state: &RuntimeState,
) -> Option<ExternalInterrupts>
where
    WatchdogImpl: Watchdog + Clone,
{
    let probe = NetworkProbe::discover(fdt, cpu.bootstrap_processor().id())?;
    let service = NetworkService::new(
        cpu.clone(),
        debug_state.clone(),
        kernel.timer(),
        probe.device.clone(),
    );
    debug_state.install_network_service(helios_kernel::ComponentHostNetworkService::from_service(
        service,
    ));
    probe.enable();
    tracing::info!(
        "virtio network online irq={} context={}",
        probe.irq_source.0.get(),
        probe.context.0
    );
    let mut routes = ExternalInterruptRoutes::new();
    routes.set_network(probe.irq_source, probe.device);
    Some(ExternalInterrupts {
        plic: probe.plic,
        context: probe.context,
        routes,
    })
}

impl ExternalInterrupts {
    pub(crate) fn host_fs_only(
        plic: &'static Plic,
        context: PlicContext,
        interrupt: crate::host_fs::HostFsInterrupt,
    ) -> Self {
        let mut routes = ExternalInterruptRoutes::new();
        routes.set_host_fs(interrupt.source, interrupt.transport);
        Self {
            plic,
            context,
            routes,
        }
    }

    pub(crate) fn attach_host_fs(&mut self, interrupt: crate::host_fs::HostFsInterrupt) {
        self.routes
            .set_host_fs(interrupt.source, interrupt.transport);
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

impl NetworkProbe {
    fn discover(fdt: &Fdt<'_>, bootstrap_hart: u16) -> Option<Self> {
        let (device, irq_source) = discover_network_device(fdt)?;
        let (plic, context) = discover_plic_context(fdt, bootstrap_hart)?;
        Some(Self {
            device,
            irq_source,
            plic,
            context,
        })
    }

    fn enable(&self) {
        self.plic.set_priority(self.irq_source, 1);
        self.plic.enable(self.irq_source, self.context);
        self.plic.set_threshold(self.context, 0);
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
        InterfaceCapabilities {
            max_frame_len: self.max_frame_len(),
            checksum: helios_kernel::ChecksumOffload {
                tx_tcp: self.inner.tx_checksum_negotiated(),
                tx_udp: self.inner.tx_checksum_negotiated(),
                ..helios_kernel::ChecksumOffload::default()
            },
            events: EventDeliveryCapabilities {
                polling: true,
                interrupts: true,
                adaptive_moderation: false,
                rx_coalescing: false,
                tx_coalescing: false,
                rx_poll_budget: DEFAULT_POLL_BUDGET,
                tx_completion_budget: DEFAULT_POLL_BUDGET,
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
        frames: &'slots mut [Option<bytes::Bytes>],
    ) -> Result<Option<usize>, IoError>
    where
        'a: 'slots,
    {
        self.inner
            .try_receive_frames_immediate_on_pair(queue_idx, frames)
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

fn discover_network_device(fdt: &Fdt<'_>) -> Option<(VirtioNetworkDevice, InterruptSourceId)> {
    let candidate = helios_virtio::mmio_candidates(fdt).find(|candidate| {
        crate::matches_virtio_mmio_device(candidate.base, helios_virtio::DeviceType::Network)
    })?;
    let base = candidate.base;
    let header = core::ptr::NonNull::new(base as *mut u8)
        .unwrap_or_else(|| panic!("virtio MMIO base {base:#x} was unexpectedly null"));
    let irq_source = candidate
        .irq
        .map(InterruptSourceId)
        .unwrap_or_else(|| panic!("virtio-net node at {base:#x} has no valid interrupt source"));
    let device =
        unsafe { helios_virtio::net_from_mmio(header, candidate.size) }.unwrap_or_else(|error| {
            panic!("failed to initialize virtio-net device at {base:#x}: {error}")
        });
    Some((
        VirtioNetworkDevice {
            inner: Arc::new(device),
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
