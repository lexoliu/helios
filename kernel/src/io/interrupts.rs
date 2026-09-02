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

/// Block devices one platform may route interrupts for.
///
/// A machine routinely carries more than one: the disk the kernel was
/// booted from and the scratch disk it owns are separate devices on the
/// same bus, and the kernel has to be able to reach both to tell them
/// apart.
pub const MAX_BLOCK_DEVICES: usize = 4;

/// A device that consumes external interrupts for one source.
pub trait ExternalInterruptHandler {
    fn handle_interrupt(&self);
}

/// Maps claimed interrupt sources to the device handlers a backend
/// registered at boot.
pub struct ExternalInterruptRoutes<Source, Network, HostFs, Entropy, Balloon, Block> {
    network: Option<(Source, Network)>,
    host_fs: Option<(Source, HostFs)>,
    entropy: Option<(Source, Entropy)>,
    balloon: Option<(Source, Balloon)>,
    block: [Option<(Source, Block)>; MAX_BLOCK_DEVICES],
}

impl<Source, Network, HostFs, Entropy, Balloon, Block>
    ExternalInterruptRoutes<Source, Network, HostFs, Entropy, Balloon, Block>
where
    Source: PartialEq + Copy,
    Network: ExternalInterruptHandler,
    HostFs: ExternalInterruptHandler,
    Entropy: ExternalInterruptHandler,
    Balloon: ExternalInterruptHandler,
    Block: ExternalInterruptHandler,
{
    pub const fn new() -> Self {
        Self {
            network: None,
            host_fs: None,
            entropy: None,
            balloon: None,
            block: [const { None }; MAX_BLOCK_DEVICES],
        }
    }

    pub fn set_network(&mut self, source: Source, handler: Network) {
        assert!(
            self.network.is_none(),
            "network interrupt route was installed more than once"
        );
        self.network = Some((source, handler));
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

    /// Dispatches a claimed source to its handler. Returns false when no
    /// route matches so the backend can fail fast with controller
    /// context in the message.
    #[must_use]
    pub fn route(&self, source: Source) -> bool {
        if dispatch(&self.network, source)
            || dispatch(&self.host_fs, source)
            || dispatch(&self.entropy, source)
            || dispatch(&self.balloon, source)
        {
            return true;
        }
        self.block.iter().any(|slot| dispatch(slot, source))
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

impl<Source, Network, HostFs, Entropy, Balloon, Block> Default
    for ExternalInterruptRoutes<Source, Network, HostFs, Entropy, Balloon, Block>
where
    Source: PartialEq + Copy,
    Network: ExternalInterruptHandler,
    HostFs: ExternalInterruptHandler,
    Entropy: ExternalInterruptHandler,
    Balloon: ExternalInterruptHandler,
    Block: ExternalInterruptHandler,
{
    fn default() -> Self {
        Self::new()
    }
}
