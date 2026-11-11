//! One instance's side of the device path.
//!
//! A driver is an ordinary user-mode instance that happens to hold a
//! device. What its store carries for that is the lease and, beside it,
//! the instance's holds on the display, the desktop, the 3D engine and
//! a playback stream. Where any of them appears in the instance's
//! memory is not here: every span is one the instance chose, recorded
//! in the store's [`ViewTable`], which is the one place that knows what
//! backs the instance's memory.
//!
//! # Concurrency contract
//!
//! Store state is owned by the one task running the instance, so
//! nothing here is shared and nothing here locks. The relay the lease
//! points at, and the claim words and queues behind the other holds,
//! are the shared parts, and they carry their own synchronisation.

use crate::audio::{AudioOwnership, AudioService, AudioServiceError};
use crate::buffer::{ViewError, ViewTable};
use crate::display::{DisplayOwnership, DisplayService, DisplayServiceError};
use crate::gpu::{Gpu3dOwnership, Gpu3dService, Gpu3dServiceError};
use crate::surface::SurfaceOwnership;

use super::grant::{DeviceName, GrantError};
use super::lease::GrantLease;
use super::registry::DeviceGrantRegistry;

/// One instance's hold on the device path.
///
/// Five things can put hardware, or another instance's pixels, inside
/// an instance's memory: a device grant, a display claim, the windows
/// of the surface path, a claim on the 3D engine, and — through a
/// stream rather than a view — a playback claim. All five are released
/// by the store's own drop.
#[derive(Default)]
pub struct DeviceOwnership {
    /// The device, once this instance claimed one.
    lease: Option<GrantLease>,
    /// The display, once this instance claimed it.
    display: DisplayOwnership,
    /// The client windows this instance draws in, or the views of
    /// everybody's it composes. Empty on every instance that neither
    /// asks for a window nor composes the desktop.
    surfaces: SurfaceOwnership,
    /// The machine's 3D engine, once this instance claimed it.
    gpu: Gpu3dOwnership,
    /// The playback stream, once this instance claimed one.
    audio: AudioOwnership,
}

impl DeviceOwnership {
    pub const fn new() -> Self {
        Self {
            lease: None,
            display: DisplayOwnership::new(),
            surfaces: SurfaceOwnership::new(),
            gpu: Gpu3dOwnership::new(),
            audio: AudioOwnership::new(),
        }
    }

    /// This instance's side of the 3D path.
    pub const fn gpu(&self) -> &Gpu3dOwnership {
        &self.gpu
    }

    /// This instance's side of the 3D path, to act on.
    pub const fn gpu_mut(&mut self) -> &mut Gpu3dOwnership {
        &mut self.gpu
    }

    /// Take exclusive ownership of the machine's 3D engine. The views
    /// the claim's aperture placements get go in a set of `views` that
    /// ends when the claim does.
    pub fn claim_gpu(
        &mut self,
        service: &Gpu3dService,
        views: &mut ViewTable,
    ) -> Result<(), Gpu3dServiceError> {
        self.gpu.claim(service, views.group())
    }

    /// This instance's side of the surface path.
    pub const fn surfaces(&self) -> &SurfaceOwnership {
        &self.surfaces
    }

    /// This instance's side of the surface path, to act on.
    pub const fn surfaces_mut(&mut self) -> &mut SurfaceOwnership {
        &mut self.surfaces
    }

    /// This instance's side of the audio path.
    pub const fn audio(&self) -> &AudioOwnership {
        &self.audio
    }

    /// This instance's side of the audio path, to act on.
    pub const fn audio_mut(&mut self) -> &mut AudioOwnership {
        &mut self.audio
    }

    /// Take exclusive ownership of one of the machine's playback
    /// streams.
    pub fn claim_audio(
        &mut self,
        service: &AudioService,
        id: u32,
    ) -> Result<(), AudioServiceError> {
        self.audio.claim(service, id)
    }

    /// This instance's side of the display path.
    pub const fn display(&self) -> &DisplayOwnership {
        &self.display
    }

    /// This instance's side of the display path, to act on.
    pub const fn display_mut(&mut self) -> &mut DisplayOwnership {
        &mut self.display
    }

    /// Take exclusive ownership of the machine's display.
    pub fn claim_display(&mut self, service: &DisplayService) -> Result<(), DisplayServiceError> {
        self.display.claim(service)
    }

    /// Whether this instance holds a device.
    pub const fn holds_device(&self) -> bool {
        self.lease.is_some()
    }

    /// Take exclusive ownership of the device `name` names.
    ///
    /// Refused when this instance already holds one — a driver drives
    /// one device, and a second claim would make reclaim ambiguous.
    pub fn claim(
        &mut self,
        registry: &DeviceGrantRegistry,
        name: &str,
        views: &mut ViewTable,
    ) -> Result<(), GrantError> {
        if self.lease.is_some() {
            return Err(GrantError::AlreadyClaimed);
        }
        self.lease = Some(registry.claim(name, views.group())?);
        Ok(())
    }

    pub fn lease(&self) -> Option<&GrantLease> {
        self.lease.as_ref()
    }

    pub fn lease_mut(&mut self) -> Option<&mut GrantLease> {
        self.lease.as_mut()
    }

    /// The lease, checked against the device `name` names.
    ///
    /// A handle that outlived a reclaim names a device this instance no
    /// longer holds; answering it against whatever it holds now would
    /// let a restarted driver drive the wrong hardware.
    pub fn lease_for_mut(&mut self, name: &DeviceName) -> Result<&mut GrantLease, GrantError> {
        match self.lease.as_mut() {
            Some(lease) if lease.grant().name() == name => Ok(lease),
            Some(_) | None => Err(GrantError::NotFound),
        }
    }

    /// Give the device back: every view of it ends, then every source
    /// is masked and every ring unpinned before anyone else is offered
    /// it.
    ///
    /// Fails, holding on to the device, only when a view cannot be ended
    /// for want of memory to put back.
    pub fn release(&mut self, views: &mut ViewTable) -> Result<(), ViewError> {
        let Some(lease) = self.lease.as_ref() else {
            return Ok(());
        };
        lease.end_views(views)?;
        self.lease = None;
        Ok(())
    }
}
