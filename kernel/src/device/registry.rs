//! Every device the machine is willing to grant away, and who has it.
//!
//! Discovery publishes the whole set once, during bring-up: a backend
//! walks its bus or its firmware description, builds a [`DeviceGrant`]
//! per device it is not driving itself, and hands them here. Nothing is
//! added afterwards, so the set every processor reads is immutable and
//! needs no lock.
//!
//! A device goes to exactly one owner. The claim is a compare-exchange
//! on the device's own flag, so two supervisors racing for the same
//! device produce one lease and one [`GrantError::AlreadyClaimed`]
//! rather than two drivers writing the same registers.
//!
//! # Concurrency contract
//!
//! [`DeviceGrantRegistry::publish`] runs once on the bootstrap processor
//! before secondary processors start. Every other method is lock-free
//! and safe from any processor; [`DeviceGrantRegistry::forward`] is
//! additionally safe from interrupt context.

use core::sync::atomic::{AtomicBool, Ordering};

use arrayvec::ArrayVec;
use spin::Once;
use triomphe::Arc;

use super::grant::{DeviceGrant, GrantError, GrantInterrupt};
use super::interrupt::InterruptRelay;
use super::lease::{DeviceWindow, GrantLease, PublishedDevice};
use super::platform::device_hooks_installed;
use crate::io::ExternalInterruptHandler;

/// Devices one machine may grant away.
///
/// The bound is deliberate: grant bookkeeping is kernel memory, and a
/// machine that wants to hand a hundred devices to user-mode drivers is
/// a machine whose discovery needs a policy rather than a bigger array.
pub const MAX_GRANTS: usize = 8;

/// The set of devices discovery published, and their owners.
pub struct DeviceGrantRegistry {
    devices: Once<ArrayVec<Arc<PublishedDevice>, MAX_GRANTS>>,
}

impl DeviceGrantRegistry {
    pub const fn new() -> Self {
        Self {
            devices: Once::new(),
        }
    }

    /// Publish everything discovery found. Called once, during
    /// bring-up.
    ///
    /// The platform surface has to be installed first: a device whose
    /// registers cannot be mapped and whose interrupts cannot be masked
    /// is not a device anyone can be handed, and failing here names the
    /// bring-up order rather than leaving the failure for the first
    /// driver.
    pub fn publish<Grants>(&self, grants: Grants) -> Result<(), GrantError>
    where
        Grants: IntoIterator<Item = DeviceGrant>,
    {
        if !device_hooks_installed() {
            return Err(GrantError::PlatformUnavailable);
        }
        let mut published: ArrayVec<Arc<PublishedDevice>, MAX_GRANTS> = ArrayVec::new();
        for grant in grants {
            if published
                .iter()
                .any(|device| device.grant.name() == grant.name())
            {
                return Err(GrantError::DuplicateName);
            }
            let relay = InterruptRelay::new(grant.interrupts());
            let device = Arc::new(PublishedDevice {
                grant,
                relay,
                claimed: AtomicBool::new(false),
            });
            published
                .try_push(device)
                .map_err(|_| GrantError::RegistryFull)?;
        }
        let count = published.len();
        let installed = self.devices.call_once(|| published);
        assert!(
            installed.len() == count,
            "device discovery published its grants more than once"
        );
        for device in installed {
            tracing::info!(
                target: "helios_kernel::device",
                device = %device.grant.name(),
                regions = device.grant.regions().len(),
                interrupts = device.grant.interrupts().len(),
                dma_budget_bytes = device.grant.dma().byte_budget,
                confined = device.grant.confinement().is_some(),
                "device grant published"
            );
        }
        Ok(())
    }

    /// Every device discovery published, whether or not it has an owner.
    ///
    /// This is what the inspector lists: a granted device is an ordinary
    /// part of the machine's inventory, not a hidden one.
    pub fn devices(&self) -> &[Arc<PublishedDevice>] {
        self.devices.get().map_or(&[], |devices| devices.as_slice())
    }

    /// The device `name` names, whether or not it has an owner.
    pub fn device(&self, name: &str) -> Option<&Arc<PublishedDevice>> {
        self.devices()
            .iter()
            .find(|device| device.grant.name().as_str() == name)
    }

    /// Take exclusive ownership of `name`, mapping its regions into the
    /// linear memory `window` describes.
    ///
    /// The second caller for a device that already has an owner gets
    /// [`GrantError::AlreadyClaimed`]; there is no queue, because a
    /// driver waiting for hardware another driver holds is a
    /// provisioning mistake rather than a transient shortage.
    pub fn claim(&self, name: &str, window: DeviceWindow) -> Result<GrantLease, GrantError> {
        let device = self.device(name).ok_or(GrantError::NotFound)?;
        device
            .claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| GrantError::AlreadyClaimed)?;
        tracing::info!(
            target: "helios_kernel::device",
            device = name,
            window_offset = window.offset(),
            window_bytes = window.bytes(),
            "device grant claimed"
        );
        Ok(GrantLease::new(device.clone(), window))
    }

    /// Hand `source` to whichever published device raises it.
    ///
    /// Called from interrupt context by a backend whose controller
    /// delivers granted sources through the same path as its own
    /// devices'. Returns false when no published device owns the source,
    /// so the backend fails fast with controller context in the message.
    #[must_use]
    pub fn forward(&self, source: GrantInterrupt) -> bool {
        self.devices()
            .iter()
            .any(|device| device.relay.forward(source))
    }
}

impl Default for DeviceGrantRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// The route a backend installs for one of a granted device's
/// interrupts.
///
/// Installed at boot against the published device, so it stays valid
/// across every owner the device has: an owner's death masks the source
/// and leaves the route in place for the owner that replaces it.
pub struct DeviceInterruptRoute {
    device: Arc<PublishedDevice>,
    source: GrantInterrupt,
}

impl DeviceInterruptRoute {
    pub fn new(device: Arc<PublishedDevice>, source: GrantInterrupt) -> Self {
        assert!(
            device.relay().index_of(source).is_some(),
            "an interrupt route was installed for a source the device does not raise"
        );
        Self { device, source }
    }
}

impl ExternalInterruptHandler for DeviceInterruptRoute {
    fn handle_interrupt(&self) {
        assert!(
            self.device.relay().forward(self.source),
            "a device interrupt route delivered a source its device does not raise"
        );
    }
}
