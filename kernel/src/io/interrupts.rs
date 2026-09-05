//! External interrupt routing shared by interrupt-driven backends.
//!
//! Backends own the interrupt controller (PLIC, GIC, MSI-X): they claim
//! a source on the interrupt path, ask the routes to dispatch it, and
//! complete it afterwards. The routes only map sources to the device
//! handlers registered at boot.
//!
//! Concurrency contract: routes are installed during single-processor
//! bring-up and are read-only on the interrupt path afterwards.
//! Handlers run in interrupt context and must be non-blocking.

use helios_hal::cpu::{Cpu, ProcessorId};

use crate::device::DeviceInterruptRoute;

/// Block devices one platform may route interrupts for.
///
/// A machine routinely carries more than one: the disk the kernel was
/// booted from and the scratch disk it owns are separate devices on the
/// same bus, and the kernel has to be able to reach both to tell them
/// apart.
pub const MAX_BLOCK_DEVICES: usize = 4;

/// Interrupt messages one network device may install routes for.
///
/// A device with per-queue vectors takes one per queue pair plus one for
/// configuration changes, which is what bounds this: the number of queue
/// vectors the x86 backend hands out, and the configuration message.
pub const MAX_NETWORK_INTERRUPTS: usize = 9;

/// Interrupt sources one platform may route to user-mode drivers.
///
/// Granted devices are routed through the same table as the kernel's
/// own, because the controller does not distinguish them: what a grant
/// changes is what the handler does, not where the source arrives. The
/// bound is the number of grants a machine publishes times the sources
/// one device raises, capped where the routing table stops being a
/// linear scan worth doing in interrupt context.
pub const MAX_DEVICE_INTERRUPTS: usize = 16;

/// A device that consumes external interrupts for one source.
pub trait ExternalInterruptHandler {
    fn handle_interrupt(&self);
}

/// A route slot a backend can never fill.
///
/// A backend whose device has no interrupt source to route — a PCI
/// function without an MSI-X capability on a backend that routes
/// MSI-X only — names this as the slot's handler type. The slot is
/// then provably empty: nothing can be registered in it and dispatch
/// never reaches it, without a handler whose body cannot run.
impl ExternalInterruptHandler for core::convert::Infallible {
    fn handle_interrupt(&self) {
        match *self {}
    }
}

/// Maps claimed interrupt sources to the device handlers a backend
/// registered at boot.
pub struct ExternalInterruptRoutes<Source, Network, HostFs, Entropy, Balloon, Vsock, Block> {
    network: [Option<(Source, Network)>; MAX_NETWORK_INTERRUPTS],
    host_fs: Option<(Source, HostFs)>,
    entropy: Option<(Source, Entropy)>,
    balloon: Option<(Source, Balloon)>,
    vsock: Option<(Source, Vsock)>,
    block: [Option<(Source, Block)>; MAX_BLOCK_DEVICES],
    /// Sources a user-mode driver owns. Concrete rather than generic:
    /// what a granted source reaches is the kernel's own relay, which
    /// only holds the source off and wakes the owner, so no backend has
    /// a handler type of its own to name here.
    device: [Option<(Source, DeviceInterruptRoute)>; MAX_DEVICE_INTERRUPTS],
}

impl<Source, Network, HostFs, Entropy, Balloon, Vsock, Block>
    ExternalInterruptRoutes<Source, Network, HostFs, Entropy, Balloon, Vsock, Block>
where
    Source: PartialEq + Copy,
    Network: ExternalInterruptHandler,
    HostFs: ExternalInterruptHandler,
    Entropy: ExternalInterruptHandler,
    Balloon: ExternalInterruptHandler,
    Vsock: ExternalInterruptHandler,
    Block: ExternalInterruptHandler,
{
    pub const fn new() -> Self {
        Self {
            network: [const { None }; MAX_NETWORK_INTERRUPTS],
            host_fs: None,
            entropy: None,
            balloon: None,
            vsock: None,
            block: [const { None }; MAX_BLOCK_DEVICES],
            device: [const { None }; MAX_DEVICE_INTERRUPTS],
        }
    }

    /// Registers one of the network device's interrupt messages.
    ///
    /// A device with per-queue vectors installs several: each queue
    /// pair's own message, delivered to that pair's processor, plus the
    /// configuration-change message. A device with a single interrupt
    /// line installs exactly one.
    pub fn add_network(&mut self, source: Source, handler: Network) {
        let slot = self
            .network
            .iter_mut()
            .find(|slot| slot.is_none())
            .unwrap_or_else(|| {
                panic!("more than {MAX_NETWORK_INTERRUPTS} network interrupt routes were installed")
            });
        *slot = Some((source, handler));
    }

    pub fn set_host_fs(&mut self, source: Source, handler: HostFs) {
        assert!(
            self.host_fs.is_none(),
            "host-fs interrupt route was installed more than once"
        );
        self.host_fs = Some((source, handler));
    }

    pub fn set_entropy(&mut self, source: Source, handler: Entropy) {
        assert!(
            self.entropy.is_none(),
            "entropy interrupt route was installed more than once"
        );
        self.entropy = Some((source, handler));
    }

    pub fn set_balloon(&mut self, source: Source, handler: Balloon) {
        assert!(
            self.balloon.is_none(),
            "memory balloon interrupt route was installed more than once"
        );
        self.balloon = Some((source, handler));
    }

    pub fn set_vsock(&mut self, source: Source, handler: Vsock) {
        assert!(
            self.vsock.is_none(),
            "vsock interrupt route was installed more than once"
        );
        self.vsock = Some((source, handler));
    }

    /// Registers one more block device.
    ///
    /// Unlike the single-device slots this one takes several handlers:
    /// the platform decides which of the disks it exposes the kernel
    /// ends up owning, and that decision needs every candidate to be
    /// reachable first.
    pub fn add_block(&mut self, source: Source, handler: Block) {
        let slot = self
            .block
            .iter_mut()
            .find(|slot| slot.is_none())
            .unwrap_or_else(|| {
                panic!("more than {MAX_BLOCK_DEVICES} block interrupt routes were installed")
            });
        *slot = Some((source, handler));
    }

    /// Registers one interrupt of a device granted to a user-mode
    /// driver.
    ///
    /// The route is installed at boot against the published device, not
    /// against whoever currently owns it, so it survives an owner's
    /// death and its replacement's start-up without the controller
    /// being touched.
    pub fn add_device(&mut self, source: Source, route: DeviceInterruptRoute) {
        let slot = self
            .device
            .iter_mut()
            .find(|slot| slot.is_none())
            .unwrap_or_else(|| {
                panic!("more than {MAX_DEVICE_INTERRUPTS} granted-device interrupt routes were installed")
            });
        *slot = Some((source, route));
    }

    /// Dispatches a claimed source to its handler. Returns false when no
    /// route matches so the backend can fail fast with controller
    /// context in the message.
    #[must_use]
    pub fn route(&self, source: Source) -> bool {
        if self.network.iter().any(|slot| dispatch(slot, source)) {
            return true;
        }
        if dispatch(&self.host_fs, source)
            || dispatch(&self.entropy, source)
            || dispatch(&self.balloon, source)
            || dispatch(&self.vsock, source)
        {
            return true;
        }
        if self.block.iter().any(|slot| dispatch(slot, source)) {
            return true;
        }
        self.device.iter().any(|slot| dispatch(slot, source))
    }
}

/// Pulls the owner of every queue that made progress out of its idle
/// park, skipping the processor already running the handler.
///
/// A device queue is drained by the processor whose shard owns it, so
/// its completions are only useful to that processor. A transport with
/// one interrupt line delivers them somewhere else, and this is the
/// hand-off; a transport with per-queue vectors delivers them to the
/// right processor already and never calls this.
///
/// # SMP contract
///
/// Called from interrupt context on any processor. It takes no locks
/// and allocates nothing; `Cpu::wake_processor` is the only thing it
/// does, once per queue, and never for the processor it runs on.
pub fn wake_queue_owners<CpuImpl: Cpu>(cpu: &CpuImpl, queues: impl Iterator<Item = usize>) {
    let current = cpu.current_processor();
    let processors = cpu.processor_count();
    for queue in queues {
        // A queue beyond the processor count belongs to no shard: the
        // service sizes its shard set to the processors it has.
        if queue >= processors {
            continue;
        }
        let owner = ProcessorId::new(
            u16::try_from(queue)
                .unwrap_or_else(|_| panic!("queue {queue} exceeds the processor id range")),
        );
        if owner != current {
            cpu.wake_processor(owner);
        }
    }
}

fn dispatch<Source, Handler>(slot: &Option<(Source, Handler)>, source: Source) -> bool
where
    Source: PartialEq + Copy,
    Handler: ExternalInterruptHandler,
{
    match slot {
        Some((registered, handler)) if *registered == source => {
            handler.handle_interrupt();
            true
        }
        Some(_) | None => false,
    }
}

impl<Source, Network, HostFs, Entropy, Balloon, Vsock, Block> Default
    for ExternalInterruptRoutes<Source, Network, HostFs, Entropy, Balloon, Vsock, Block>
where
    Source: PartialEq + Copy,
    Network: ExternalInterruptHandler,
    HostFs: ExternalInterruptHandler,
    Entropy: ExternalInterruptHandler,
    Balloon: ExternalInterruptHandler,
    Vsock: ExternalInterruptHandler,
    Block: ExternalInterruptHandler,
{
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use helios_hal::cpu::ProcessorId;

    use crate::test_support::RecordingSmpCpu;

    /// A single-line transport notices every queue's completions on one
    /// processor, so the hand-off is an IPI to each owner — and never to
    /// the processor already running the handler, which is about to
    /// drain its own queue anyway.
    #[test]
    fn only_foreign_queue_owners_are_woken() {
        let cpu = RecordingSmpCpu::new(1, 4);

        super::wake_queue_owners(&cpu, [0usize, 1, 3].into_iter());

        assert_eq!(
            cpu.woken(),
            alloc::vec![ProcessorId::new(0), ProcessorId::new(3)],
            "queue 1 belongs to this processor and costs no IPI"
        );
    }

    /// A device may expose more queues than the machine has processors;
    /// the shard set is sized to the processors, so a queue past that
    /// belongs to nobody.
    #[test]
    fn a_queue_without_a_processor_is_not_woken() {
        let cpu = RecordingSmpCpu::new(0, 2);

        super::wake_queue_owners(&cpu, [1usize, 2, 7].into_iter());

        assert_eq!(cpu.woken(), alloc::vec![ProcessorId::new(1)]);
    }

    #[test]
    fn nothing_to_wake_costs_no_ipi() {
        let cpu = RecordingSmpCpu::new(0, 4);

        super::wake_queue_owners(&cpu, core::iter::empty());

        assert!(cpu.woken().is_empty());
    }
}
