//! The handle the rest of the kernel reaches the display through.
//!
//! The display device is a backend type — a virtio-gpu resource behind
//! whichever transport the platform exposes it on — and the component
//! host that serves `helios:system/display` never names it. What crosses
//! that boundary here is a queue rather than a trait object: the device
//! stays owned, whole, by one task per queue it has, and everything else
//! asks that task for work by sending it a message and awaiting the
//! reply the message carried along. There is no vtable between a
//! compositor and its display engine, and no lock around the device.
//!
//! # Concurrency contract
//!
//! [`DisplayService`] is cloneable and every method on it may be called
//! from any processor. `claim` is a compare-and-exchange on one word;
//! the request queues are the kernel's bounded provider queues, whose
//! producers park on a permit-based notification rather than spinning,
//! and whose single consumer is the owner task in [`super::owner`].
//!
//! Two queues, because the hardware has two. Frames go on the control
//! queue, where each one waits for the display engine to finish the one
//! before it; pointer motion goes on the cursor queue, which is served
//! by its own task, so moving the pointer never waits behind a frame
//! somebody is presenting.
//!
//! A claim's release is not a message. The store that holds a claim is
//! dropped by whatever kills its instance, and a drop cannot await a
//! queue that is full, so the release is one permit on a [`Notify`] the
//! owner task races against its inbox. The claim word moves to
//! [`ClaimState::RELEASING`] at the same moment, so nobody is handed the
//! display between the moment its last owner let go and the moment the
//! display engine has actually given the resources back.

use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use core::task::{Context, Poll};

use futures::channel::oneshot;
use helios_hal::display::{
    CursorImage, DisplayMode, FramebufferId, PixelFormat, Point, Rect, ScanoutId, ScanoutList,
};
use helios_hal::pmm::PhysFrameRange;
use triomphe::Arc;

use concurrent_queue::ConcurrentQueue;

use crate::component::{ProviderError, ProviderSender};
use crate::exec::{Notify, NotifyWaiter};

use super::DisplayPins;
use super::DisplayServiceError;

/// Requests one claim may have in flight on each queue before its next
/// one waits for room.
///
/// A compositor that has queued this many frames is a compositor the
/// display engine has not kept up with, and making it wait is the
/// backpressure a display path needs: the alternative is a queue that
/// grows until the machine is out of memory.
pub const REQUEST_QUEUE_DEPTH: usize = 8;

/// One frame that reached the display engine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameToken {
    /// How many frames this surface has published, this one included.
    pub sequence: u64,
    /// The kernel's monotonic clock when the flush completed, in
    /// nanoseconds since boot.
    pub presented_nanos: u64,
}

/// What the display's claim word holds.
///
/// Three states rather than two, because giving the resources back is
/// asynchronous: the display engine has to be told to drop every
/// resource and blank every scanout, and until it has, the display is
/// neither held by anybody nor free for anybody.
pub(super) struct ClaimState;

impl ClaimState {
    /// Nobody holds the display.
    pub(super) const FREE: u8 = 0;
    /// One instance holds it.
    pub(super) const HELD: u8 = 1;
    /// Its last owner let go and the owner task has not finished
    /// handing the resources back.
    pub(super) const RELEASING: u8 = 2;
}

/// A counted event a stream in some instance's store is driven by.
///
/// Two of them exist: one per surface, bumped every time the display
/// engine finishes a flush, and one per device, bumped every time the
/// set of outputs changes. Both are read the same way — "has the count
/// moved since I last looked, and if not, wake me when it does" — so
/// they are one type rather than two that differ in their field names.
///
/// # Concurrency contract
///
/// Written by the owner tasks and read from whichever processor runs the
/// reading store. A reader arms the notification inside
/// [`Self::poll_next`] before it re-reads the count, so an event
/// published between the read and the park wakes it; the notification is
/// a broadcast, because every reader of one signal is owed the same
/// event.
#[derive(Default)]
pub struct SequenceSignal {
    sequence: AtomicU64,
    nanos: AtomicU64,
    published: Notify,
}

impl SequenceSignal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record event number `sequence`, taken at `nanos` on the kernel's
    /// monotonic clock, and wake every reader waiting for one.
    pub fn publish(&self, sequence: u64, nanos: u64) {
        self.nanos.store(nanos, Ordering::Release);
        self.sequence.store(sequence, Ordering::Release);
        self.published.notify_all();
    }

    /// Bump the count by one, taking the time from `nanos`.
    pub fn bump(&self, nanos: u64) -> u64 {
        let next = self.sequence.load(Ordering::Acquire) + 1;
        self.publish(next, nanos);
        next
    }

    /// How many events this signal has published.
    pub fn sequence(&self) -> u64 {
        self.sequence.load(Ordering::Acquire)
    }

    /// A fresh wait on this signal.
    pub fn waiter(&self) -> NotifyWaiter {
        self.published.waiter()
    }

    /// The next event published after `last_seen`, as its count and the
    /// clock reading it was taken at.
    pub fn poll_next(
        &self,
        cx: &mut Context<'_>,
        waiter: &mut NotifyWaiter,
        last_seen: &mut u64,
    ) -> Poll<(u64, u64)> {
        loop {
            let sequence = self.sequence.load(Ordering::Acquire);
            if sequence != *last_seen {
                *last_seen = sequence;
                return Poll::Ready((sequence, self.nanos.load(Ordering::Acquire)));
            }
            match self.published.poll_notified(cx, waiter) {
                Poll::Ready(()) => continue,
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Work for the task that owns the display engine's control queue.
///
/// Every variant carries the claim generation it was made under. A
/// request that outlived its claim names a display its instance no
/// longer holds, and serving it against whoever holds the display now
/// would let a dead compositor draw on a live one's screen.
pub(crate) enum ControlRequest {
    Scanouts {
        generation: u64,
        reply: oneshot::Sender<Result<ScanoutList, DisplayServiceError>>,
    },
    Modes {
        generation: u64,
        scanout: ScanoutId,
        reply: oneshot::Sender<Result<DisplayMode, DisplayServiceError>>,
    },
    CreateSurface {
        generation: u64,
        scanout: ScanoutId,
        mode: DisplayMode,
        format: PixelFormat,
        backing: PhysFrameRange,
        reply: oneshot::Sender<Result<FramebufferId, DisplayServiceError>>,
    },
    DestroySurface {
        generation: u64,
        framebuffer: FramebufferId,
    },
    Present {
        generation: u64,
        framebuffer: FramebufferId,
        region: Rect,
        vsync: Arc<SequenceSignal>,
        reply: oneshot::Sender<Result<FrameToken, DisplayServiceError>>,
    },
    CreateCursor {
        generation: u64,
        format: PixelFormat,
        backing: PhysFrameRange,
        reply: oneshot::Sender<Result<FramebufferId, DisplayServiceError>>,
    },
    SetCursor {
        generation: u64,
        scanout: ScanoutId,
        position: Point,
        image: CursorImage,
        reply: oneshot::Sender<Result<(), DisplayServiceError>>,
    },
}

impl ControlRequest {
    pub(crate) const fn generation(&self) -> u64 {
        match self {
            Self::Scanouts { generation, .. }
            | Self::Modes { generation, .. }
            | Self::CreateSurface { generation, .. }
            | Self::DestroySurface { generation, .. }
            | Self::Present { generation, .. }
            | Self::CreateCursor { generation, .. }
            | Self::SetCursor { generation, .. } => *generation,
        }
    }
}

/// Work for the task that owns the display engine's cursor queue.
pub(crate) enum CursorRequest {
    Move {
        generation: u64,
        scanout: ScanoutId,
        position: Point,
        reply: oneshot::Sender<Result<(), DisplayServiceError>>,
    },
    Hide {
        generation: u64,
        scanout: ScanoutId,
        reply: oneshot::Sender<Result<(), DisplayServiceError>>,
    },
}

impl CursorRequest {
    pub(crate) const fn generation(&self) -> u64 {
        match self {
            Self::Move { generation, .. } | Self::Hide { generation, .. } => *generation,
        }
    }
}

/// What every holder of the display shares with the tasks that own it.
pub(super) struct DisplayShared {
    pub(super) control: ProviderSender<ControlRequest>,
    pub(super) cursor: ProviderSender<CursorRequest>,
    /// One of [`ClaimState`]'s three values.
    pub(super) claim: AtomicU8,
    /// Bumped by every successful claim, so a request that outlived its
    /// claim can be told from one that did not.
    pub(super) generation: AtomicU64,
    /// One permit per claim that has been let go.
    pub(super) release: Notify,
    /// How many outputs the device reported when the kernel brought it
    /// up. A claim reads the live set with `scanouts`; this is what a
    /// caller that has not claimed anything can still be told.
    pub(super) scanout_count: AtomicU32,
    /// Every change to the device's set of outputs, as the topology
    /// follower publishes them.
    ///
    /// Shared rather than owned, because a `changed` stream reader in
    /// some instance's store outlives the host call that built it and
    /// holds the signal directly.
    pub(super) changes: Arc<SequenceSignal>,
    /// The frame buffers of a claim that has been let go, waiting for
    /// the owner task to stop the display engine reading them.
    ///
    /// One slot, and one is provably enough: there is at most one claim
    /// at a time, and the claim word does not return to
    /// [`ClaimState::FREE`] until this has been drained.
    pub(super) returned: ConcurrentQueue<DisplayPins>,
}

/// The kernel's display, as everything outside the owner tasks sees it.
#[derive(Clone)]
pub struct DisplayService {
    shared: Arc<DisplayShared>,
}

impl DisplayService {
    pub(super) const fn from_shared(shared: Arc<DisplayShared>) -> Self {
        Self { shared }
    }

    /// How many outputs the device presented at bring-up.
    pub fn scanout_count(&self) -> u32 {
        self.shared.scanout_count.load(Ordering::Acquire)
    }

    /// The signal every change to the set of outputs is published on.
    pub fn changes(&self) -> Arc<SequenceSignal> {
        self.shared.changes.clone()
    }

    /// Whether an instance holds the display right now.
    pub fn is_claimed(&self) -> bool {
        self.shared.claim.load(Ordering::Acquire) != ClaimState::FREE
    }

    /// Take exclusive ownership of the display.
    ///
    /// The second caller is refused rather than queued, and so is a
    /// caller that arrives while the previous owner's resources are
    /// still being handed back: the display is not free until the
    /// display engine says it is, and answering otherwise would hand out
    /// a screen with somebody else's pixels still on it.
    pub fn claim(&self) -> Result<DisplayClaim, DisplayServiceError> {
        self.shared
            .claim
            .compare_exchange(
                ClaimState::FREE,
                ClaimState::HELD,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| DisplayServiceError::AlreadyClaimed)?;
        let generation = self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        Ok(DisplayClaim {
            shared: self.shared.clone(),
            generation,
        })
    }
}

/// One instance's hold on the display.
///
/// Dropping it — or dying while holding it — moves the display into
/// [`ClaimState::RELEASING`] and wakes the owner task, which releases
/// every resource this claim created and leaves the scanouts blank
/// before the display is offered to anyone else.
pub struct DisplayClaim {
    shared: Arc<DisplayShared>,
    generation: u64,
}

impl DisplayClaim {
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// A handle that can queue work for this claim.
    ///
    /// Cheap and cloneable, so a host call that has to keep sending
    /// after it has given the store back — a `present` whose future the
    /// guest awaits long after the call returned — carries one instead
    /// of borrowing the claim. A sender that outlives its claim sends
    /// requests the owner task drops, because they no longer name the
    /// generation it is serving.
    pub fn sender(&self) -> DisplaySender {
        DisplaySender {
            shared: self.shared.clone(),
            generation: self.generation,
        }
    }

    /// The signal every change to the set of outputs is published on.
    pub fn changes(&self) -> Arc<SequenceSignal> {
        self.shared.changes.clone()
    }

    /// Hand this claim's frame buffers to the owner task.
    ///
    /// The pages are not freed here: the display engine may still be
    /// scanning them out, and the owner task is what knows when it has
    /// stopped. Called by the store's own drop, so it neither allocates
    /// nor waits.
    ///
    /// # Panics
    ///
    /// Panics when the slot is already occupied, which would mean two
    /// claims existed at once — the one invariant the claim word is
    /// there to keep.
    pub(super) fn return_pins(&self, pins: DisplayPins) {
        self.shared
            .returned
            .push(pins)
            .unwrap_or_else(|_| panic!("two display claims returned their frame buffers at once"));
    }
}

/// The right to queue work for one claim.
#[derive(Clone)]
pub struct DisplaySender {
    shared: Arc<DisplayShared>,
    generation: u64,
}

impl DisplaySender {
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Queue `request` on the control queue and await its reply.
    pub(crate) async fn control<T>(
        &self,
        request: ControlRequest,
        reply: oneshot::Receiver<Result<T, DisplayServiceError>>,
    ) -> Result<T, DisplayServiceError> {
        self.shared.control.send(request).await.map_err(closed)?;
        reply.await.map_err(|_| DisplayServiceError::Closed)?
    }

    /// Queue `request` on the cursor queue and await its reply.
    pub(crate) async fn cursor<T>(
        &self,
        request: CursorRequest,
        reply: oneshot::Receiver<Result<T, DisplayServiceError>>,
    ) -> Result<T, DisplayServiceError> {
        self.shared.cursor.send(request).await.map_err(closed)?;
        reply.await.map_err(|_| DisplayServiceError::Closed)?
    }

    /// Tell the owner task about a resource this claim no longer wants,
    /// without waiting for it to be gone.
    ///
    /// The display engine still has to be told, and telling it is the
    /// owner task's work; what the caller gets back is only that the
    /// message is queued.
    pub(crate) async fn control_oneway(
        &self,
        request: ControlRequest,
    ) -> Result<(), DisplayServiceError> {
        self.shared.control.send(request).await.map_err(closed)
    }
}

impl Drop for DisplayClaim {
    fn drop(&mut self) {
        // Not a message: a drop cannot await room in a queue, and the
        // instance this claim belonged to may already be dead. One
        // permit is enough, because there is exactly one claim to
        // release and the owner clears the word once it has.
        self.shared
            .claim
            .store(ClaimState::RELEASING, Ordering::Release);
        self.shared.release.notify_one();
    }
}

const fn closed(error: ProviderError) -> DisplayServiceError {
    match error {
        ProviderError::Unavailable | ProviderError::Closed | ProviderError::Full => {
            DisplayServiceError::Closed
        }
    }
}
