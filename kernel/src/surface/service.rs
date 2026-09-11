//! The registry the rest of the kernel reaches client windows through.
//!
//! A surface has two sides that never meet: a resource in the client's
//! table, and a number the compositor was given. This is what holds them
//! together — the identity, the geometry, the queue of input the
//! compositor routed to it, and the arena a dying client handed back.
//!
//! # Concurrency contract
//!
//! [`SurfaceService`] is cloneable and every method on it may be called
//! from any processor. The live table is a short spin-locked span walked
//! only when a surface is created, destroyed, or handed input — never on
//! a path that draws — and no interrupt handler can reach it, so it
//! needs no local mask.
//!
//! Each surface's event queue has exactly one producer — the compositor,
//! through `deliver` — and one consumer per stream the client opened.
//! The producer never waits: a report that does not fit is dropped whole
//! and counted, because parking the compositor inside `deliver` would
//! park the desktop. The consumer parks on [`Notify`], armed before it
//! looks at the queue, so a report published between the look and the
//! park wakes it.
//!
//! What a client hands back on death is not a message either: a drop
//! cannot await room in a queue, so the arena goes on an unbounded
//! return queue and the supervisor is signalled.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::task::{Context, Poll};

use alloc::vec::Vec;
use arrayvec::ArrayVec;
use concurrent_queue::ConcurrentQueue;
use futures::channel::oneshot;
use helios_hal::input::InputEvent;
use helios_hal::iommu::PhysicalRange;
use spin::Mutex;
use triomphe::Arc;

use crate::InstanceId;
use crate::exec::{Notify, NotifyWaiter};
use crate::pins::PinnedArena;

use super::SurfaceServiceError;

/// Surfaces one instance may hold at once.
///
/// A program with eight windows open is a program, not a compositor; the
/// bound is what keeps a client's arena a value on its store's own stack
/// rather than an allocation whose size a guest chooses.
pub const MAX_INSTANCE_SURFACES: usize = 8;

/// Surfaces the machine may have on the desktop at once.
///
/// Every one of them costs the compositor a mapped view inside its own
/// surface window, so the bound is the compositor's arena as much as the
/// registry's.
pub const MAX_LIVE_SURFACES: usize = 64;

/// Events one surface's queue holds before a report is lost.
///
/// The same depth `helios:system/input` gives a device, for the same
/// reason: a client that has fallen a whole device ring behind is a
/// client the user has already outrun.
pub const SURFACE_EVENT_QUEUE_DEPTH: usize = 64;

/// Surface calls that may be queued for the compositor before a client
/// waits.
pub const SURFACE_REQUEST_QUEUE_DEPTH: usize = 16;

/// Stable identity of one window, minted by the kernel and never reused
/// while the machine runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SurfaceId(u64);

impl SurfaceId {
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// The identity a guest named.
    ///
    /// Every identity the kernel minted is live in the registry or it is
    /// not; a number a guest invented names nothing and is refused by
    /// the lookup, so this needs no validation of its own.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// The pixel geometry of one window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceGeometry {
    pub width: u32,
    pub height: u32,
}

impl SurfaceGeometry {
    /// How many bytes one frame of this geometry occupies, in the
    /// four-byte pixels every Helios display format uses.
    ///
    /// `None` for a geometry with no pixels, or one larger than any
    /// memory could back.
    pub const fn frame_bytes(self) -> Option<u64> {
        if self.width == 0 || self.height == 0 {
            return None;
        }
        let Some(pixels) = (self.width as u64).checked_mul(self.height as u64) else {
            return None;
        };
        pixels.checked_mul(crate::pins::BYTES_PER_PIXEL as u64)
    }
}

/// A rectangular region of one window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl SurfaceRect {
    /// Whether this rectangle lies wholly inside `geometry`.
    pub const fn fits(self, geometry: SurfaceGeometry) -> bool {
        let Some(right) = self.x.checked_add(self.width) else {
            return false;
        };
        let Some(bottom) = self.y.checked_add(self.height) else {
            return false;
        };
        self.width > 0 && self.height > 0 && right <= geometry.width && bottom <= geometry.height
    }
}

/// One window, as everything outside the client's store sees it.
pub struct SurfaceShared {
    id: SurfaceId,
    geometry: SurfaceGeometry,
    /// Whole reports the compositor routed here, oldest first.
    events: ConcurrentQueue<InputEvent>,
    /// Raised once per routed report.
    published: Notify,
    /// Cleared when the client drops the surface, and when the
    /// compositor dies under it.
    alive: AtomicBool,
    /// Events handed to the client's reader.
    delivered: AtomicU64,
    /// Reports dropped because the client had not kept up.
    lost_reports: AtomicU64,
}

impl SurfaceShared {
    pub const fn id(&self) -> SurfaceId {
        self.id
    }

    pub const fn geometry(&self) -> SurfaceGeometry {
        self.geometry
    }

    /// Whether this surface is still on the desktop.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// Take this surface off the desktop, from either end.
    pub fn retire(&self) {
        self.alive.store(false, Ordering::Release);
        self.published.notify_all();
    }

    /// Hand one whole report to the client that owns this surface.
    ///
    /// Never waits. The caller is the compositor, and a compositor
    /// parked inside `deliver` is a desktop that has stopped redrawing.
    pub fn publish_report(&self, report: &[InputEvent]) -> bool {
        if !self.is_alive() {
            return false;
        }
        if report.len() > SURFACE_EVENT_QUEUE_DEPTH
            || self.events.len() + report.len() > SURFACE_EVENT_QUEUE_DEPTH
        {
            let lost = self.lost_reports.fetch_add(1, Ordering::AcqRel) + 1;
            if lost == 1 {
                tracing::warn!(
                    target: "helios_kernel::surface",
                    surface = self.id.raw(),
                    events = report.len(),
                    "an input report was dropped: this surface's client is not reading it"
                );
            }
            return false;
        }
        for event in report {
            self.events
                .push(*event)
                .expect("the only producer just measured room for this whole report");
        }
        self.delivered
            .fetch_add(report.len() as u64, Ordering::AcqRel);
        self.published.notify_all();
        true
    }

    /// A reader of this surface's routed input.
    ///
    /// The wait is armed as the reader is built, so an event routed
    /// between here and the first poll wakes it rather than being waited
    /// past.
    pub fn events(shared: &Arc<Self>) -> SurfaceEvents {
        SurfaceEvents {
            waiter: shared.published.waiter(),
            shared: shared.clone(),
        }
    }
}

/// Events the compositor routed to one surface, as many as one drain of
/// its queue yields.
pub type SurfaceEventBurst = ArrayVec<InputEvent, SURFACE_EVENT_QUEUE_DEPTH>;

/// One reader of one surface's routed input.
///
/// # Concurrency contract
///
/// Owned by whatever task drives the stream, and the wait it carries is
/// armed before every look at the queue, so a report routed between a
/// look and a park cannot be slept through.
pub struct SurfaceEvents {
    shared: Arc<SurfaceShared>,
    waiter: NotifyWaiter,
}

impl SurfaceEvents {
    /// Every event queued right now, a park until one is, or `None` once
    /// the surface is gone.
    pub fn poll_burst(&mut self, cx: &mut Context<'_>) -> Poll<Option<SurfaceEventBurst>> {
        loop {
            let mut burst = SurfaceEventBurst::new();
            while !burst.is_full() {
                match self.shared.events.pop() {
                    Ok(event) => burst.push(event),
                    Err(_) => break,
                }
            }
            if !burst.is_empty() {
                return Poll::Ready(Some(burst));
            }
            if !self.shared.is_alive() {
                return Poll::Ready(None);
            }
            match self.shared.published.poll_notified(cx, &mut self.waiter) {
                Poll::Ready(()) => continue,
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// What one client handed back when it died.
///
/// The arena is in here rather than freed by the drop that produced it,
/// because the compositor may still be composing from those pages: the
/// supervisor takes the windows off the desktop and drops its views
/// first, and the arena's own drop is what gives the pages back.
pub struct ReturnedSurfaces {
    /// The windows this client held, in no particular order.
    pub ids: ArrayVec<SurfaceId, MAX_INSTANCE_SURFACES>,
    /// The pages they lived in.
    pub pins: PinnedArena<MAX_LIVE_SURFACES>,
}

/// One `create` the kernel forwards to the compositor.
pub struct SurfaceCreate {
    pub id: SurfaceId,
    pub geometry: SurfaceGeometry,
    /// The client's pinned run, which the supervisor maps a second time
    /// into the compositor's own linear memory before it calls the
    /// guest.
    pub physical: PhysicalRange,
    pub reply: oneshot::Sender<Result<(), SurfaceServiceError>>,
}

/// Work the compositor's supervisor takes off the provider slot.
pub enum SurfaceRequest {
    /// A client asked for a window.
    Create(SurfaceCreate),
    /// A client published the pixels in a region of one.
    Commit {
        id: SurfaceId,
        region: SurfaceRect,
        reply: oneshot::Sender<Result<(), SurfaceServiceError>>,
    },
    /// A client dropped one window while staying alive. Its pages stay
    /// pinned in the client's arena until that arena ends, so the
    /// supervisor drops only its own view.
    Destroy { id: SurfaceId },
}

/// The registry every side of the surface path shares.
struct SurfaceRegistry {
    next_id: AtomicU64,
    live: Mutex<Vec<Arc<SurfaceShared>>>,
    /// Which instance the kernel provisioned as the compositor. Zero
    /// until the supervisor has one, which is also what makes `deliver`
    /// refuse everybody before there is a desktop.
    compositor: AtomicU64,
    /// Arenas dying clients handed back, waiting for the supervisor.
    returned: ConcurrentQueue<ReturnedSurfaces>,
    /// Raised once per arena handed back.
    returns: Notify,
    /// Windows the machine has had since boot.
    created: AtomicU64,
}

/// The machine's client windows, as everything outside the compositor's
/// store sees them.
#[derive(Clone)]
pub struct SurfaceService {
    inner: Arc<SurfaceRegistry>,
}

impl Default for SurfaceService {
    fn default() -> Self {
        Self::new()
    }
}

impl SurfaceService {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(SurfaceRegistry {
                next_id: AtomicU64::new(1),
                live: Mutex::new(Vec::new()),
                compositor: AtomicU64::new(0),
                returned: ConcurrentQueue::unbounded(),
                returns: Notify::new(),
                created: AtomicU64::new(0),
            }),
        }
    }

    /// Record which instance is the compositor.
    ///
    /// Called by the supervisor each time it builds one. Only that
    /// instance may `deliver`, so a program cannot inject input into its
    /// neighbour by guessing a surface identifier.
    pub fn set_compositor(&self, instance: InstanceId) {
        self.inner
            .compositor
            .store(instance.raw(), Ordering::Release);
    }

    /// Forget the compositor and take every window off the desktop.
    ///
    /// Called when the plugin dies. The clients keep their pages — those
    /// are theirs — and learn the surface is `gone` from the next call
    /// they make on it.
    pub fn retire_compositor(&self) {
        self.inner.compositor.store(0, Ordering::Release);
        let live = core::mem::take(&mut *self.inner.live.lock());
        for surface in &live {
            surface.retire();
        }
    }

    /// Whether `instance` is the compositor.
    pub fn is_compositor(&self, instance: InstanceId) -> bool {
        let compositor = self.inner.compositor.load(Ordering::Acquire);
        compositor != 0 && compositor == instance.raw()
    }

    /// How many windows the machine has had since boot.
    pub fn created(&self) -> u64 {
        self.inner.created.load(Ordering::Acquire)
    }

    /// How many windows are on the desktop right now.
    pub fn live(&self) -> usize {
        self.inner.live.lock().len()
    }

    /// Mint a window of `geometry` and put it in the registry.
    pub fn register(
        &self,
        geometry: SurfaceGeometry,
    ) -> Result<Arc<SurfaceShared>, SurfaceServiceError> {
        let mut live = self.inner.live.lock();
        if live.len() >= MAX_LIVE_SURFACES {
            return Err(SurfaceServiceError::TooManySurfaces);
        }
        let id = SurfaceId(self.inner.next_id.fetch_add(1, Ordering::AcqRel));
        let shared = Arc::new(SurfaceShared {
            id,
            geometry,
            events: ConcurrentQueue::bounded(SURFACE_EVENT_QUEUE_DEPTH),
            published: Notify::new(),
            alive: AtomicBool::new(true),
            delivered: AtomicU64::new(0),
            lost_reports: AtomicU64::new(0),
        });
        live.push(shared.clone());
        self.inner.created.fetch_add(1, Ordering::AcqRel);
        Ok(shared)
    }

    /// Take one window off the desktop and out of the registry.
    pub fn unregister(&self, id: SurfaceId) {
        let mut live = self.inner.live.lock();
        if let Some(index) = live.iter().position(|surface| surface.id == id) {
            let surface = live.swap_remove(index);
            drop(live);
            surface.retire();
        }
    }

    /// The window `id` names, while it is on the desktop.
    pub fn lookup(&self, id: SurfaceId) -> Option<Arc<SurfaceShared>> {
        self.inner
            .live
            .lock()
            .iter()
            .find(|surface| surface.id == id)
            .cloned()
    }

    /// Hand a dead client's arena to the supervisor.
    ///
    /// Never waits and never fails: the queue is unbounded because the
    /// alternative is a drop that has to park, and a drop cannot.
    pub fn return_surfaces(&self, returned: ReturnedSurfaces) {
        for id in &returned.ids {
            self.unregister(*id);
        }
        self.inner
            .returned
            .push(returned)
            .unwrap_or_else(|_| panic!("the surface return queue is never closed"));
        self.inner.returns.notify_all();
    }

    /// Everything handed back since the last drain.
    pub fn take_returned(&self) -> Vec<ReturnedSurfaces> {
        let mut returned = Vec::new();
        while let Ok(entry) = self.inner.returned.pop() {
            returned.push(entry);
        }
        returned
    }

    /// A waiter on the return queue, armed before its first look.
    pub fn returns_waiter(&self) -> NotifyWaiter {
        self.inner.returns.waiter()
    }

    /// Park until something is handed back.
    pub fn poll_returns(&self, cx: &mut Context<'_>, waiter: &mut NotifyWaiter) -> Poll<()> {
        self.inner.returns.poll_notified(cx, waiter)
    }
}
