//! The handle the rest of the kernel reaches the 3D engine through.
//!
//! The display engine's rendering half is the same device its scanout
//! half is — a virtio-gpu behind whichever transport the platform
//! exposes it on — and the component host that serves
//! `helios:system/gpu` never names it. What crosses that boundary here
//! is a queue rather than a trait object, exactly as on the display
//! path: the device stays owned, whole, by the tasks in
//! [`super::owner`], and everything else asks those tasks for work by
//! sending a message and awaiting the reply the message carried along.
//! There is no vtable between a rendering plugin and the engine, and no
//! lock around the device.
//!
//! # Sessions
//!
//! The renderer multiplexes contexts by itself, so nothing here is
//! exclusive. An instance opens a [`Gpu3dSession`]; every message it
//! sends carries the session's record, and the owner tasks file what
//! the message made under it. A session ends when it is dropped — its
//! instance let its last `gpu` handle go, or died — and what it made,
//! and only that, is handed back.
//!
//! # Concurrency contract
//!
//! [`Gpu3dService`] is cloneable and every method on it may be called
//! from any processor. Opening a session is one counter increment; the
//! request queues are the kernel's bounded provider queues, whose
//! producers park on a permit-based notification rather than spinning.
//!
//! Two queues, because the work has two urgencies. Everything that
//! changes what the renderer holds — contexts, blobs, the mappings
//! between them — goes on the control queue, where a capset read of
//! tens of kilobytes may take the device a while to answer. Command
//! submissions go on the submit queue, so a submission never waits
//! behind one of those reads. The submit queue's order is the order the
//! device sees: the submit server posts one command at a time, popping
//! the next only once the last is on the device's ring, and a producer
//! checks a submission's fence against its ring and queues it in one
//! step of the task that owns its store.
//!
//! A context's rings are [`RingTimeline`]s shared between the store that
//! checks fences against them and the submit server that retires fences
//! on them; the server is the only one that publishes, so every ring's
//! stream is monotone.
//!
//! A session's end is not a message, for the reason the display's
//! release is not: the store that holds a session is dropped by
//! whatever kills its instance, and a drop cannot await a queue that is
//! full. The end is a flag on the session's record and one permit on a
//! [`Notify`] the control server races against its inbox. The submit
//! server may still be holding one of the session's submissions: a
//! request it already popped is in flight until the device answers it,
//! and the release does not start on that session until its count is
//! zero — a chain parked for ring room otherwise publishes after
//! `CTX_DESTROY`, naming a context the device has already forgotten.
//! The release is pending work rather than a wait: the control server
//! goes on serving every other session, and the drain to zero banks a
//! permit on the same [`Notify`] that has it resume.
//!
//! A blob a scanout shows is held by that scanout: the surface carries a
//! [`ScanoutHold`] on the blob's [`BlobShared`], and the blob is not
//! destroyed — whether its handle was dropped or its session ended —
//! until every hold is gone. The display path drops a hold only after it
//! has blanked the scanout, so the device never scans out a resource
//! that no longer exists; the hold's drop banks a permit that has the
//! control server finish the release.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use arrayvec::ArrayVec;
use futures::channel::oneshot;
use helios_hal::device::DeviceRegion;
use helios_hal::display::{
    BlobId, BlobMemory, BlobUsage, CapsetId, CapsetList, ContextId, ContextName, FenceId,
    RingCount, RingIndex,
};
use helios_hal::iommu::PhysicalRange;
use triomphe::Arc;

use crate::buffer::Buffer;
use crate::component::{ProviderError, ProviderSender};
use crate::display::SequenceSignal;
use crate::exec::Notify;
use crate::process::GpuOpenCap;

use super::{Gpu3dServiceError, MAX_EXECUTE_BYTES};

/// Requests the machine's sessions may have waiting on each queue
/// before the next one waits for room.
///
/// The same bound the display path sets, for the same reason: a plugin
/// that has queued this many submissions is one the renderer has not
/// kept up with, and making it wait is the backpressure a command path
/// needs.
pub const REQUEST_QUEUE_DEPTH: usize = crate::display::REQUEST_QUEUE_DEPTH;

/// Which session a request was made under.
///
/// Counted up from one and never reused, so a handle that outlived its
/// session can never name a later one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionId(u64);

impl SessionId {
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// What the owner tasks and the session's store share about one session.
pub(crate) struct SessionState {
    pub(super) id: SessionId,
    /// Set once, when the session is dropped. A request read after it is
    /// set is dropped unanswered: its instance let the engine go.
    pub(super) ended: AtomicBool,
    /// Submissions of this session the submit server has popped and the
    /// device has not finished with.
    ///
    /// Counted from the pop — before the session check — because a
    /// release noticed between the two reads would otherwise see the
    /// count still at zero and destroy a context underneath a chain the
    /// device is about to take.
    pub(super) submits_in_flight: AtomicU64,
    /// The control server's wake-up. A drain to zero of an ended
    /// session banks one permit on it, so the pending release resumes.
    pub(super) release: Arc<Notify>,
}

impl SessionState {
    pub(super) fn has_ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    /// Whether the device still holds a submission of this session.
    pub(super) fn is_draining(&self) -> bool {
        self.submits_in_flight.load(Ordering::Acquire) != 0
    }
}

/// What the owner task, the blob's handle and every scanout of it share
/// about one blob.
pub struct BlobShared {
    id: BlobId,
    /// Scanouts showing the blob right now.
    scanouts: AtomicU64,
    /// The control server's wake-up, banked when the last scanout lets
    /// go, so a release the scanout held back resumes.
    release: Arc<Notify>,
}

impl BlobShared {
    pub(crate) fn new(id: BlobId, release: Arc<Notify>) -> Self {
        Self {
            id,
            scanouts: AtomicU64::new(0),
            release,
        }
    }

    /// The virtio resource id the device knows the blob by.
    pub const fn id(&self) -> BlobId {
        self.id
    }

    /// Whether a scanout shows the blob.
    pub fn is_scanned_out(&self) -> bool {
        self.scanouts.load(Ordering::Acquire) != 0
    }
}

/// One scanout's hold on a blob: the blob is not destroyed while it
/// exists. Dropped by the display path once the scanout is blanked.
pub struct ScanoutHold {
    blob: Arc<BlobShared>,
}

impl ScanoutHold {
    /// Hold the blob for one scanout of it.
    ///
    /// Taken in the store step that checks the blob's session is open,
    /// so a release that starts afterwards sees the hold.
    pub fn new(blob: &Arc<BlobShared>) -> Self {
        blob.scanouts.fetch_add(1, Ordering::AcqRel);
        Self { blob: blob.clone() }
    }

    /// The blob held.
    pub fn blob(&self) -> BlobId {
        self.blob.id
    }
}

impl Drop for ScanoutHold {
    fn drop(&mut self) {
        if self.blob.scanouts.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.blob.release.notify_one();
        }
    }
}

/// One ring of one context: the fences the store has let onto it, and
/// the fences the device has retired on it.
///
/// `submitted` is written by the task that owns the context's store,
/// which checks every fence against it; `fences` is published by the
/// submit server alone, so the stream a guest reads only ever moves
/// forward.
pub struct RingTimeline {
    fences: SequenceSignal,
    submitted: AtomicU64,
}

impl RingTimeline {
    fn new() -> Self {
        Self {
            fences: SequenceSignal::new(),
            submitted: AtomicU64::new(0),
        }
    }

    /// Every fence retired on this ring, published as its raw value.
    pub fn signal(&self) -> &SequenceSignal {
        &self.fences
    }

    /// The newest fence the store has let onto this ring.
    pub fn submitted(&self) -> u64 {
        self.submitted.load(Ordering::Acquire)
    }

    /// Whether `fence` comes after every fence already let onto the
    /// ring.
    pub(crate) fn admits(&self, fence: FenceId) -> bool {
        fence.raw() > self.submitted()
    }

    /// Record that `fence` is on its way to the device. Called in the
    /// step that queued it, and only after [`Self::admits`] said yes.
    pub(crate) fn record_submitted(&self, fence: FenceId) {
        self.submitted.store(fence.raw(), Ordering::Release);
    }

    /// Retire `fence` on this ring, if it comes after what has already
    /// been retired: the stream answers for every fence before the
    /// newest, so it never moves back.
    pub(super) fn retire(&self, fence: u64, nanos: u64) {
        if fence > self.fences.sequence() {
            self.fences.publish(fence, nanos);
        }
    }
}

/// What the store, the submit server and a guest's fence streams share
/// about one context.
pub struct ContextShared {
    id: ContextId,
    rings: RingCount,
    timelines: Box<[RingTimeline]>,
    /// Set once the device refused one of this context's commands after
    /// its submission had resolved, or a submission the store had
    /// accepted could not be posted. Every later call on the context
    /// answers `device-fault`.
    faulted: AtomicBool,
}

impl ContextShared {
    pub(super) fn new(id: ContextId, rings: RingCount) -> Self {
        Self {
            id,
            rings,
            timelines: rings.rings().map(|_| RingTimeline::new()).collect(),
            faulted: AtomicBool::new(false),
        }
    }

    /// The device-side context identifier.
    pub const fn id(&self) -> ContextId {
        self.id
    }

    /// How many rings the context was created with.
    pub const fn rings(&self) -> RingCount {
        self.rings
    }

    /// One of the context's rings, or `None` for a ring it does not
    /// have.
    pub fn timeline(&self, ring: RingIndex) -> Option<&RingTimeline> {
        self.timelines.get(ring.index())
    }

    /// Whether the device has faulted this context.
    pub fn is_faulted(&self) -> bool {
        self.faulted.load(Ordering::Acquire)
    }

    /// Check a submission on `ring` carrying `fence` against this
    /// context: refused `device-fault` once the device has faulted it,
    /// `bad-ring` for a ring it was not created with, and `stale-fence`
    /// for a fence that does not come after every fence already let onto
    /// that ring. Each ring is its own timeline, so a fence is only ever
    /// stale against its own ring.
    ///
    /// Nothing is recorded: the caller records the fence on the ring it
    /// answers with ([`RingTimeline::record_submitted`]) in the step that
    /// queues the submission.
    pub(crate) fn admit(
        &self,
        ring: u8,
        fence: Option<u64>,
    ) -> Result<(RingIndex, Option<FenceId>, &RingTimeline), Gpu3dServiceError> {
        if self.is_faulted() {
            return Err(Gpu3dServiceError::DeviceFault);
        }
        let Some((ring, timeline)) = RingIndex::new(ring)
            .and_then(|ring| self.timeline(ring).map(|timeline| (ring, timeline)))
        else {
            return Err(Gpu3dServiceError::BadRing);
        };
        let fence = fence.map(FenceId::new);
        if let Some(fence) = fence
            && !timeline.admits(fence)
        {
            return Err(Gpu3dServiceError::StaleFence);
        }
        Ok((ring, fence, timeline))
    }

    /// Fault the context and retire every ring up to the newest fence
    /// let onto it.
    ///
    /// Retiring is the whole promise a faulted context still keeps: no
    /// reader of any of its rings waits for a fence that will now never
    /// be signalled. What the device made of the work before those
    /// points is undefined; the pages it read stay referenced by their
    /// requests until it answers each of them, so nothing it may still
    /// read goes back to a pool.
    pub(super) fn fault(&self, nanos: u64) {
        self.faulted.store(true, Ordering::Release);
        for timeline in &*self.timelines {
            timeline.retire(timeline.submitted(), nanos);
        }
    }
}

/// One renderer context a session opened, as the store records it.
pub struct ContextRecord {
    /// The renderer this context speaks to.
    pub capset: CapsetId,
    /// The context's identity, rings and fault state, shared with the
    /// submit server.
    pub shared: Arc<ContextShared>,
}

/// Work for the task that owns what the renderer holds.
pub(crate) enum Gpu3dRequest {
    /// Every capability set the device carries, in its own order.
    Capsets {
        reply: oneshot::Sender<Result<CapsetList, Gpu3dServiceError>>,
    },
    /// The bytes of one capability set at one version.
    Capset {
        id: CapsetId,
        version: u32,
        reply: oneshot::Sender<Result<Vec<u8>, Gpu3dServiceError>>,
    },
    /// Open a renderer context of the type `capset` names, with `rings`
    /// rings.
    CreateContext {
        capset: CapsetId,
        rings: RingCount,
        name: ContextName,
        reply: oneshot::Sender<Result<ContextRecord, Gpu3dServiceError>>,
    },
    /// Let a context go. No reply: the store drops the handle and the
    /// renderer is told by the task that owns it.
    DestroyContext { context: ContextId },
    /// Create a blob resource.
    CreateBlob {
        spec: BlobSpec,
        reply: oneshot::Sender<Result<Arc<BlobShared>, Gpu3dServiceError>>,
    },
    /// Let a blob go. A mapped blob is taken out of the aperture first;
    /// a blob a scanout shows goes once the last scanout lets it go.
    DestroyBlob { blob: BlobId },
    /// Place a blob's host storage in the engine's host-visible
    /// aperture and report where it landed, and how it is to be
    /// reached.
    MapBlob {
        blob: BlobId,
        reply: oneshot::Sender<Result<DeviceRegion, Gpu3dServiceError>>,
    },
    /// Take a blob back out of the aperture.
    UnmapBlob {
        blob: BlobId,
        reply: oneshot::Sender<Result<(), Gpu3dServiceError>>,
    },
    /// Make a blob reachable from a context's command stream.
    AttachResource {
        context: ContextId,
        blob: BlobId,
        reply: oneshot::Sender<Result<(), Gpu3dServiceError>>,
    },
    /// Take a blob back out of a context's command stream.
    DetachResource {
        context: ContextId,
        blob: BlobId,
        reply: oneshot::Sender<Result<(), Gpu3dServiceError>>,
    },
}

/// What one `CreateBlob` request describes.
///
/// `backing` carries the instance's own buffer for the guest-backed
/// kinds and is absent for a host-3D blob, whose storage is the
/// renderer's. The owner task's record of the blob keeps it for as long
/// as the device does.
pub(crate) struct BlobSpec {
    /// The renderer context the resource belongs to.
    pub(crate) context: ContextId,
    /// Where its storage lives.
    pub(crate) memory: BlobMemory,
    /// What it may be used for.
    pub(crate) usage: BlobUsage,
    /// Its size in bytes.
    pub(crate) size: u64,
    /// The `blob_id` the renderer names the host allocation by: zero
    /// for a guest blob, the renderer's own object id otherwise.
    pub(crate) blob_id: u64,
    /// The instance's own buffer, for the guest-backed kinds.
    pub(crate) backing: Option<Buffer>,
}

/// An inline command stream, carried in the request itself.
pub(crate) type InlineCommands = ArrayVec<u8, MAX_EXECUTE_BYTES>;

/// Where a submission's command stream is.
#[expect(
    clippy::large_enum_variant,
    reason = "the inline stream is carried by value so a short execbuffer costs no allocation; \
              the submit queue is bounded, so the size it adds per slot is a fixed cost"
)]
pub(crate) enum SubmitCommands {
    /// A run of a command buffer the instance pinned.
    Pages {
        /// The buffer the run lies in, held until the device has
        /// answered so its pages cannot go back to the pool while the
        /// device may still be reading them.
        buffer: Buffer,
        /// The run of `buffer` the device reads the stream out of.
        range: PhysicalRange,
    },
    /// A short stream the request carries.
    Inline(InlineCommands),
}

/// Work for the task that posts the command streams.
pub(crate) struct SubmitRequest {
    /// The context the stream is for, whose ring the fence retires on.
    pub(crate) context: Arc<ContextShared>,
    pub(crate) commands: SubmitCommands,
    pub(crate) ring: RingIndex,
    pub(crate) fence: Option<FenceId>,
    /// Answered the moment the command is on the device's ring, or with
    /// the refusal that kept it off.
    pub(crate) reply: oneshot::Sender<Result<(), Gpu3dServiceError>>,
}

/// A request on the control queue and the session it was made under.
pub(super) struct ControlMessage {
    pub(super) session: Arc<SessionState>,
    pub(super) request: Gpu3dRequest,
}

/// A request on the submit queue and the session it was made under.
pub(super) struct SubmitMessage {
    pub(super) session: Arc<SessionState>,
    pub(super) request: SubmitRequest,
}

/// What every session shares with the tasks that own the engine.
pub(super) struct Gpu3dShared {
    pub(super) control: ProviderSender<ControlMessage>,
    pub(super) submit: ProviderSender<SubmitMessage>,
    /// The control server's wake-up for pending releases: one permit per
    /// session that has ended, per ended session's drain to zero, and
    /// per blob whose last scanout let go.
    pub(super) release: Arc<Notify>,
    /// The last session id handed out.
    pub(super) sessions: AtomicU64,
    /// Whether the device renders at all. Read once at bring-up and
    /// never again: a renderer is a property of the host's build, not
    /// of anything the machine does while it runs.
    pub(super) renders: bool,
}

/// One submission the submit server has taken out of the queue and the
/// device has not finished with.
///
/// Held from the pop — before the session check — until the device has
/// answered, because the release path drains its session's count before
/// it destroys a context: a submission still parked for ring room
/// publishes onto the control ring whenever the device takes it, and a
/// context destroyed underneath one leaves the engine running a command
/// stream against a context it has already torn down.
pub(super) struct InFlightSubmit {
    session: Arc<SessionState>,
}

impl InFlightSubmit {
    /// Count one more of `session`'s submissions with the device.
    pub(super) fn begin(session: Arc<SessionState>) -> Self {
        session.submits_in_flight.fetch_add(1, Ordering::AcqRel);
        Self { session }
    }
}

impl Drop for InFlightSubmit {
    fn drop(&mut self) {
        if self
            .session
            .submits_in_flight
            .fetch_sub(1, Ordering::AcqRel)
            == 1
            && self.session.has_ended()
        {
            // Read after the count: a session that ends after this read
            // banks its own permit, so one of the two always wakes the
            // release.
            self.session.release.notify_one();
        }
    }
}

/// The machine's 3D engine, as everything outside the owner tasks sees
/// it.
#[derive(Clone)]
pub struct Gpu3dService {
    shared: Arc<Gpu3dShared>,
}

impl Gpu3dService {
    pub(super) const fn from_shared(shared: Arc<Gpu3dShared>) -> Self {
        Self { shared }
    }

    /// Whether this machine's display engine renders at all.
    pub fn renders(&self) -> bool {
        self.shared.renders
    }

    /// Open a session on the machine's 3D engine.
    ///
    /// Refused only when the device renders nothing: there is no
    /// renderer to open. Any number of sessions are open at once.
    ///
    /// `_cap` is the proof that the caller's authority names the GPU
    /// class; the check happened where the cap was derived.
    pub fn open(&self, _cap: GpuOpenCap) -> Result<Gpu3dSession, Gpu3dServiceError> {
        if !self.shared.renders {
            return Err(Gpu3dServiceError::NoRenderer);
        }
        let id = SessionId(self.shared.sessions.fetch_add(1, Ordering::Relaxed) + 1);
        Ok(Gpu3dSession {
            shared: self.shared.clone(),
            state: Arc::new(SessionState {
                id,
                ended: AtomicBool::new(false),
                submits_in_flight: AtomicU64::new(0),
                release: self.shared.release.clone(),
            }),
        })
    }
}

/// One instance's session on the machine's 3D engine.
///
/// Dropping it — or dying while holding it — marks the session ended
/// and wakes the owner task, which releases every context, blob and
/// mapping this session created and nothing any other session did.
pub struct Gpu3dSession {
    shared: Arc<Gpu3dShared>,
    state: Arc<SessionState>,
}

impl Gpu3dSession {
    pub fn id(&self) -> SessionId {
        self.state.id
    }

    /// A handle that can queue work for this session.
    ///
    /// Cheap and cloneable, so a host call that has to keep sending
    /// after it has given the store back carries one instead of
    /// borrowing the session. A sender that outlives its session sends
    /// requests the owner task drops.
    pub fn sender(&self) -> Gpu3dSender {
        Gpu3dSender {
            shared: self.shared.clone(),
            state: self.state.clone(),
        }
    }
}

impl Drop for Gpu3dSession {
    fn drop(&mut self) {
        // Not a message: a drop cannot await room in a queue, and the
        // instance this session belonged to may already be dead. The
        // flag is what every request of the session is checked against,
        // and the permit is what has the owner task sweep up after it.
        self.state.ended.store(true, Ordering::Release);
        self.shared.release.notify_one();
    }
}

/// The right to queue work for one session.
#[derive(Clone)]
pub struct Gpu3dSender {
    shared: Arc<Gpu3dShared>,
    state: Arc<SessionState>,
}

impl Gpu3dSender {
    pub fn session(&self) -> SessionId {
        self.state.id
    }

    fn message(&self, request: Gpu3dRequest) -> ControlMessage {
        ControlMessage {
            session: self.state.clone(),
            request,
        }
    }

    /// Queue `request` on the control queue and await its reply.
    pub(crate) async fn control<T>(
        &self,
        request: Gpu3dRequest,
        reply: oneshot::Receiver<Result<T, Gpu3dServiceError>>,
    ) -> Result<T, Gpu3dServiceError> {
        self.shared
            .control
            .send(self.message(request))
            .await
            .map_err(closed)?;
        reply.await.map_err(|_| Gpu3dServiceError::Closed)?
    }

    /// Tell the owner task about a resource this session no longer
    /// wants, without waiting for it to be gone.
    ///
    /// The display engine still has to be told, and telling it is the
    /// owner task's work; what the caller gets back is only that the
    /// message is queued.
    pub(crate) async fn control_oneway(
        &self,
        request: Gpu3dRequest,
    ) -> Result<(), Gpu3dServiceError> {
        self.shared
            .control
            .send(self.message(request))
            .await
            .map_err(closed)
    }

    /// Queue `request` on the submit queue if there is room, without
    /// waiting.
    ///
    /// A submission is checked against its ring and queued in one step,
    /// so the queue's order is the order its fences were admitted in.
    /// `Ok(false)` says the queue was full and `request` was dropped
    /// unsent; the caller waits in [`Self::submit_room`], then checks
    /// and builds it again.
    pub(crate) fn try_submit(&self, request: SubmitRequest) -> Result<bool, Gpu3dServiceError> {
        match self.shared.submit.try_send(SubmitMessage {
            session: self.state.clone(),
            request,
        }) {
            Ok(()) => Ok(true),
            Err(ProviderError::Full) => Ok(false),
            Err(error) => Err(closed(error)),
        }
    }

    /// Wait until the submit queue has room.
    pub(crate) async fn submit_room(&self) {
        self.shared.submit.room().await;
    }

    /// Hand a turn [`Self::submit_room`] gave this caller to the next
    /// producer parked there, when the caller then queued nothing.
    pub(crate) fn pass_submit_room(&self) {
        self.shared.submit.pass_room();
    }
}

const fn closed(error: ProviderError) -> Gpu3dServiceError {
    match error {
        ProviderError::Unavailable | ProviderError::Closed | ProviderError::Full => {
            Gpu3dServiceError::Closed
        }
    }
}
