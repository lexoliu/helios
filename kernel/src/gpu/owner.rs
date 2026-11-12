//! The kernel's ownership of the display engine's rendering half.
//!
//! The renderer needs no follower task of its own: nothing about it
//! changes without being asked — a monitor announces itself, a fence
//! does not — because a fence is only ever the completion of a command
//! the submit server sent, and that server is already running when it
//! retires. What the engine does need owned is the claim's record of
//! what has to be given back: the contexts it opened, the blobs it
//! created, and which of those sit in the aperture.
//!
//! # SMP contract
//!
//! Two tasks, both local to the processor that brought the device up,
//! which is the processor the device's interrupt is routed to.
//!
//! * The control server owns everything that changes what the renderer
//!   holds: capability-set reads, contexts, blobs, aperture placements
//!   and attachments. It serves one request at a time, so the record of
//!   a claim's resources is never in two minds about what exists.
//! * The submit server owns the command streams. It is a separate task
//!   for the reason the queues are separate: a submission must not wait
//!   behind a capset read of tens of kilobytes, and the driver serialises
//!   the two chains onto its one control ring itself. Every request it
//!   pops counts against the claim's in-flight tally until the device
//!   has answered it, and a release waits that tally out before it
//!   destroys anything — a chain still parked for ring room publishes
//!   after `CTX_DESTROY` otherwise, naming a context the device has
//!   already forgotten.
//!
//! Both hold the same device handle. The trait's own contract says every
//! method takes `&self`, may be called from several tasks at once, and
//! that an implementation serialises access to its own rings.

use arrayvec::ArrayVec;
use core::pin::pin;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use futures::future::{Either, select};
use helios_hal::cpu::Cpu;
use helios_hal::device::DeviceRegion;
use helios_hal::display::{
    BlobId, BlobRequest, CapsetId, CapsetList, ContextId, ContextName, Gpu3d, Gpu3dError,
};
use helios_hal::pmm::PhysFrameRange;
use helios_hal::watchdog::Watchdog;
use triomphe::Arc;

use crate::Kernel;
use crate::buffer::Buffer;
use crate::component::{ProviderReceiver, provider_channel};
use crate::exec::monotonic_nanos;

use super::service::{
    ClaimState, Gpu3dRequest, Gpu3dService, Gpu3dShared, InFlightSubmit, SubmitRequest,
};
use super::{ContextRecord, Gpu3dServiceError, MAX_GPU_BLOBS, MAX_GPU_CONTEXTS};

/// Bytes of one capability-set read the kernel will carry.
///
/// The capset's size is the device's own figure, but the reply is a
/// kernel allocation against a host-provided length, so it is capped:
/// the largest capset a shipping renderer publishes is a few tens of
/// kilobytes, and a device that reports more than this gets its answer
/// truncated by the chain rather than by a budget the kernel chose.
const MAX_CAPSET_BYTES: usize = 1 << 20;

/// Brings the display engine's rendering half under kernel ownership
/// and publishes the service `helios:system/gpu` is served from.
///
/// The device is the same one the display path holds; what callers get
/// back is a handle to the tasks this spawns, which is what a claim and
/// every submission after it travels through. A device that renders
/// nothing gets the service — so a claim is answered `no-renderer`
/// rather than `unavailable` — but no tasks, because no claim can ever
/// be taken out on it.
pub fn install_gpu3d_device<CpuImpl, WatchdogImpl, MeasuredBootImpl, Device>(
    kernel: &Kernel<CpuImpl, WatchdogImpl, MeasuredBootImpl>,
    cpu: &CpuImpl,
    device: Device,
) -> Gpu3dService
where
    CpuImpl: Cpu + Clone + Send + Sync + 'static,
    WatchdogImpl: Watchdog + Clone,
    MeasuredBootImpl: helios_hal::measured_boot::MeasuredBoot,
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
            let shared = shared.clone();
            let cpu = cpu.clone();
            kernel.spawn_local_detached(async move {
                serve_submit(&device, &shared, &submit_rx, &cpu).await;
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
    ProviderReceiver<Gpu3dRequest>,
    ProviderReceiver<SubmitRequest>,
) {
    let (control, control_rx) = provider_channel(super::service::REQUEST_QUEUE_DEPTH);
    let (submit, submit_rx) = provider_channel(super::service::REQUEST_QUEUE_DEPTH);
    let shared = Arc::new(Gpu3dShared {
        control,
        submit,
        claim: AtomicU8::new(ClaimState::FREE),
        generation: AtomicU64::new(0),
        release: crate::exec::Notify::new(),
        submits_in_flight: AtomicU64::new(0),
        submits_drained: crate::exec::ProgressSignal::new(),
        renders,
    });
    (shared, control_rx, submit_rx)
}

/// One blob the current claim created, and whether it sits in the
/// aperture.
struct HeldBlob {
    id: BlobId,
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

/// Everything the current claim has the renderer holding.
///
/// The owner task, not the claiming store, is the record of what has to
/// be given back: a store that is killed mid-submission leaves nothing
/// behind, and its resources are still here to be released.
struct ClaimResources {
    generation: u64,
    contexts: ArrayVec<ContextId, MAX_GPU_CONTEXTS>,
    blobs: ArrayVec<HeldBlob, MAX_GPU_BLOBS>,
}

impl ClaimResources {
    const fn new() -> Self {
        Self {
            generation: 0,
            contexts: ArrayVec::new_const(),
            blobs: ArrayVec::new_const(),
        }
    }
}

/// Serves everything that changes what the renderer holds.
pub(super) async fn serve_control<Device>(
    device: &Device,
    shared: &Gpu3dShared,
    inbox: &ProviderReceiver<Gpu3dRequest>,
) where
    Device: Gpu3d,
{
    let mut held = ClaimResources::new();
    // The capability sets are a property of the host's build, so they
    // are read once and kept: a context create checks against them
    // without a second round trip, and a `capsets` request answers out
    // of the same list.
    let mut capsets: Option<CapsetList> = None;
    loop {
        // The release is a banked permit rather than a message, and it
        // is polled first: a claim that has been let go is torn down
        // before anything queued under it is served, and the generation
        // check below drops whatever was left in the queue.
        let released = pin!(shared.release.notified());
        let next = pin!(inbox.recv());
        match select(released, next).await {
            Either::Left(((), _)) => {
                // The records' references to the blobs' buffers go as
                // the blobs do, after the renderer has let them go.
                release_claim(device, shared, &mut held).await;
                shared.claim.store(ClaimState::FREE, Ordering::Release);
            }
            Either::Right((Some(request), _)) => {
                serve_request(device, shared, &mut held, &mut capsets, request).await;
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
    shared: &Gpu3dShared,
    held: &mut ClaimResources,
    capsets: &mut Option<CapsetList>,
    request: Gpu3dRequest,
) where
    Device: Gpu3d,
{
    let generation = shared.generation.load(Ordering::Acquire);
    if request.generation() != generation {
        // A request that outlived its claim. Nothing is answered: the
        // instance that asked is gone, so the reply channel's other end
        // is gone with it, and serving the request against whoever
        // holds the engine now would let a dead plugin drive a live
        // one's renderer.
        return;
    }
    if held.generation != generation {
        held.generation = generation;
    }
    match request {
        Gpu3dRequest::Capsets { reply, .. } => {
            let _ = reply.send(known_capsets(device, capsets).await);
        }
        Gpu3dRequest::Capset { id, reply, .. } => {
            let _ = reply.send(read_capset(device, capsets, id).await);
        }
        Gpu3dRequest::CreateContext {
            capset,
            name,
            reply,
            ..
        } => {
            let _ = reply.send(create_context(device, held, capsets, capset, name).await);
        }
        Gpu3dRequest::DestroyContext { context, .. } => {
            destroy_context(device, held, context).await;
        }
        // A host call that stopped waiting closes its end before it
        // goes. One that went before its request was served is owed
        // nothing, and one that went while the device was working can
        // never learn what was made to undo it, so it is undone here.
        Gpu3dRequest::CreateBlob { spec, reply, .. } => {
            if reply.is_canceled() {
                return;
            }
            if let Err(Ok(blob)) = reply.send(create_blob(device, held, spec).await) {
                destroy_blob(device, held, blob).await;
            }
        }
        Gpu3dRequest::DestroyBlob { blob, .. } => {
            destroy_blob(device, held, blob).await;
        }
        Gpu3dRequest::MapBlob { blob, reply, .. } => {
            if reply.is_canceled() {
                return;
            }
            if let Err(Ok(_)) = reply.send(map_blob(device, held, blob).await) {
                let _ = unmap_blob(device, held, blob).await;
            }
        }
        Gpu3dRequest::UnmapBlob { blob, reply, .. } => {
            let _ = reply.send(unmap_blob(device, held, blob).await);
        }
        Gpu3dRequest::AttachResource {
            context,
            blob,
            reply,
            ..
        } => {
            let _ = reply.send(attach(device, held, context, blob, true).await);
        }
        Gpu3dRequest::DetachResource {
            context,
            blob,
            reply,
            ..
        } => {
            let _ = reply.send(attach(device, held, context, blob, false).await);
        }
    }
}

/// The bytes of one capability set, at the newest version the host
/// speaks.
async fn read_capset<Device: Gpu3d>(
    device: &Device,
    known: &mut Option<CapsetList>,
    id: CapsetId,
) -> Result<alloc::vec::Vec<u8>, Gpu3dServiceError> {
    let capsets = known_capsets(device, known).await?;
    let Some(info) = capsets.iter().find(|info| info.id == id) else {
        return Err(Gpu3dServiceError::NoSuchCapset);
    };
    let mut bytes = alloc::vec![0_u8; (info.max_size as usize).min(MAX_CAPSET_BYTES)];
    let written = device
        .capset(id, info.max_version, &mut bytes)
        .await
        .map_err(Gpu3dServiceError::from)?;
    bytes.truncate(written);
    Ok(bytes)
}

/// Open a renderer context of the type `capset` names.
///
/// The capset is checked against the device's own list rather than left
/// to the device: an engine that negotiated no `CONTEXT_INIT` ignores
/// the field entirely, and a guest asking for venus on a virgl-only
/// host would otherwise be handed a context that speaks the wrong
/// renderer's protocol.
async fn create_context<Device: Gpu3d>(
    device: &Device,
    held: &mut ClaimResources,
    known: &mut Option<CapsetList>,
    capset: CapsetId,
    name: ContextName,
) -> Result<ContextRecord, Gpu3dServiceError> {
    if held.contexts.is_full() {
        return Err(Gpu3dServiceError::TooManyContexts);
    }
    if !known_capsets(device, known)
        .await?
        .iter()
        .any(|info| info.id == capset)
    {
        return Err(Gpu3dServiceError::UnsupportedContext);
    }
    let id = device
        .create_context(capset, name)
        .await
        .map_err(Gpu3dServiceError::from)?;
    held.contexts.push(id);
    Ok(ContextRecord {
        id,
        capset,
        fences: Arc::new(crate::display::SequenceSignal::new()),
    })
}

async fn destroy_context<Device: Gpu3d>(
    device: &Device,
    held: &mut ClaimResources,
    context: ContextId,
) {
    let Some(index) = held.contexts.iter().position(|held| *held == context) else {
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
    held: &mut ClaimResources,
    spec: super::service::BlobSpec,
) -> Result<BlobId, Gpu3dServiceError> {
    if !held.contexts.contains(&spec.context) {
        // A blob names the renderer that owns it, and that renderer is
        // this claim's context: one the claim never opened — or has
        // already let go — is not a place a resource can live.
        return Err(Gpu3dServiceError::InvalidBlob);
    }
    if held.blobs.is_full() {
        return Err(Gpu3dServiceError::TooManyBlobs);
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
            host_id: spec.host_id,
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
    held.blobs.push(HeldBlob {
        id: blob,
        mapped: false,
        backing: spec.backing,
    });
    Ok(blob)
}

/// Hand one blob back to the renderer.
///
/// A blob still sitting in the aperture is taken out of it first: the
/// device cannot forget a resource whose host storage a guest can still
/// reach, and the guest's own mapping of it is what the aperture
/// placement names.
async fn destroy_blob<Device: Gpu3d>(device: &Device, held: &mut ClaimResources, blob: BlobId) {
    let Some(index) = held.blobs.iter().position(|held| held.id == blob) else {
        return;
    };
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

/// Place `blob` in the host-visible aperture and report where.
async fn map_blob<Device: Gpu3d>(
    device: &Device,
    held: &mut ClaimResources,
    blob: BlobId,
) -> Result<DeviceRegion, Gpu3dServiceError> {
    let Some(index) = held.blobs.iter().position(|held| held.id == blob) else {
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
    held: &mut ClaimResources,
    blob: BlobId,
) -> Result<(), Gpu3dServiceError> {
    let Some(index) = held.blobs.iter().position(|held| held.id == blob) else {
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
    held: &mut ClaimResources,
    context: ContextId,
    blob: BlobId,
    attach: bool,
) -> Result<(), Gpu3dServiceError> {
    if !held.contexts.contains(&context) || !held.blobs.iter().any(|held| held.id == blob) {
        return Err(Gpu3dServiceError::InvalidBlob);
    }
    let outcome = if attach {
        device.attach_resource(context, blob).await
    } else {
        device.detach_resource(context, blob).await
    };
    outcome.map_err(Gpu3dServiceError::from)
}

/// Hand everything the claim held back to the machine.
///
/// The wait first is what makes it safe to destroy anything at all: a
/// submission the submit server popped before the release was noticed
/// can still be inside `device.submit`, parked for ring room, and it
/// publishes onto the control ring whenever the device takes it —
/// after `CTX_DESTROY` if nothing orders it. Every one of those counts
/// against the shared in-flight tally, so nothing is handed back until
/// the tally reads zero; the mark is sampled before the count so a
/// drain between the two is not missed.
///
/// The order after that is what makes it safe to drop the records'
/// buffers: every mapped blob leaves the aperture, every blob is
/// destroyed, and only then is a context let go — a renderer still
/// holding a context that names dead resources would be reading guest
/// pages on their way back to a pool.
async fn release_claim<Device: Gpu3d>(
    device: &Device,
    shared: &Gpu3dShared,
    held: &mut ClaimResources,
) {
    let mut drained = shared.submits_drained.mark();
    while shared.submits_in_flight.load(Ordering::Acquire) != 0 {
        shared.submits_drained.changed(drained).await;
        drained = shared.submits_drained.mark();
    }
    let blobs = held.blobs.len();
    for blob in held.blobs.drain(..) {
        release_blob(device, blob).await;
    }
    for context in &held.contexts {
        if let Err(error) = device.destroy_context(*context).await {
            tracing::warn!(
                %error,
                "the display engine would not release a context of a claim that ended"
            );
        }
    }
    tracing::info!(contexts = held.contexts.len(), blobs, "3D claim released");
    held.contexts.clear();
}

/// Serves the command streams a claim's contexts submit.
///
/// Every request counts against the shared in-flight tally from the
/// pop until the device has answered it — including the ones the
/// generation check drops, which were in flight like any other in the
/// window between the two. The tally, not the check, is what a release
/// waits out: a chain parked for ring room publishes whenever the
/// device takes it, and destroying the context underneath it would
/// leave the engine running a command stream against a context it has
/// torn down. The request's own reference to the command buffer is what
/// keeps the pages from going back to the pool while the device reads
/// them; it is dropped once the device has answered, and kept out of the
/// pool for good when the device never did (a transport error), because
/// the chain may still be with it.
///
/// The fence a submission carries is published to the signal the
/// request brought with it the moment the device has taken the buffer,
/// whatever it answered: the completion is the signal, and a reader of
/// the context's `fences` stream must never wait for a point the device
/// has decided will not come.
pub(super) async fn serve_submit<Device, CpuImpl>(
    device: &Device,
    shared: &Gpu3dShared,
    inbox: &ProviderReceiver<SubmitRequest>,
    cpu: &CpuImpl,
) where
    Device: Gpu3d,
    CpuImpl: Cpu,
{
    while let Some(request) = inbox.recv().await {
        // Counted from the pop, ahead of the generation check: the
        // release path waits this count out before it lets a context
        // go, and a submission it could not see between the two reads
        // would be free to publish after `CTX_DESTROY`.
        let _in_flight = InFlightSubmit::begin(shared);
        let SubmitRequest::Submit {
            generation,
            context,
            buffer,
            commands,
            fence,
            fences,
            reply,
        } = request;
        if generation != shared.generation.load(Ordering::Acquire) {
            // A submission that outlived its claim is dropped rather
            // than served, for the reason the control queue's are.
            continue;
        }
        let outcome = device.submit(context, commands, fence).await;
        if let Err(error @ Gpu3dError::Transport(_)) = &outcome {
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
        fences.publish(fence.raw(), monotonic_nanos(cpu));
        let _ = reply.send(outcome.map_err(Gpu3dServiceError::from));
    }
}
