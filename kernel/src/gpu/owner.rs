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
//!   the two chains onto its one control ring itself.
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
    BlobId, BlobRequest, CapsetId, CapsetList, ContextId, ContextName, Gpu3d,
};
use helios_hal::pmm::PhysFrameRange;
use helios_hal::watchdog::Watchdog;
use triomphe::Arc;

use crate::Kernel;
use crate::component::{ProviderReceiver, provider_channel};
use crate::exec::monotonic_nanos;

use super::service::{ClaimState, Gpu3dRequest, Gpu3dService, Gpu3dShared, SubmitRequest};
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
        renders,
        returned: concurrent_queue::ConcurrentQueue::bounded(1),
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
                release_claim(device, &mut held).await;
                // Only now: the renderer has stopped reading, so
                // dropping the arena hands the pages back to a pool
                // nothing is decoding.
                while let Ok(pins) = shared.returned.pop() {
                    drop(pins);
                }
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
        Gpu3dRequest::CreateBlob { spec, reply, .. } => {
            let _ = reply.send(create_blob(device, held, spec).await);
        }
        Gpu3dRequest::DestroyBlob { blob, .. } => {
            destroy_blob(device, held, blob).await;
        }
        Gpu3dRequest::MapBlob { blob, reply, .. } => {
            let _ = reply.send(map_blob(device, held, blob).await);
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
    let pages: &[PhysFrameRange] = match &spec.backing {
        Some(range) => core::slice::from_ref(range),
        None => &[],
    };
    let blob = device
        .create_blob(BlobRequest {
            context: spec.context,
            memory: spec.memory,
            usage: spec.usage,
            size: spec.size,
            host_id: spec.host_id,
            backing: pages,
        })
        .await
        .map_err(Gpu3dServiceError::from)?;
    held.blobs.push(HeldBlob {
        id: blob,
        mapped: false,
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
    if record.mapped
        && let Err(error) = device.unmap_blob(blob).await
    {
        tracing::warn!(
            %error,
            "the display engine would not unmap a blob its owner destroyed"
        );
    }
    if let Err(error) = device.destroy_blob(blob).await {
        tracing::warn!(
            %error,
            "the display engine would not release a blob its owner destroyed"
        );
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
/// The order is what makes it safe to release the caller's pages
/// afterwards: every mapped blob leaves the aperture, every blob is
/// destroyed, and only then is a context let go — a renderer still
/// holding a context that names dead resources would be reading guest
/// pages on their way back to a pool.
async fn release_claim<Device: Gpu3d>(device: &Device, held: &mut ClaimResources) {
    for blob in &held.blobs {
        if blob.mapped
            && let Err(error) = device.unmap_blob(blob.id).await
        {
            tracing::warn!(
                %error,
                "the display engine would not unmap a blob of a claim that ended"
            );
        }
        if let Err(error) = device.destroy_blob(blob.id).await {
            tracing::warn!(
                %error,
                "the display engine would not release a blob of a claim that ended"
            );
        }
    }
    for context in &held.contexts {
        if let Err(error) = device.destroy_context(*context).await {
            tracing::warn!(
                %error,
                "the display engine would not release a context of a claim that ended"
            );
        }
    }
    tracing::info!(
        contexts = held.contexts.len(),
        blobs = held.blobs.len(),
        "3D claim released"
    );
    held.contexts.clear();
    held.blobs.clear();
}

/// Serves the command streams a claim's contexts submit.
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
        let SubmitRequest::Submit {
            generation,
            context,
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
        let outcome = device
            .submit(context, commands, fence)
            .await
            .map_err(Gpu3dServiceError::from);
        fences.publish(fence.raw(), monotonic_nanos(cpu));
        let _ = reply.send(outcome);
    }
}
