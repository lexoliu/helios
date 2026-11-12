//! The kernel's ownership of the display engine's rendering half.
//!
//! The renderer needs no follower task of its own: nothing about it
//! changes without being asked — a monitor announces itself, a fence
//! does not — because a fence is only ever the completion of a command
//! the submit server posted, and that server is already running when it
//! retires. What the engine does need owned is the record of what every
//! session has to give back: the contexts it opened, the blobs it
//! created, and which of those sit in the aperture.
//!
//! # SMP contract
//!
//! Two tasks, both local to the processor that brought the device up,
//! which is the processor the device's interrupt is routed to.
//!
//! * The control server owns everything that changes what the renderer
//!   holds, for every session: capability-set reads, contexts, blobs,
//!   aperture placements and attachments. It serves one request at a
//!   time, so the record of what each session made is never in two
//!   minds about what exists.
//! * The submit server owns the command streams. It is a separate task
//!   for the reason the queues are separate: a submission must not wait
//!   behind a capset read of tens of kilobytes. It posts one command at
//!   a time — the next is popped only once the last is on the device's
//!   ring — so the device sees every ring's commands in the order their
//!   fences were admitted, and drives up to [`SUBMITS_IN_FLIGHT`] of
//!   them through to their answers at once. Every request it pops
//!   counts against its session's in-flight tally until the device has
//!   answered it, and a session's release stays pending, without
//!   holding up the control server, until that tally reads zero.
//!
//! Both hold the same device handle. The trait's own contract says every
//! method takes `&self`, may be called from several tasks at once, and
//! that an implementation serialises access to its own rings.

use arrayvec::ArrayVec;
use core::future::Future;
use core::pin::pin;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::task::Poll;
use futures::future::{Either, poll_fn, select};
use helios_hal::cpu::Cpu;
use helios_hal::device::DeviceRegion;
use helios_hal::display::{
    BlobId, BlobMemory, BlobRequest, CapsetId, CapsetList, CommandStream, ContextId, ContextName,
    Gpu3d, Gpu3dError, RingCount, Submission,
};
use helios_hal::pmm::PhysFrameRange;
use helios_hal::watchdog::Watchdog;
use triomphe::Arc;

use crate::Kernel;
use crate::buffer::Buffer;
use crate::component::{ProviderReceiver, provider_channel};
use crate::exec::{InFlight, Notify, monotonic_nanos};

use super::service::{
    BlobShared, BlobSpec, ContextShared, ControlMessage, Gpu3dRequest, Gpu3dService, Gpu3dShared,
    InFlightSubmit, SessionState, SubmitCommands, SubmitMessage, SubmitRequest,
};
use super::{
    ContextRecord, Gpu3dServiceError, MAX_GPU_BLOBS, MAX_GPU_CONTEXTS, MAX_MACHINE_GPU_BLOBS,
    MAX_MACHINE_GPU_CONTEXTS,
};

/// Bytes of one capability-set read the kernel will carry.
///
/// The capset's size is the device's own figure, but the reply is a
/// kernel allocation against a host-provided length, so it is capped:
/// the largest capset a shipping renderer publishes is a few tens of
/// kilobytes, and a device that reports more than this gets its answer
/// truncated by the chain rather than by a budget the kernel chose.
const MAX_CAPSET_BYTES: usize = 1 << 20;

/// Submissions the submit server drives through to their answers at
/// once.
///
/// The device's control ring is 64 descriptors deep and a submission
/// takes three of them — the request, the command stream and the reply
/// — so twenty-one is the most the ring itself holds; sixteen leaves the
/// control server room for its own commands beside a full pipeline.
pub(super) const SUBMITS_IN_FLIGHT: usize = 16;

/// Brings the display engine's rendering half under kernel ownership
/// and publishes the service `helios:system/gpu` is served from.
///
/// The device is the same one the display path holds; what callers get
/// back is a handle to the tasks this spawns, which is what a session
/// and every submission after it travels through. A device that renders
/// nothing gets the service — so an `open` is answered `no-renderer`
/// rather than `unavailable` — but no tasks, because no session can
/// ever be opened on it.
pub fn install_gpu3d_device<CpuImpl, WatchdogImpl, Device>(
    kernel: &Kernel<CpuImpl, WatchdogImpl>,
    cpu: &CpuImpl,
    device: Device,
) -> Gpu3dService
where
    CpuImpl: Cpu + Clone + Send + Sync + 'static,
    WatchdogImpl: Watchdog + Clone,
    Device: Gpu3d + Clone,
{
    let (shared, control_rx, submit_rx) = gpu3d_channels(device.renders());

    if device.renders() {
        {
            let device = device.clone();
            let shared = shared.clone();
            kernel.spawn_local_detached(async move {
                serve_control(&device, &shared, &control_rx).await;
            });
        }
        {
            let device = device.clone();
            let cpu = cpu.clone();
            kernel.spawn_local_detached(async move {
                serve_submit(&device, &submit_rx, &cpu).await;
            });
        }
    }

    Gpu3dService::from_shared(shared)
}

/// The shared state and the two inboxes one engine's tasks are built
/// around.
///
/// Split out of [`install_gpu3d_device`] so a test can drive the same
/// server loops without a kernel to spawn them on.
pub(super) fn gpu3d_channels(
    renders: bool,
) -> (
    Arc<Gpu3dShared>,
    ProviderReceiver<ControlMessage>,
    ProviderReceiver<SubmitMessage>,
) {
    let (control, control_rx) = provider_channel(super::service::REQUEST_QUEUE_DEPTH);
    let (submit, submit_rx) = provider_channel(super::service::REQUEST_QUEUE_DEPTH);
    let shared = Arc::new(Gpu3dShared {
        control,
        submit,
        release: Arc::new(Notify::new()),
        sessions: AtomicU64::new(0),
        renders,
    });
    (shared, control_rx, submit_rx)
}

/// One context a session opened.
struct HeldContext {
    session: Arc<SessionState>,
    id: ContextId,
}

/// One blob a session created, and whether it sits in the aperture.
struct HeldBlob {
    session: Arc<SessionState>,
    id: BlobId,
    /// What the blob's handle and its scanouts share: while a scanout
    /// holds the blob, it is not destroyed.
    shared: Arc<BlobShared>,
    /// The blob's handle let it go while a scanout still showed it: it
    /// is destroyed once the last scanout lets go.
    dropped: bool,
    /// The blob is occupying the host-visible aperture, so handing it
    /// back takes an unmap before the destroy: a resource destroyed
    /// while it is placed would leave the engine decoding a span that
    /// no longer names anything.
    mapped: bool,
    /// The instance's buffer behind a guest-backed blob, held until the
    /// device has destroyed the blob: the run cannot go back to the pool
    /// while the renderer may still read it. [`release_blob`] drops it
    /// only on the device's agreement.
    backing: Option<Buffer>,
}

/// Everything every session has the renderer holding.
///
/// The owner task, not the store, is the record of what has to be given
/// back: a store that is killed mid-submission leaves nothing behind,
/// and its resources are still here to be released. One flat table per
/// kind, whose capacity is the machine-wide bound; the per-instance
/// bound is a count of one session's entries in it.
struct Held {
    contexts: ArrayVec<HeldContext, MAX_MACHINE_GPU_CONTEXTS>,
    blobs: ArrayVec<HeldBlob, MAX_MACHINE_GPU_BLOBS>,
}

impl Held {
    const fn new() -> Self {
        Self {
            contexts: ArrayVec::new_const(),
            blobs: ArrayVec::new_const(),
        }
    }

    fn holds_context(&self, session: &SessionState, context: ContextId) -> bool {
        self.contexts
            .iter()
            .any(|held| held.session.id == session.id && held.id == context)
    }

    fn blob_index(&self, session: &SessionState, blob: BlobId) -> Option<usize> {
        self.blobs
            .iter()
            .position(|held| held.session.id == session.id && held.id == blob)
    }

    /// Every session that has ended and still has something here.
    fn ended_sessions(
        &self,
    ) -> ArrayVec<Arc<SessionState>, { MAX_MACHINE_GPU_CONTEXTS + MAX_MACHINE_GPU_BLOBS }> {
        let mut ended = ArrayVec::new();
        for session in self
            .contexts
            .iter()
            .map(|held| &held.session)
            .chain(self.blobs.iter().map(|held| &held.session))
        {
            if session.has_ended()
                && !ended
                    .iter()
                    .any(|seen: &Arc<SessionState>| seen.id == session.id)
            {
                ended.push(session.clone());
            }
        }
        ended
    }

    /// The next blob of `session` nothing holds back any more.
    fn releasable_blob(&self, session: &SessionState) -> Option<usize> {
        self.blobs
            .iter()
            .position(|held| held.session.id == session.id && !held.shared.is_scanned_out())
    }
}

/// Serves everything that changes what the renderer holds.
pub(super) async fn serve_control<Device>(
    device: &Device,
    shared: &Gpu3dShared,
    inbox: &ProviderReceiver<ControlMessage>,
) where
    Device: Gpu3d,
{
    let mut held = Held::new();
    // The capability sets are a property of the host's build, so they
    // are read once and kept: a context create checks against them
    // without a second round trip, and a `capsets` request answers out
    // of the same list.
    let mut capsets: Option<CapsetList> = None;
    loop {
        // An end is a banked permit rather than a message, and it is
        // polled first: a session that has ended is swept up before
        // anything queued under it is served, and the check below drops
        // whatever was left in the queue. A release that something still
        // holds back — a submission the device has, a scanout of a blob
        // — is left pending, and the permit that thing banks when it
        // lets go brings the server back here.
        let released = pin!(shared.release.notified());
        let next = pin!(inbox.recv());
        match select(released, next).await {
            Either::Left(((), _)) => advance_releases(device, &mut held).await,
            Either::Right((Some(message), _)) => {
                if message.session.has_ended() {
                    // A request that outlived its session. Nothing is
                    // answered: the instance that asked let the engine
                    // go, and what the request would have made would
                    // belong to nobody.
                    continue;
                }
                serve_request(device, &mut held, &mut capsets, &shared.release, message).await;
            }
            Either::Right((None, _)) => return,
        }
    }
}

/// The device's capability sets, read once and kept.
///
/// The list is the host's, not the guest's — nothing the machine does
/// changes it — so the first successful read answers every later one.
/// A read the device fails is retried by the next request rather than
/// latched: a transient fault at bring-up must not answer "no
/// renderer" for the machine's whole run.
async fn known_capsets<Device: Gpu3d>(
    device: &Device,
    known: &mut Option<CapsetList>,
) -> Result<CapsetList, Gpu3dServiceError> {
    if let Some(capsets) = known {
        return Ok(capsets.clone());
    }
    let capsets = device.capsets().await.map_err(Gpu3dServiceError::from)?;
    *known = Some(capsets.clone());
    Ok(capsets)
}

async fn serve_request<Device>(
    device: &Device,
    held: &mut Held,
    capsets: &mut Option<CapsetList>,
    release: &Arc<Notify>,
    message: ControlMessage,
) where
    Device: Gpu3d,
{
    let ControlMessage { session, request } = message;
    match request {
        Gpu3dRequest::Capsets { reply } => {
            let _ = reply.send(known_capsets(device, capsets).await);
        }
        Gpu3dRequest::Capset { id, version, reply } => {
            let _ = reply.send(read_capset(device, capsets, id, version).await);
        }
        Gpu3dRequest::CreateContext {
            capset,
            rings,
            name,
            reply,
        } => {
            let _ = reply
                .send(create_context(device, held, capsets, &session, capset, rings, name).await);
        }
        Gpu3dRequest::DestroyContext { context } => {
            destroy_context(device, held, &session, context).await;
        }
        // A host call that stopped waiting closes its end before it
        // goes. One that went before its request was served is owed
        // nothing, and one that went while the device was working can
        // never learn what was made to undo it, so it is undone here.
        Gpu3dRequest::CreateBlob { spec, reply } => {
            if reply.is_canceled() {
                return;
            }
            let created = create_blob(device, held, &session, release, spec).await;
            if let Err(Ok(blob)) = reply.send(created) {
                destroy_blob(device, held, &session, blob.id()).await;
            }
        }
        Gpu3dRequest::DestroyBlob { blob } => {
            destroy_blob(device, held, &session, blob).await;
        }
        Gpu3dRequest::MapBlob { blob, reply } => {
            if reply.is_canceled() {
                return;
            }
            if let Err(Ok(_)) = reply.send(map_blob(device, held, &session, blob).await) {
                let _ = unmap_blob(device, held, &session, blob).await;
            }
        }
        Gpu3dRequest::UnmapBlob { blob, reply } => {
            let _ = reply.send(unmap_blob(device, held, &session, blob).await);
        }
        Gpu3dRequest::AttachResource {
            context,
            blob,
            reply,
        } => {
            let _ = reply.send(attach(device, held, &session, context, blob, true).await);
        }
        Gpu3dRequest::DetachResource {
            context,
            blob,
            reply,
        } => {
            let _ = reply.send(attach(device, held, &session, context, blob, false).await);
        }
    }
}

/// The bytes of one capability set at `version`.
///
/// A version past the newest the host speaks is a set the device does
/// not carry, refused here rather than sent: the host would answer it
/// with a set of some other version, or with nothing.
async fn read_capset<Device: Gpu3d>(
    device: &Device,
    known: &mut Option<CapsetList>,
    id: CapsetId,
    version: u32,
) -> Result<alloc::vec::Vec<u8>, Gpu3dServiceError> {
    let capsets = known_capsets(device, known).await?;
    let Some(info) = capsets.iter().find(|info| info.id == id) else {
        return Err(Gpu3dServiceError::NoSuchCapset);
    };
    if version > info.max_version {
        return Err(Gpu3dServiceError::NoSuchCapset);
    }
    let mut bytes = alloc::vec![0_u8; (info.max_size as usize).min(MAX_CAPSET_BYTES)];
    let written = device
        .capset(id, version, &mut bytes)
        .await
        .map_err(Gpu3dServiceError::from)?;
    bytes.truncate(written);
    Ok(bytes)
}

/// Open a renderer context of the type `capset` names, with `rings`
/// rings.
///
/// The capset is checked against the device's own list rather than left
/// to the device: an engine that negotiated no `CONTEXT_INIT` ignores
/// the field entirely, and a guest asking for venus on a virgl-only
/// host would otherwise be handed a context that speaks the wrong
/// renderer's protocol.
async fn create_context<Device: Gpu3d>(
    device: &Device,
    held: &mut Held,
    known: &mut Option<CapsetList>,
    session: &Arc<SessionState>,
    capset: CapsetId,
    rings: RingCount,
    name: ContextName,
) -> Result<ContextRecord, Gpu3dServiceError> {
    let own = held
        .contexts
        .iter()
        .filter(|held| held.session.id == session.id)
        .count();
    if own >= MAX_GPU_CONTEXTS {
        return Err(Gpu3dServiceError::TooManyContexts);
    }
    if held.contexts.is_full() {
        return Err(Gpu3dServiceError::ContextsExhausted);
    }
    if !known_capsets(device, known)
        .await?
        .iter()
        .any(|info| info.id == capset)
    {
        return Err(Gpu3dServiceError::UnsupportedContext);
    }
    let id = device
        .create_context(capset, rings, name)
        .await
        .map_err(Gpu3dServiceError::from)?;
    held.contexts.push(HeldContext {
        session: session.clone(),
        id,
    });
    Ok(ContextRecord {
        capset,
        shared: Arc::new(ContextShared::new(id, rings)),
    })
}

async fn destroy_context<Device: Gpu3d>(
    device: &Device,
    held: &mut Held,
    session: &SessionState,
    context: ContextId,
) {
    let Some(index) = held
        .contexts
        .iter()
        .position(|held| held.session.id == session.id && held.id == context)
    else {
        return;
    };
    held.contexts.remove(index);
    if let Err(error) = device.destroy_context(context).await {
        tracing::warn!(
            %error,
            "the display engine would not release a context its owner destroyed"
        );
    }
}

async fn create_blob<Device: Gpu3d>(
    device: &Device,
    held: &mut Held,
    session: &Arc<SessionState>,
    release: &Arc<Notify>,
    spec: BlobSpec,
) -> Result<Arc<BlobShared>, Gpu3dServiceError> {
    // A pure guest blob has no host allocation for a `blob_id` to name,
    // so one that names one is refused before the device is asked, the
    // way Linux's `verify_blob` (drivers/gpu/drm/virtio/virtgpu_ioctl.c)
    // refuses it.
    if spec.memory == BlobMemory::Guest && spec.blob_id != 0 {
        return Err(Gpu3dServiceError::InvalidBlob);
    }
    if !held.holds_context(session, spec.context) {
        // A blob names the renderer that owns it, and that renderer is
        // this session's context: one the session never opened — or
        // has already let go — is not a place a resource can live.
        return Err(Gpu3dServiceError::InvalidBlob);
    }
    let own = held
        .blobs
        .iter()
        .filter(|held| held.session.id == session.id)
        .count();
    if own >= MAX_GPU_BLOBS {
        return Err(Gpu3dServiceError::TooManyBlobs);
    }
    if held.blobs.is_full() {
        return Err(Gpu3dServiceError::BlobsExhausted);
    }
    let frames = spec.backing.as_ref().map(Buffer::frames);
    let pages: &[PhysFrameRange] = match &frames {
        Some(range) => core::slice::from_ref(range),
        None => &[],
    };
    let created = device
        .create_blob(BlobRequest {
            context: spec.context,
            memory: spec.memory,
            usage: spec.usage,
            size: spec.size,
            blob_id: spec.blob_id,
            backing: pages,
        })
        .await;
    let blob = match created {
        Ok(blob) => blob,
        Err(error) => {
            if let (Gpu3dError::Transport(_), Some(backing)) = (&error, spec.backing) {
                // No answer came, so the device may hold a blob nobody
                // has a name for, attached to these pages: the run is
                // kept out of the pool for good rather than handed to
                // the next instance.
                tracing::error!(
                    %error,
                    bytes = backing.bytes(),
                    "the display engine did not answer a blob create; its backing run is kept \
                     out of the pool for good, because the engine may be reading and writing it"
                );
                backing.leak();
            }
            return Err(Gpu3dServiceError::from(error));
        }
    };
    let shared = Arc::new(BlobShared::new(blob, release.clone()));
    held.blobs.push(HeldBlob {
        session: session.clone(),
        id: blob,
        shared: shared.clone(),
        dropped: false,
        mapped: false,
        backing: spec.backing,
    });
    Ok(shared)
}

/// Hand one blob back to the renderer, or, while a scanout shows it,
/// once the last scanout lets go.
///
/// A blob still sitting in the aperture is taken out of it first: the
/// device cannot forget a resource whose host storage a guest can still
/// reach, and the guest's own mapping of it is what the aperture
/// placement names.
async fn destroy_blob<Device: Gpu3d>(
    device: &Device,
    held: &mut Held,
    session: &SessionState,
    blob: BlobId,
) {
    let Some(index) = held.blob_index(session, blob) else {
        return;
    };
    if held.blobs[index].shared.is_scanned_out() {
        held.blobs[index].dropped = true;
        return;
    }
    let record = held.blobs.remove(index);
    release_blob(device, record).await;
}

/// Take one blob out of the aperture and destroy it, and let its backing
/// go only if the display engine agreed to both.
///
/// A blob the engine would not unmap or destroy is one it may still
/// read and write, so the run behind it never goes back to the pool: the
/// device is outside the kernel's trust, and a run handed to the next
/// instance while the renderer still writes it would put that instance's
/// memory under the renderer. A blob whose unmap failed is not destroyed
/// at all, because the driver refuses to destroy a placed blob. The
/// bytes are lost instead, and said so.
async fn release_blob<Device: Gpu3d>(device: &Device, record: HeldBlob) {
    let HeldBlob {
        id,
        mapped,
        backing,
        ..
    } = record;
    if mapped && let Err(error) = device.unmap_blob(id).await {
        keep_backing(
            "would not take a blob out of the aperture",
            &error,
            id,
            backing,
        );
        return;
    }
    if let Err(error) = device.destroy_blob(id).await {
        keep_backing("would not destroy a blob", &error, id, backing);
    }
}

/// Keep a blob's backing out of the pool for good.
fn keep_backing(refusal: &str, error: &Gpu3dError, blob: BlobId, backing: Option<Buffer>) {
    tracing::error!(
        %error,
        blob = blob.raw(),
        bytes = backing.as_ref().map_or(0, Buffer::bytes),
        "the display engine {refusal}; its backing run is kept out of the pool for good, \
         because the engine may still be reading and writing it"
    );
    if let Some(backing) = backing {
        backing.leak();
    }
}

/// Place `blob` in the host-visible aperture and report where, and
/// under which memory type.
async fn map_blob<Device: Gpu3d>(
    device: &Device,
    held: &mut Held,
    session: &SessionState,
    blob: BlobId,
) -> Result<DeviceRegion, Gpu3dServiceError> {
    let Some(index) = held.blob_index(session, blob) else {
        return Err(Gpu3dServiceError::InvalidBlob);
    };
    // Checked here as well as in the store: two `map-blob` calls on one
    // blob can both be in flight, and this record is the one place they
    // meet.
    if held.blobs[index].mapped {
        return Err(Gpu3dServiceError::AlreadyMapped);
    }
    let region = device
        .map_blob(blob)
        .await
        .map_err(Gpu3dServiceError::from)?;
    held.blobs[index].mapped = true;
    Ok(region)
}

/// Take `blob` back out of the aperture.
async fn unmap_blob<Device: Gpu3d>(
    device: &Device,
    held: &mut Held,
    session: &SessionState,
    blob: BlobId,
) -> Result<(), Gpu3dServiceError> {
    let Some(index) = held.blob_index(session, blob) else {
        return Err(Gpu3dServiceError::InvalidBlob);
    };
    if !held.blobs[index].mapped {
        return Err(Gpu3dServiceError::NotMapped);
    }
    device
        .unmap_blob(blob)
        .await
        .map_err(Gpu3dServiceError::from)?;
    held.blobs[index].mapped = false;
    Ok(())
}

/// `attach` selects which direction the resource moves.
async fn attach<Device: Gpu3d>(
    device: &Device,
    held: &mut Held,
    session: &SessionState,
    context: ContextId,
    blob: BlobId,
    attach: bool,
) -> Result<(), Gpu3dServiceError> {
    if !held.holds_context(session, context) || held.blob_index(session, blob).is_none() {
        return Err(Gpu3dServiceError::InvalidBlob);
    }
    let outcome = if attach {
        device.attach_resource(context, blob).await
    } else {
        device.detach_resource(context, blob).await
    };
    outcome.map_err(Gpu3dServiceError::from)
}

/// Hand everything every ended session held back to the machine, and
/// nothing any live session holds.
///
/// Also hands back every blob whose handle let it go while a scanout
/// showed it, once no scanout does.
///
/// Nothing here waits: a release something still holds back is left as
/// it is, and the permit that thing banks when it lets go runs this
/// again. Every other session's requests are served in between.
///
/// A session is not touched while the device still holds one of its
/// submissions: a submission the submit server popped before the end
/// was noticed can still be inside `device.submit`, parked for ring
/// room, and it publishes onto the control ring whenever the device
/// takes it — after `CTX_DESTROY` if nothing orders it. Every one of
/// those counts against its session's in-flight tally, and the drain to
/// zero of an ended session banks the permit that resumes it.
///
/// The order after that is what makes it safe to drop the records'
/// buffers: every mapped blob leaves the aperture, every blob is
/// destroyed — a blob a scanout still shows only once the scanout has
/// let go, which it does after blanking — and only then is a context
/// let go: a renderer still holding a context that names dead resources
/// would be reading guest pages on their way back to a pool.
async fn advance_releases<Device: Gpu3d>(device: &Device, held: &mut Held) {
    while let Some(index) = held
        .blobs
        .iter()
        .position(|blob| blob.dropped && !blob.session.has_ended() && !blob.shared.is_scanned_out())
    {
        release_blob(device, held.blobs.remove(index)).await;
    }
    for session in held.ended_sessions() {
        if session.is_draining() {
            continue;
        }
        while let Some(index) = held.releasable_blob(&session) {
            release_blob(device, held.blobs.remove(index)).await;
        }
        if held.blobs.iter().any(|blob| blob.session.id == session.id) {
            // A scanout still shows one of its blobs.
            continue;
        }
        let mut contexts = 0_usize;
        while let Some(index) = held
            .contexts
            .iter()
            .position(|held| held.session.id == session.id)
        {
            let context = held.contexts.remove(index);
            if let Err(error) = device.destroy_context(context.id).await {
                tracing::warn!(
                    %error,
                    "the display engine would not release a context of a session that ended"
                );
            }
            contexts += 1;
        }
        tracing::info!(session = session.id.raw(), contexts, "gpu session released");
    }
}

/// What the submit server learns when one posted submission finishes.
struct Finished {
    context: Arc<ContextShared>,
    commands: SubmitCommands,
    ring: helios_hal::display::RingIndex,
    fence: Option<helios_hal::display::FenceId>,
    /// Still here when the submission never reached the device's ring,
    /// so the refusal is the guest's answer.
    reply: Option<futures::channel::oneshot::Sender<Result<(), Gpu3dServiceError>>>,
    outcome: Result<(), Gpu3dError>,
}

/// What wakes the submit server.
#[expect(
    clippy::large_enum_variant,
    reason = "the event is a stack temporary the server matches at once; boxing the message \
              would allocate per submission to shrink a value that is never stored"
)]
enum SubmitEvent<Done> {
    Finished(Done),
    Next(SubmitMessage),
    Closed,
}

/// Posts the command streams every session's contexts submit.
///
/// One command is posted at a time: the next request is popped only
/// once the last one's chain is on the device's ring (or the driver
/// refused it before), so the device sees every ring's commands in queue
/// order, which is the order their fences were admitted in. Up to
/// [`SUBMITS_IN_FLIGHT`] posted submissions are driven through to the
/// device's answer at once.
///
/// A guest's `submit` resolves the moment its command is on the ring,
/// from the driver's `posted` signal; the fence retires on the
/// context's ring when the device answers. A refusal the device answers
/// after that point faults the context, and so does a submission the
/// store admitted that the driver could not post: either way the ring
/// has a point the device will never signal, so every ring of the
/// context is retired up to its newest fence and every later call on it
/// is refused.
///
/// Every request counts against its session's in-flight tally from the
/// pop until the device has answered it — including the ones the
/// session check drops, which were in flight like any other in the
/// window between the two. The request's own reference to the command
/// buffer is what keeps the pages from going back to the pool while the
/// device reads them; it is dropped once the device has answered, and
/// kept out of the pool for good when the device never did (a transport
/// error), because the chain may still be with it.
pub(super) async fn serve_submit<Device, CpuImpl>(
    device: &Device,
    inbox: &ProviderReceiver<SubmitMessage>,
    cpu: &CpuImpl,
) where
    Device: Gpu3d,
    CpuImpl: Cpu,
{
    // Set while the newest submission is not yet on the device's ring.
    // Raised by the server when it starts one, lowered by the driver's
    // `posted` signal or by the submission's end, whichever is first.
    let unposted = AtomicBool::new(false);
    let mut running = pin!(InFlight::<_, SUBMITS_IN_FLIGHT>::new());
    let mut open = true;
    loop {
        let event = {
            let mut next = pin!(inbox.recv());
            poll_fn(|cx| {
                if let Poll::Ready(Some(done)) = running.as_mut().poll_next(cx) {
                    return Poll::Ready(SubmitEvent::Finished(done));
                }
                // Checked after the set was polled: the poll is where the
                // driver says a chain is on the ring.
                if open && !unposted.load(Ordering::Acquire) && running.len() < SUBMITS_IN_FLIGHT {
                    return match next.as_mut().poll(cx) {
                        Poll::Ready(Some(message)) => Poll::Ready(SubmitEvent::Next(message)),
                        Poll::Ready(None) => Poll::Ready(SubmitEvent::Closed),
                        Poll::Pending => Poll::Pending,
                    };
                }
                Poll::Pending
            })
            .await
        };
        match event {
            SubmitEvent::Finished((in_flight, finished)) => {
                settle(finished, cpu);
                // The session's tally drops only once the answer has been
                // settled, so a release never runs between the two.
                drop(in_flight);
            }
            SubmitEvent::Closed => {
                open = false;
                if running.is_empty() {
                    return;
                }
            }
            SubmitEvent::Next(SubmitMessage { session, request }) => {
                // Counted from the pop, ahead of the session check: the
                // release path waits this count out before it lets a
                // context go, and a submission it could not see between
                // the two reads would be free to publish after
                // `CTX_DESTROY`.
                let in_flight = InFlightSubmit::begin(session.clone());
                if session.has_ended() {
                    // A submission that outlived its session is dropped
                    // rather than served, for the reason the control
                    // queue's are.
                    continue;
                }
                if request.context.is_faulted() {
                    refuse_faulted(request, cpu);
                    continue;
                }
                unposted.store(true, Ordering::Release);
                let unposted = &unposted;
                running.as_mut().push(async move {
                    let SubmitRequest {
                        context,
                        commands,
                        ring,
                        fence,
                        reply,
                    } = request;
                    let mut reply = Some(reply);
                    let stream = match &commands {
                        SubmitCommands::Pages { range, .. } => CommandStream::Pages(*range),
                        SubmitCommands::Inline(bytes) => CommandStream::Inline(bytes),
                    };
                    let mut posted = || {
                        unposted.store(false, Ordering::Release);
                        if let Some(reply) = reply.take() {
                            let _ = reply.send(Ok(()));
                        }
                    };
                    let outcome = device
                        .submit(
                            Submission {
                                context: context.id(),
                                commands: stream,
                                ring,
                                fence,
                            },
                            &mut posted,
                        )
                        .await;
                    unposted.store(false, Ordering::Release);
                    (
                        in_flight,
                        Finished {
                            context,
                            commands,
                            ring,
                            fence,
                            reply,
                            outcome,
                        },
                    )
                });
            }
        }
        if !open && running.is_empty() {
            return;
        }
    }
}

/// A submission for a context the device has already faulted: refused,
/// with its fence retired so no reader waits for it.
fn refuse_faulted<CpuImpl: Cpu>(request: SubmitRequest, cpu: &CpuImpl) {
    if let (Some(fence), Some(timeline)) = (request.fence, request.context.timeline(request.ring)) {
        timeline.retire(fence.raw(), monotonic_nanos(cpu));
    }
    let _ = request.reply.send(Err(Gpu3dServiceError::DeviceFault));
}

/// Settle one submission the device has answered, or refused before it
/// reached the ring.
fn settle<CpuImpl: Cpu>(finished: Finished, cpu: &CpuImpl) {
    let Finished {
        context,
        commands,
        ring,
        fence,
        reply,
        outcome,
    } = finished;
    let nanos = monotonic_nanos(cpu);
    let timeline = context
        .timeline(ring)
        .expect("the store admitted the submission against one of the context's rings");
    let error = match outcome {
        Ok(()) => {
            if let Some(fence) = fence {
                timeline.retire(fence.raw(), nanos);
            }
            return;
        }
        Err(error) => error,
    };
    if let (Gpu3dError::Transport(_), SubmitCommands::Pages { buffer, .. }) = (&error, commands) {
        // No answer came, so the chain may still be with the device
        // and may be read at any later time: the run is kept out of
        // the pool for good rather than handed to the next instance.
        tracing::error!(
            %error,
            bytes = buffer.bytes(),
            "the display engine did not answer a command submission; its command buffer is \
             kept out of the pool for good, because the engine may still be reading it"
        );
        buffer.leak();
    }
    tracing::error!(
        context = context.id().raw(),
        ring = ring.raw(),
        code = error.device_code(),
        posted = reply.is_none(),
        %error,
        "the display engine refused a command stream; the context is faulted"
    );
    if let Some(fence) = fence {
        timeline.retire(fence.raw(), nanos);
    }
    context.fault(nanos);
    if let Some(reply) = reply {
        let _ = reply.send(Err(Gpu3dServiceError::from(error)));
    }
}
