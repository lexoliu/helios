//! One instance's session on the machine's 3D engine.
//!
//! What an instance's store carries for the renderer is small: its
//! session, how many `gpu` handles hold it open, and the set of views —
//! command buffers, guest blobs, aperture placements — that ends when
//! the session does. The views live in the store's [`ViewTable`]; the
//! buffers behind them are held by the handles that name them and by
//! the requests that have the device read them.
//!
//! One session per instance, however many `gpu` handles it opens: a
//! second `open` is another handle onto the session the first made, so
//! the instance's own bounds on contexts and blobs are bounds on what
//! the instance holds rather than on what one of its handles does.
//!
//! # Concurrency contract
//!
//! Store state is owned by the one task running the instance, so
//! nothing here is shared and nothing here locks. The session record
//! and the request queues behind [`Gpu3dSession`] are the shared part
//! and carry their own synchronisation.
//!
//! # What an end has to do, and what it must not
//!
//! Ending the session while the instance lives — its last `gpu` handle
//! dropped — ends every view of the session first: an aperture
//! placement must not stay shown in the instance's memory once the owner
//! task takes the blob out of the aperture, and a buffer shown nowhere
//! goes back to the pool as soon as the device has let go of it. Dying
//! ends them the same way, earlier: the store's view table is dropped
//! before this, so every span is withdrawn by the time the session's
//! drop raises the release. Neither path awaits the device; the owner
//! task destroys the session's contexts, unmaps and destroys its blobs,
//! and drops its references to their buffers.

use crate::buffer::{ViewError, ViewGroup, ViewTable};

use crate::process::GpuOpenCap;

use super::{Gpu3dServiceError, MAX_GPU_VIEWS};

use super::service::{Gpu3dService, Gpu3dSession, SessionId};

/// One instance's side of the 3D path.
#[derive(Default)]
pub struct Gpu3dOwnership {
    /// The session, once this instance opened one.
    session: Option<Gpu3dSession>,
    /// The set of views that ends when the session does. Present
    /// exactly when `session` is.
    views: Option<ViewGroup>,
    /// How many `gpu` handles hold the session open.
    handles: usize,
}

impl Gpu3dOwnership {
    pub const fn new() -> Self {
        Self {
            session: None,
            views: None,
            handles: 0,
        }
    }

    /// Whether this instance has a session open.
    pub const fn is_open(&self) -> bool {
        self.session.is_some()
    }

    /// Open this instance's session, or hold the open one once more.
    ///
    /// `views` is the set every view of a new session goes in; it is
    /// unused when the session is already open, whose set it already
    /// has.
    pub fn open(
        &mut self,
        service: &Gpu3dService,
        views: impl FnOnce() -> ViewGroup,
        cap: GpuOpenCap,
    ) -> Result<SessionId, Gpu3dServiceError> {
        if self.session.is_none() {
            self.session = Some(service.open(cap)?);
            self.views = Some(views());
        }
        self.handles += 1;
        Ok(self.session_ref()?.id())
    }

    /// The session, or the reason there is none.
    pub fn session_ref(&self) -> Result<&Gpu3dSession, Gpu3dServiceError> {
        self.session.as_ref().ok_or(Gpu3dServiceError::SessionEnded)
    }

    /// The session, if it is the one `id` names.
    pub fn session_of(&self, id: SessionId) -> Result<&Gpu3dSession, Gpu3dServiceError> {
        match &self.session {
            Some(session) if session.id() == id => Ok(session),
            _ => Err(Gpu3dServiceError::SessionEnded),
        }
    }

    /// Whether `id` names the session that is open now.
    pub fn is_current(&self, id: SessionId) -> bool {
        self.session_of(id).is_ok()
    }

    /// The set the session's views go in, or the reason there is none.
    pub fn views(&self) -> Result<ViewGroup, Gpu3dServiceError> {
        self.views.ok_or(Gpu3dServiceError::SessionEnded)
    }

    /// The set the session's next view goes in, refused once the
    /// session shows [`MAX_GPU_VIEWS`]: the session's share of the
    /// store's table, which a plugin creating command buffers without
    /// bound would otherwise take from the store's other paths.
    pub fn room_for_view(&self, table: &ViewTable) -> Result<ViewGroup, Gpu3dServiceError> {
        let group = self.views()?;
        if table.group_len(group) >= MAX_GPU_VIEWS {
            return Err(Gpu3dServiceError::TooManyViews);
        }
        Ok(group)
    }

    /// Let one `gpu` handle of session `id` go, ending the session when
    /// it was the last.
    ///
    /// A handle of a session that has already ended changes nothing.
    /// Ending a session ends every view of it first, then the session's
    /// drop raises the release. Fails, holding on to the session, only
    /// when a view cannot be ended for want of memory to put back.
    pub fn close(&mut self, id: SessionId, views: &mut ViewTable) -> Result<(), ViewError> {
        if !self.is_current(id) {
            return Ok(());
        }
        if self.handles > 1 {
            self.handles -= 1;
            return Ok(());
        }
        if let Some(group) = self.views {
            views.end_group(group)?;
        }
        self.views = None;
        self.session = None;
        self.handles = 0;
        Ok(())
    }
}
