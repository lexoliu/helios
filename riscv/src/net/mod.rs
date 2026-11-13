extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::num::NonZeroU32;

use fdt::Fdt;
use helios_hal::cpu::{Cpu, ProcessorId};
use helios_hal::io::IoError;
use helios_hal::watchdog::Watchdog;
use helios_kernel::{
    DebugSerialInterrupt, DeviceInterruptRoute, ExternalInterruptHandler, ExternalInterruptRoutes,
    GrantInterrupt, InterfaceCapabilities, InterfaceEventMark, Kernel, LinkState, NetworkDevice,
};
use helios_virtio::{InterruptController, MmioInterrupt};
use plic::Plic;
use spin::{Mutex, Once};

use crate::RiscvCpu;
use crate::debug_state::RuntimeState;

type RiscvVirtioNetTransport = helios_virtio::VirtioMmioTransport<helios_virtio::MmioBus>;
type RiscvVirtioNetDevice = helios_virtio::VirtioNetDevice<RiscvVirtioNetTransport>;

#[derive(Clone)]
pub(crate) struct VirtioNetworkDevice {
    inner: Arc<RiscvVirtioNetDevice>,
    /// Held so the interrupt handler can steer by IPI.
    ///
    /// The transport has one interrupt line, and the hart that takes
    /// its PLIC's interrupts notices every queue pair's completions
    /// whichever hart owns the pair. Waking the owners is the only
    /// steering a single-source transport can do.
    cpu: RiscvCpu,
}

/// The debug UART's route handler, keyed by the port's access type:
/// it masks the port's receive interrupt through `DebugTransport` and
/// raises the debug console's receive signal.
type DebugSerialHandler = DebugSerialInterrupt<crate::DebugTransport>;

type DeviceRoutes = ExternalInterruptRoutes<
    InterruptSourceId,
    VirtioNetworkDevice,
    crate::host_fs::HostFsTransportService,
    crate::entropy::VirtioEntropyDevice,
    crate::balloon::VirtioBalloonInterrupt,
    crate::vsock::VirtioVsockDevice,
    crate::gpu::VirtioDisplayDevice,
    crate::input::VirtioInputDevice,
    crate::snd::VirtioSoundDevice,
    crate::block::VirtioBlockDevice,
    DebugSerialHandler,
>;

/// Bits of a granted device's [`GrantInterrupt`] that carry its PLIC
/// source; the bits above them carry the index of the source's
/// [`ExternalInterrupts`] domain in [`InterruptDomains`].
///
/// The PLIC numbers its sources 1 to 1023, ID 0 meaning "no interrupt"
/// (RISC-V PLIC Specification 1.0.0, "Interrupt Identifiers" and
/// "Memory Map"), so ten bits hold any of them. The kernel treats a grant's interrupt number as opaque
/// and hands it back unchanged to the `mask` and `unmask` hooks, which
/// is what lets the backend fold the controller in: a bare source
/// number is unique on one PLIC only. With one PLIC the encoding is the
/// source number itself.
const GRANT_SOURCE_BITS: u32 = 10;

/// The machine's interrupt domains, from the moment the bootstrap hart
/// has attached every device until the machine stops.
///
/// A static because the device path's masking hooks are plain function
/// pointers by design (`DeviceInterruptHooks`), and they act on the
/// domain a granted source belongs to. The harts reach it through
/// [`HartInterrupts`], handed to each one's runtime when it is built.
static INTERRUPT_DOMAINS: Once<InterruptDomains> = Once::new();

/// One PLIC's interrupt domain: the controller, the context of the hart
/// that takes its interrupts, and the routes of every device wired to
/// it.
///
/// # Concurrency contract
///
/// Built on the bootstrap hart while no other hart runs kernel code:
/// devices attach through `&mut self`, which sets their priorities and
/// records the sources to arm. Then moved into [`INTERRUPT_DOMAINS`] by
/// [`publish_interrupt_domains`], and never mutated again except through
/// the PLIC's own registers.
///
/// `context`'s enable words and threshold are first written by `hart`
/// itself, in [`Self::open`], once it runs kernel code: the SBI firmware
/// resets a hart's PLIC contexts when it starts the hart (OpenSBI's
/// `plic_warm_irqchip_init` disables every source on them and raises
/// their threshold to 7), so anything the bootstrap hart wrote there for
/// a hart it had not started yet would be lost.
///
/// Afterwards `hart` alone claims and completes on `context`, from its
/// supervisor-external trap, and so it alone runs the route handlers.
/// When `hart` is not the bootstrap hart that is a hart other than the
/// one that brought the devices up; every handler here is `Send + Sync`
/// (the static requires it) and wakes its device's owners by `Notify`
/// or IPI, which reach a task wherever it runs.
///
/// The enable words are written from any hart — a granted device's
/// owner masks and unmasks from wherever its task runs, and `mask` also
/// runs in `hart`'s interrupt context — always through
/// [`Self::set_source_enabled`].
pub(crate) struct ExternalInterrupts {
    plic: &'static Plic,
    controller: InterruptController,
    hart: ProcessorId,
    context: PlicContext,
    /// Serialises this PLIC's enable words.
    ///
    /// `plic::Plic::enable` and `disable` are a read-modify-write of the
    /// 32-bit word that carries a source's enable bit, and that word
    /// covers thirty-two sources. Two harts touching two different
    /// sources in the same word therefore race, and one of them loses
    /// its bit — a granted device that stops delivering, or worse, a
    /// kernel device that does. Each PLIC has its own words, so each
    /// domain has its own lock; only `context`'s words are ever written.
    ///
    /// The lock is taken with interrupts masked on the local hart
    /// because one of the writers is a granted device's `mask`, which
    /// runs in interrupt context: a task holding this lock and then
    /// interrupted into `mask` on its own hart would spin on itself.
    enable_words: Mutex<()>,
    /// Whether any device is routed here. A PLIC nothing is wired to
    /// keeps its sources disabled and its taking hart never visits it.
    routed: bool,
    /// The sources [`Self::open`] enables: every kernel device attached
    /// during bring-up. A granted device's source is not among them; its
    /// owner arms it.
    armed: Vec<InterruptSourceId>,
    routes: DeviceRoutes,
}

impl ExternalInterruptHandler for VirtioNetworkDevice {
    fn handle_interrupt(&self) {
        let progress = self.inner.handle_interrupt();
        helios_kernel::wake_queue_owners(
            &self.cpu,
            progress
                .iter()
                .map(|pair| self.inner.queue_pair_owner(pair)),
        );
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PlicContext(pub(crate) usize);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct InterruptSourceId(pub(crate) NonZeroU32);

/// One device's interrupt line: the PLIC its node's interrupt parent
/// names, and the source number on that PLIC.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlicInterrupt {
    pub(crate) controller: InterruptController,
    pub(crate) source: InterruptSourceId,
}

impl PlicInterrupt {
    /// The line a decoded `interrupts` specifier names. `None` for
    /// source 0, which the PLIC reserves for "no interrupt".
    pub(crate) fn from_specifier(interrupt: MmioInterrupt) -> Option<Self> {
        NonZeroU32::new(interrupt.number).map(|number| Self {
            controller: interrupt.controller,
            source: InterruptSourceId(number),
        })
    }

    /// The source number, for the bring-up log.
    pub(crate) fn number(self) -> u32 {
        self.source.0.get()
    }
}

/// The network device the bootstrap hart brought up, together with the
/// PLIC line the device tree routes it to.
pub(crate) struct NetworkInterrupt {
    line: PlicInterrupt,
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
    // The pair budget is the processor count: every activated pair is
    // drained by the one packet pump its owning hart runs, so a device
    // advertising more pairs than the machine has harts is clamped to
    // what has a pump rather than left to collect frames nobody drains.
    let pair_budget = helios_virtio::QueuePairBudget::new(cpu.processor_count());
    let Some((device, line)) = discover_network_device(cpu, fdt, pair_budget) else {
        tracing::warn!("virtio network device was not discovered on the platform bus");
        return None;
    };
    kernel.install_network_interface(debug_state, device.clone());
    tracing::info!(
        pair_budget = pair_budget.get(),
        queue_pairs = device.queue_pair_count(),
        "virtio network online irq={}",
        line.number()
    );
    Some(NetworkInterrupt { line, device })
}

impl ExternalInterrupts {
    /// Opens `hart`'s `context` on `plic`. Devices attach to it as they
    /// come up; a source with no route reaching `handle` is a bring-up
    /// bug and panics there.
    fn new(
        plic: &'static Plic,
        controller: InterruptController,
        hart: ProcessorId,
        context: PlicContext,
    ) -> Self {
        Self {
            plic,
            controller,
            hart,
            context,
            enable_words: Mutex::new(()),
            routed: false,
            armed: Vec::new(),
            routes: ExternalInterruptRoutes::new(),
        }
    }

    /// Give `source` a priority and record it for the taking hart to
    /// enable in [`Self::open`].
    fn enable_source(&mut self, source: InterruptSourceId) {
        self.plic.set_priority(source, 1);
        self.armed.push(source);
        self.routed = true;
    }

    /// Enable every armed source on the taking context and open its
    /// threshold. Runs on the taking hart, once, after the firmware has
    /// started it and before it sets `sie.SEIE`.
    fn open(&self) {
        for &source in &self.armed {
            self.set_source_enabled(source, true);
        }
        self.plic.set_threshold(self.context, 0);
    }

    /// Arm or hold off one source on the taking context, without
    /// disturbing the thirty-one that share its enable word.
    pub(crate) fn set_source_enabled(&self, source: InterruptSourceId, enabled: bool) {
        critical_section::with(|_| {
            let _words = self.enable_words.lock();
            if enabled {
                self.plic.enable(source, self.context);
            } else {
                self.plic.disable(source, self.context);
            }
        });
    }

    /// Claim, dispatch and complete every source pending on the taking
    /// context. Runs on the taking hart only, in its supervisor-external
    /// trap.
    fn handle(&self) {
        while let Some(source) = self.plic.claim(self.context) {
            let source = InterruptSourceId(source);
            assert!(
                self.routes.route(source),
                "unexpected external interrupt source={} on the PLIC at {:#x}, context={}",
                source.0.get(),
                self.controller.base(),
                self.context.0
            );
            self.plic.complete(self.context, source);
        }
    }
}

/// Every PLIC domain on the machine, one per controller a hart can take
/// interrupts from.
///
/// # Concurrency contract
///
/// The bootstrap hart discovers the domains and attaches every device
/// to the domain of its line's controller while it is the only hart
/// running kernel code, then publishes the whole table once, before it
/// starts any secondary hart. From then on the table is immutable:
/// each hart's trap reads the domains it takes with no lock, and the
/// grant hooks read the one a granted source belongs to.
///
/// A source that fires during bring-up stays pending in its PLIC: the
/// hart that takes it enables it only once it has installed its runtime,
/// which carries the published table, and then sets its own `sie.SEIE`.
pub(crate) struct InterruptDomains {
    /// Indexed by the domain number a granted source's
    /// [`GrantInterrupt`] carries, so never reordered.
    domains: Vec<ExternalInterrupts>,
}

impl InterruptDomains {
    /// One domain for every PLIC in which some hart has a
    /// supervisor-external context, taken by the hart
    /// [`helios_virtio::plic_descriptions`] chooses.
    ///
    /// A PLIC no hart has a supervisor context in has no domain, so a
    /// device wired to it fails at attach rather than never
    /// interrupting.
    pub(crate) fn discover(fdt: &Fdt<'_>, bootstrap_hart: ProcessorId) -> Self {
        let domains = helios_virtio::plic_descriptions(fdt, bootstrap_hart.id())
            .filter_map(|plic| {
                let taking = plic.taking?;
                // SAFETY: the device tree names this window as a PLIC's
                // registers, and the kernel's identity map covers the
                // platform's MMIO.
                let registers = unsafe { Plic::from_addr(plic.controller.base()) };
                Some(ExternalInterrupts::new(
                    registers,
                    plic.controller,
                    ProcessorId::new(taking.hart),
                    PlicContext(taking.context),
                ))
            })
            .collect();
        Self { domains }
    }

    /// Whether some hart takes `controller`'s interrupts.
    pub(crate) fn takes(&self, controller: InterruptController) -> bool {
        self.index_of(controller).is_some()
    }

    fn index_of(&self, controller: InterruptController) -> Option<usize> {
        self.domains
            .iter()
            .position(|domain| domain.controller == controller)
    }

    fn domain_mut(&mut self, line: PlicInterrupt, device: &str) -> &mut ExternalInterrupts {
        let index = self.index_of(line.controller).unwrap_or_else(|| {
            panic!(
                "{device} is wired to the interrupt controller at {:#x}, in which no hart has a \
                 PLIC supervisor-external context",
                line.controller.base()
            )
        });
        &mut self.domains[index]
    }

    /// Enable `line` on its domain's taking context and hand its route
    /// to `route`.
    fn attach(
        &mut self,
        line: PlicInterrupt,
        device: &str,
        route: impl FnOnce(&mut DeviceRoutes, InterruptSourceId),
    ) {
        let domain = self.domain_mut(line, device);
        domain.enable_source(line.source);
        route(&mut domain.routes, line.source);
    }

    pub(crate) fn attach_network(&mut self, interrupt: NetworkInterrupt) {
        self.attach(interrupt.line, "virtio-net", |routes, source| {
            routes.add_network(source, interrupt.device);
        });
    }

    pub(crate) fn attach_host_fs(&mut self, interrupt: crate::host_fs::HostFsInterrupt) {
        self.attach(interrupt.line, "virtio-9p", |routes, source| {
            routes.set_host_fs(source, interrupt.transport);
        });
    }

    pub(crate) fn attach_vsock(&mut self, interrupt: crate::vsock::VsockInterrupt) {
        self.attach(interrupt.line, "virtio-vsock", |routes, source| {
            routes.set_vsock(source, interrupt.device);
        });
    }

    pub(crate) fn attach_display(&mut self, interrupt: crate::gpu::DisplayInterrupt) {
        self.attach(interrupt.line, "virtio-gpu", |routes, source| {
            routes.set_display(source, interrupt.device);
        });
    }

    pub(crate) fn attach_input(&mut self, interrupt: crate::input::InputInterrupt) {
        self.attach(interrupt.line, "virtio-input", |routes, source| {
            routes.add_input(source, interrupt.device);
        });
    }

    pub(crate) fn attach_sound(&mut self, interrupt: crate::snd::SoundInterrupt) {
        self.attach(interrupt.line, "virtio-snd", |routes, source| {
            routes.set_sound(source, interrupt.device);
        });
    }

    pub(crate) fn attach_entropy(&mut self, interrupt: crate::entropy::EntropyInterrupt) {
        self.attach(interrupt.line, "virtio-rng", |routes, source| {
            routes.set_entropy(source, interrupt.device);
        });
    }

    pub(crate) fn attach_balloon(&mut self, interrupt: crate::balloon::BalloonInterrupt) {
        self.attach(interrupt.line, "virtio-balloon", |routes, source| {
            routes.set_balloon(source, interrupt.handler);
        });
    }

    pub(crate) fn attach_block(&mut self, interrupt: crate::block::BlockInterrupt) {
        self.attach(interrupt.line, "virtio-blk", |routes, source| {
            routes.add_block(source, interrupt.device);
        });
    }

    /// Route the debug UART's line; the handler masks the port's
    /// receive line and raises the debug console's receive signal in
    /// interrupt context.
    pub(crate) fn attach_debug_serial(&mut self, line: PlicInterrupt, handler: DebugSerialHandler) {
        self.attach(line, "the debug UART", |routes, source| {
            routes.set_debug_serial(source, handler);
        });
    }

    /// Route a line a user-mode driver owns.
    ///
    /// Unlike every other attach, the source is given a priority and
    /// left disabled: the grant's owner arms it with its own `unmask`,
    /// and until something owns the device an interrupt would have
    /// nowhere to go.
    pub(crate) fn attach_device(&mut self, line: PlicInterrupt, route: DeviceInterruptRoute) {
        let domain = self.domain_mut(line, "a granted device");
        domain.plic.set_priority(line.source, 1);
        domain.routed = true;
        domain.routes.add_device(line.source, route);
    }

    /// The number a grant for `line` carries: its domain's index above
    /// [`GRANT_SOURCE_BITS`], its source below. `None` when no hart
    /// takes `line`'s controller, so the device cannot interrupt.
    pub(crate) fn grant_interrupt(&self, line: PlicInterrupt) -> Option<GrantInterrupt> {
        let index = u32::try_from(self.index_of(line.controller)?)
            .unwrap_or_else(|_| panic!("the machine has more PLIC domains than a u32 indexes"));
        let source = line.number();
        assert!(
            source >> GRANT_SOURCE_BITS == 0,
            "PLIC source {source} is beyond the 1023 a PLIC numbers"
        );
        Some(GrantInterrupt::new((index << GRANT_SOURCE_BITS) | source))
    }

    /// The domain and source a [`Self::grant_interrupt`] number names.
    pub(crate) fn granted_source(&self, raw: u32) -> (&ExternalInterrupts, InterruptSourceId) {
        let domain = self
            .domains
            .get((raw >> GRANT_SOURCE_BITS) as usize)
            .unwrap_or_else(|| panic!("granted interrupt {raw:#x} names no PLIC domain"));
        let source = NonZeroU32::new(raw & ((1 << GRANT_SOURCE_BITS) - 1))
            .unwrap_or_else(|| panic!("granted interrupt {raw:#x} names PLIC source 0"));
        (domain, InterruptSourceId(source))
    }

    fn taken_by(&self, hart: ProcessorId) -> impl Iterator<Item = &ExternalInterrupts> + '_ {
        self.domains
            .iter()
            .filter(move |domain| domain.routed && domain.hart == hart)
    }
}

/// Publish the machine's domains. Called once, by the bootstrap hart,
/// after every device has attached and before any secondary is started.
pub(crate) fn publish_interrupt_domains(domains: InterruptDomains) {
    assert!(
        INTERRUPT_DOMAINS.get().is_none(),
        "the interrupt domains were published more than once"
    );
    INTERRUPT_DOMAINS.call_once(|| domains);
}

/// The published domains.
///
/// # Panics
///
/// Before [`publish_interrupt_domains`]: a hart building its runtime, or
/// a granted device masked, before the bootstrap hart finished bring-up
/// would otherwise miss every route.
pub(crate) fn interrupt_domains() -> &'static InterruptDomains {
    INTERRUPT_DOMAINS
        .get()
        .expect("the interrupt domains were reached before the bootstrap hart published them")
}

/// The domains one hart takes, as its supervisor-external trap reaches
/// them. `None` when the hart takes none, so an external interrupt
/// reaching it is a bring-up bug.
#[derive(Clone, Copy)]
pub(crate) struct HartInterrupts {
    domains: &'static InterruptDomains,
    hart: ProcessorId,
}

impl HartInterrupts {
    pub(crate) fn of(hart: ProcessorId) -> Option<Self> {
        let domains = interrupt_domains();
        domains
            .taken_by(hart)
            .next()
            .is_some()
            .then_some(Self { domains, hart })
    }

    /// Open every domain this hart takes: its kernel devices' sources
    /// enabled on this hart's contexts, the thresholds at zero. Called by
    /// this hart when it builds its runtime, before it sets `sie.SEIE`.
    pub(crate) fn open(&self) {
        for domain in self.domains.taken_by(self.hart) {
            domain.open();
        }
    }

    /// Claim and dispatch everything pending on every domain this hart
    /// takes. The PLIC raises `sip.SEIP` while any of them has a source
    /// pending on this hart's context, so each is drained before the
    /// trap returns.
    pub(crate) fn handle(&self) {
        for domain in self.domains.taken_by(self.hart) {
            domain.handle();
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

    fn set_active_queue_pairs(&self, pairs: usize) -> helios_hal::io::IoResult<()> {
        self.inner.set_active_queue_pairs(pairs)
    }

    /// The PLIC source is the device's one line, so there is no message
    /// to retarget: steering is the owner the handler wakes.
    fn steer_queue_pair(&self, pair: usize, processor: helios_hal::cpu::ProcessorId) {
        self.inner.steer_pair(pair, processor);
    }

    fn capabilities(&self) -> InterfaceCapabilities {
        self.inner.interface_capabilities()
    }

    fn link_state(&self) -> LinkState {
        self.inner.link_state()
    }

    fn try_receive_frames_immediate_on<Sink>(
        &self,
        queue_idx: usize,
        budget: usize,
        sink: Sink,
    ) -> Option<helios_virtio::RxDrain>
    where
        Sink: FnMut(&helios_virtio::RxFrame),
    {
        self.inner
            .try_receive_frames_immediate_on_pair(queue_idx, budget, sink)
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

    fn rx_pool_stalls(&self, queue_idx: usize) -> u64 {
        self.inner.rx_pool_stalls(queue_idx)
    }

    fn rx_pool_free(&self, queue_idx: usize) -> u32 {
        self.inner.rx_pool_free(queue_idx)
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
    pair_budget: helios_virtio::QueuePairBudget,
) -> Option<(VirtioNetworkDevice, PlicInterrupt)> {
    let candidate = helios_virtio::mmio_candidates(fdt).find(|candidate| {
        crate::matches_virtio_mmio_device(candidate.base, helios_virtio::DeviceType::Network)
    })?;
    let base = candidate.base;
    let header = core::ptr::NonNull::new(base as *mut u8)
        .unwrap_or_else(|| panic!("virtio MMIO base {base:#x} was unexpectedly null"));
    let line = candidate
        .interrupt
        .and_then(PlicInterrupt::from_specifier)
        .unwrap_or_else(|| panic!("virtio-net node at {base:#x} has no valid interrupt source"));
    let device = unsafe { helios_virtio::net_from_mmio(header, candidate.size, pair_budget) }
        .unwrap_or_else(|error| {
            panic!("failed to initialize virtio-net device at {base:#x}: {error}")
        });
    Some((
        VirtioNetworkDevice {
            inner: Arc::new(device),
            cpu: cpu.clone(),
        },
        line,
    ))
}
