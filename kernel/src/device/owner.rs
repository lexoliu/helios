//! One instance's side of the device path.
//!
//! A driver is an ordinary user-mode instance that happens to hold a
//! device. What its store carries for that is small: where its linear
//! memory sits, how far that memory has been grown, and the lease
//! itself. All three are needed together, because the device's
//! registers are mapped *into* that memory and the mapping has to land
//! somewhere the instance can never grow over.
//!
//! # Concurrency contract
//!
//! Store state is owned by the one task running the instance, so
//! nothing here is shared and nothing here locks. The relay the lease
//! points at is the shared part, and it carries its own
//! synchronisation.

use helios_hal::vmm::VirtAddr;

use crate::display::{DisplayOwnership, DisplayService, DisplayServiceError};
use crate::gpu::{Gpu3dOwnership, Gpu3dService, Gpu3dServiceError};
use crate::surface::{SurfaceOwnership, SurfaceServiceError};

use super::grant::{DeviceName, GrantError};
use super::lease::{
    DEVICE_WINDOW_BYTES, DISPLAY_WINDOW_BYTES, DeviceWindow, GPU_WINDOW_BYTES, GrantLease,
    SURFACE_WINDOW_BYTES,
};
use super::registry::DeviceGrantRegistry;

/// Where an instance's linear memory sits, and how much address space
/// the kernel reserved for it.
///
/// The reservation is what matters, not the current size: the device
/// window lives at the top of the reservation, above everything the
/// instance can ever address, and the instance's growth is capped below
/// it for as long as it holds a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinearMemory {
    /// Where the memory starts in the address space.
    pub base: VirtAddr,
    /// How many bytes of address space the kernel reserved for it.
    pub reservation_bytes: u64,
}

/// One instance's hold on the device path.
///
/// Four things can put memory inside an instance's own: a device grant,
/// whose registers and rings go in the device window; a display claim,
/// whose frame buffers go in the display window immediately below it;
/// the client windows of the surface path below that; and a claim on
/// the 3D engine, whose command buffers and mapped host blobs go in the
/// window below those. They are all here because they are all bounded
/// by the same fact — where this instance's linear memory is and how far
/// it has grown — and an instance that holds any of them must have its
/// growth capped below the lowest window it holds.
#[derive(Default)]
pub struct DeviceOwnership {
    /// Resolved once, after the instance is built. Absent on an
    /// instance whose component has no linear memory, which is an
    /// instance that could not reach a register even if it were given
    /// one.
    memory: Option<LinearMemory>,
    /// Most bytes the instance's memory has ever been grown to.
    high_water_bytes: u64,
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
}

impl DeviceOwnership {
    pub const fn new() -> Self {
        Self {
            memory: None,
            high_water_bytes: 0,
            lease: None,
            display: DisplayOwnership::new(),
            surfaces: SurfaceOwnership::new(),
            gpu: Gpu3dOwnership::new(),
        }
    }

    /// Record where the instance's linear memory ended up.
    ///
    /// Called once, by whoever built the instance, as soon as the
    /// runtime can say. Doing it here rather than at claim time is what
    /// lets a claim be an ordinary host call: by then the answer is
    /// already known.
    pub fn set_memory(&mut self, memory: LinearMemory) {
        self.memory = Some(memory);
    }

    pub const fn memory(&self) -> Option<LinearMemory> {
        self.memory
    }

    /// Record that the instance's memory has been grown to `bytes`.
    pub fn note_growth(&mut self, bytes: u64) {
        self.high_water_bytes = self.high_water_bytes.max(bytes);
    }

    /// The most bytes the instance's memory may hold, while it holds a
    /// device or the display.
    ///
    /// A `memory.grow` past a window would put ordinary memory on top of
    /// a register file or of a frame buffer the display engine is
    /// scanning out. The limit is the lower of the windows the instance
    /// actually holds; there is none before a claim, because an
    /// instance that holds neither is an ordinary instance and pays
    /// nothing for the path existing.
    pub fn growth_limit(&self) -> Option<u64> {
        let device = self
            .window()
            .filter(|_| self.lease.is_some())
            .map(|window| window.offset());
        let display = self.display.window().map(|window| window.offset());
        let surfaces = self.surfaces.window().map(|window| window.offset());
        let gpu = self.gpu.window().map(|window| window.offset());
        [device, display, surfaces, gpu].into_iter().flatten().min()
    }

    /// The window this instance's device mappings would live in.
    pub fn window(&self) -> Option<DeviceWindow> {
        self.memory
            .filter(|memory| memory.reservation_bytes > DEVICE_WINDOW_BYTES)
            .map(|memory| DeviceWindow::top_of(memory.base, memory.reservation_bytes))
    }

    /// The window this instance's display frame buffers would live in,
    /// which is the span immediately below the device window.
    pub fn display_window(&self) -> Option<DeviceWindow> {
        self.memory
            .filter(|memory| memory.reservation_bytes > DEVICE_WINDOW_BYTES + DISPLAY_WINDOW_BYTES)
            .map(|memory| {
                DeviceWindow::top_of(memory.base, memory.reservation_bytes)
                    .below(DISPLAY_WINDOW_BYTES)
            })
    }

    /// The window this instance's client windows would live in, which
    /// is the span immediately below the display window.
    ///
    /// Below rather than beside: an instance may hold the display *and*
    /// draw in a window of its own, and the growth cap has to be under
    /// the lowest window it holds either way.
    pub fn surface_window(&self) -> Option<DeviceWindow> {
        self.memory
            .filter(|memory| {
                memory.reservation_bytes
                    > DEVICE_WINDOW_BYTES + DISPLAY_WINDOW_BYTES + SURFACE_WINDOW_BYTES
            })
            .map(|memory| {
                DeviceWindow::top_of(memory.base, memory.reservation_bytes)
                    .below(DISPLAY_WINDOW_BYTES)
                    .below(SURFACE_WINDOW_BYTES)
            })
    }

    /// The window this instance's renderer state would live in, which
    /// is the span immediately below the surface window.
    ///
    /// Below rather than beside, for the reason the surface window is:
    /// one instance may hold the display, draw in a window of its own
    /// *and* drive the renderer, and the growth cap has to be under the
    /// lowest window it holds whichever of them that is.
    pub fn gpu_window(&self) -> Option<DeviceWindow> {
        self.memory
            .filter(|memory| {
                memory.reservation_bytes
                    > DEVICE_WINDOW_BYTES
                        + DISPLAY_WINDOW_BYTES
                        + SURFACE_WINDOW_BYTES
                        + GPU_WINDOW_BYTES
            })
            .map(|memory| {
                DeviceWindow::top_of(memory.base, memory.reservation_bytes)
                    .below(DISPLAY_WINDOW_BYTES)
                    .below(SURFACE_WINDOW_BYTES)
                    .below(GPU_WINDOW_BYTES)
            })
    }

    /// This instance's side of the 3D path.
    pub const fn gpu(&self) -> &Gpu3dOwnership {
        &self.gpu
    }

    /// This instance's side of the 3D path, to act on.
    pub const fn gpu_mut(&mut self) -> &mut Gpu3dOwnership {
        &mut self.gpu
    }

    /// Take exclusive ownership of the machine's 3D engine.
    ///
    /// Refused when the instance's memory has already grown over the
    /// window its command buffers and its mapped blobs would go in, for
    /// the same reason a display claim is: they would land on memory it
    /// is using.
    pub fn claim_gpu(&mut self, service: &Gpu3dService) -> Result<(), Gpu3dServiceError> {
        let window = self
            .gpu_window()
            .ok_or(Gpu3dServiceError::WindowExhausted)?;
        if self.high_water_bytes > window.offset() {
            return Err(Gpu3dServiceError::WindowExhausted);
        }
        self.gpu.claim(service, window)
    }

    /// This instance's side of the surface path.
    pub const fn surfaces(&self) -> &SurfaceOwnership {
        &self.surfaces
    }

    /// This instance's side of the surface path, to act on, alongside
    /// the window its pages go in.
    ///
    /// The two come together because they are needed together and
    /// because the window is what this side cannot work out for itself:
    /// where the instance's linear memory is.
    ///
    /// Refused when the instance's memory has already grown over that
    /// window, for the same reason a display claim is: the pages would
    /// land on memory it is using.
    pub fn surfaces_mut(
        &mut self,
    ) -> Result<(&mut SurfaceOwnership, DeviceWindow), SurfaceServiceError> {
        let window = self
            .surface_window()
            .ok_or(SurfaceServiceError::WindowExhausted)?;
        if self.high_water_bytes > window.offset() {
            return Err(SurfaceServiceError::WindowExhausted);
        }
        Ok((&mut self.surfaces, window))
    }

    /// Drop this instance's view of the window `id` names.
    ///
    /// The compositor's half of a surface's teardown, and a no-op on an
    /// instance that never held a view — which is every instance that is
    /// not the compositor.
    pub fn unmap_surface_view(&mut self, id: crate::surface::SurfaceId) {
        self.surfaces.unmap_view(id);
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
    ///
    /// Refused when the instance's memory has already grown over the
    /// window its frame buffers would be pinned in, for the same reason
    /// a device claim is: the pages would land on memory it is using.
    pub fn claim_display(&mut self, service: &DisplayService) -> Result<(), DisplayServiceError> {
        let window = self
            .display_window()
            .ok_or(DisplayServiceError::WindowExhausted)?;
        if self.high_water_bytes > window.offset() {
            return Err(DisplayServiceError::WindowExhausted);
        }
        self.display.claim(service, window)
    }

    /// Whether this instance holds a device.
    pub const fn holds_device(&self) -> bool {
        self.lease.is_some()
    }

    /// Take exclusive ownership of the device `name` names.
    ///
    /// Refused when this instance already holds one — a driver drives
    /// one device, and a second claim would make reclaim ambiguous —
    /// and when the instance's memory has already grown over the window
    /// the device would be mapped into.
    pub fn claim(&mut self, registry: &DeviceGrantRegistry, name: &str) -> Result<(), GrantError> {
        if self.lease.is_some() {
            return Err(GrantError::AlreadyClaimed);
        }
        let window = self.window().ok_or(GrantError::WindowExhausted)?;
        if self.high_water_bytes > window.offset() {
            return Err(GrantError::WindowExhausted);
        }
        self.lease = Some(registry.claim(name, window)?);
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

    /// Give the device back, masking, unmapping and unpinning
    /// everything before anyone else is offered it.
    pub fn release(&mut self) {
        self.lease = None;
    }
}
