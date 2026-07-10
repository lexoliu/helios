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

/// A device that consumes external interrupts for one source.
pub trait ExternalInterruptHandler {
    fn handle_interrupt(&self);
}

/// Maps claimed interrupt sources to the network and host-fs handlers a
/// backend registered at boot.
pub struct ExternalInterruptRoutes<Source, Network, HostFs> {
    network: Option<(Source, Network)>,
    host_fs: Option<(Source, HostFs)>,
}

impl<Source, Network, HostFs> ExternalInterruptRoutes<Source, Network, HostFs>
where
    Source: PartialEq + Copy,
    Network: ExternalInterruptHandler,
    HostFs: ExternalInterruptHandler,
{
    pub const fn new() -> Self {
        Self {
            network: None,
            host_fs: None,
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

    /// Dispatches a claimed source to its handler. Returns false when no
    /// route matches so the backend can fail fast with controller
    /// context in the message.
    #[must_use]
    pub fn route(&self, source: Source) -> bool {
        if let Some((registered, handler)) = &self.network {
            if *registered == source {
                handler.handle_interrupt();
                return true;
            }
        }
        if let Some((registered, handler)) = &self.host_fs {
            if *registered == source {
                handler.handle_interrupt();
                return true;
            }
        }
        false
    }
}

impl<Source, Network, HostFs> Default for ExternalInterruptRoutes<Source, Network, HostFs>
where
    Source: PartialEq + Copy,
    Network: ExternalInterruptHandler,
    HostFs: ExternalInterruptHandler,
{
    fn default() -> Self {
        Self::new()
    }
}
