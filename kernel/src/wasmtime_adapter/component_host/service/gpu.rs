//! `helios:system/gpu` for the component host.
//!
//! This is the whole of what a rendering plugin sees of the machine's
//! 3D engine, and none of it interprets what the plugin sends. A
//! command buffer is a buffer pinned from the plugin's own pool and
//! shown at the span it named; `submit` names a run of it and the kernel
//! hands the device its physical address. A capability set is the
//! renderer's bytes, carried whole. A host-3D blob's storage is the
//! renderer's, shown at a span of the instance's linear memory through
//! the host-visible aperture.
//!
//! A call that waits on the owner task — `create-blob`, `map-blob` —
//! checks its span first and shows anything only in its last step, after
//! its last wait and in the same step that records the handle owning the
//! view, so a call the plugin stops waiting for shows nothing; what the
//! device made for it goes back through a [`PendingReply`].
//!
//! The session lives in the instance's store, so it is single-owned:
//! the resource handles a plugin holds carry the session id to check
//! against and nothing else. A handle that outlived its session names
//! resources the session's end already handed back, and is refused
//! `session-ended`.
//!
//! # Concurrency contract
//!
//! Every call here runs on the task that owns the store, so the session
//! and the view table need no lock, and every view is shown and ended by
//! that task alone. A submission's fence is checked against its ring
//! and the submission queued in one step of that task, so the submit
//! queue holds every ring's commands in fence order. What is shared is
//! the two queues into the owner tasks, which carry their own
//! synchronisation, the buffers, whose counts are atomic, and each
//! context's rings, whose fence streams the submit server publishes.

use alloc::string::String;
use alloc::vec::Vec;
use core::pin::Pin;
use core::task::{Context, Poll};

use futures::channel::oneshot;
use helios_hal::device::DeviceRegion;
use helios_hal::display::{
    BlobId, BlobMemory, BlobUsage, CapsetId, ContextId, ContextName, MAX_CONTEXT_NAME, RingCount,
    RingIndex,
};
use helios_hal::iommu::PhysicalRange;
use triomphe::Arc;
use wasmtime::StoreContextMut;
use wasmtime::component::{
    Access, Accessor, Destination, HasSelf, Linker, Resource, StreamProducer, StreamReader,
    StreamResult,
};

use crate::ComponentHostNetwork;
use crate::buffer::{Buffer, GuestMemory, Span, ViewError, ViewGroup, ViewId, ViewTable};
use crate::gpu::{
    BlobShared, BlobSpec, ContextShared, Gpu3dRequest, Gpu3dSender, Gpu3dServiceError,
    InlineCommands, MAX_EXECUTE_BYTES, SessionId, SubmitCommands, SubmitRequest,
};
use crate::wasmtime_adapter::bindings::gpu::bindings::helios::system::gpu as gpu_wit;

use super::super::{PendingReply, StoreData, guest_memory, view_trap};

/// Register `helios:system/gpu` in a linker.
///
/// Every program linker gets it, for the reason the display interface
/// does: linkage is cached per binary while authority belongs to the
/// instance. `open` checks the instance's GPU device class, and an
/// instance without it is told `unavailable`, which is what a machine
/// with no 3D service tells every program.
pub(in crate::wasmtime_adapter::component_host) fn add_gpu_to_linker<CpuImpl, Net, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, Net, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    gpu_wit::add_to_linker::<_, HasSelf<StoreData<CpuImpl, Net, HostFs>>>(linker, |state| state)
}

/// One of a plugin's `gpu` handles, as its store records it.
///
/// It carries the session id and nothing else: the session itself is
/// in the store's [`crate::Gpu3dOwnership`], which counts the handles
/// holding it and which the store's own drop ends.
pub struct GpuHandle {
    session: SessionId,
}

/// One renderer context, as its store records it.
pub struct ContextHandle {
    session: SessionId,
    /// The context's identity, rings and fault state, shared with the
    /// submit server that retires its fences.
    shared: Arc<ContextShared>,
}

/// One pinned command buffer, as its store records it.
///
/// Its view is in the session's set, so the session's end ends it; the
/// handle's own drop ends it only while the session it was made under
/// is open.
pub struct CommandBufferHandle {
    pub(crate) session: SessionId,
    pub(crate) buffer: Buffer,
    pub(crate) view: ViewId,
}

/// One blob resource, as its store records it.
///
/// Both views are in the session's set, for the reason a command
/// buffer's is.
pub struct BlobHandle {
    pub(crate) session: SessionId,
    pub(crate) id: BlobId,
    /// What the owner task and every scanout of the blob share: a
    /// surface made from the blob takes a hold on it, so the blob
    /// outlives this handle for as long as a scanout shows it.
    pub(crate) shared: Arc<BlobShared>,
    /// The view of the blob's own pages in the instance's memory, for
    /// the guest-backed kinds.
    pub(crate) backing: Option<ViewId>,
    /// The view of its aperture placement, once `map-blob` has put one
    /// there.
    pub(crate) mapped: Option<ViewId>,
}

fn to_wit_error(error: Gpu3dServiceError) -> gpu_wit::Error {
    match error {
        Gpu3dServiceError::Unavailable => gpu_wit::Error::Unavailable,
        Gpu3dServiceError::NoRenderer => gpu_wit::Error::NoRenderer,
        Gpu3dServiceError::SessionEnded => gpu_wit::Error::SessionEnded,
        Gpu3dServiceError::NoSuchCapset => gpu_wit::Error::NoSuchCapset,
        Gpu3dServiceError::UnsupportedContext => gpu_wit::Error::UnsupportedContext,
        Gpu3dServiceError::TooManyContexts => gpu_wit::Error::TooManyContexts,
        Gpu3dServiceError::TooManyBlobs => gpu_wit::Error::TooManyBlobs,
        Gpu3dServiceError::ContextsExhausted => gpu_wit::Error::ContextsExhausted,
        Gpu3dServiceError::BlobsExhausted => gpu_wit::Error::BlobsExhausted,
        Gpu3dServiceError::TooManyViews => gpu_wit::Error::TooManyViews,
        Gpu3dServiceError::InvalidBlob => gpu_wit::Error::InvalidBlob,
        Gpu3dServiceError::ApertureExhausted => gpu_wit::Error::ApertureExhausted,
        Gpu3dServiceError::OutOfBounds => gpu_wit::Error::OutOfBounds,
        Gpu3dServiceError::BadRing => gpu_wit::Error::BadRing,
        Gpu3dServiceError::StaleFence => gpu_wit::Error::StaleFence,
        Gpu3dServiceError::BadSpan(_) | Gpu3dServiceError::BlobSpanMismatch => {
            gpu_wit::Error::BadSpan
        }
        Gpu3dServiceError::AlreadyMapped => gpu_wit::Error::AlreadyMapped,
        Gpu3dServiceError::NotMapped => gpu_wit::Error::NotMapped,
        Gpu3dServiceError::OutOfMemory => gpu_wit::Error::OutOfMemory,
        // An owner that has stopped serving is a machine on its way
        // down. There is nothing a plugin can do about it that it would
        // not also do about a device fault.
        Gpu3dServiceError::DeviceFault | Gpu3dServiceError::Closed => gpu_wit::Error::DeviceFault,
    }
}

fn from_wit_blob_memory(memory: gpu_wit::BlobMemory) -> BlobMemory {
    match memory {
        gpu_wit::BlobMemory::Guest => BlobMemory::Guest,
        gpu_wit::BlobMemory::Host3d => BlobMemory::Host3d,
        gpu_wit::BlobMemory::Host3dGuest => BlobMemory::Host3dGuest,
    }
}

fn from_wit_blob_usage(usage: gpu_wit::BlobUsage) -> BlobUsage {
    let mut flags = BlobUsage::empty();
    if usage.contains(gpu_wit::BlobUsage::MAPPABLE) {
        flags |= BlobUsage::MAPPABLE;
    }
    if usage.contains(gpu_wit::BlobUsage::SHAREABLE) {
        flags |= BlobUsage::SHAREABLE;
    }
    if usage.contains(gpu_wit::BlobUsage::CROSS_DEVICE) {
        flags |= BlobUsage::CROSS_DEVICE;
    }
    flags
}

const fn span(at: gpu_wit::Span) -> Span {
    Span::new(at.offset, at.length)
}

/// The WIT string into the fixed-size label the device carries.
///
/// The device field holds 64 bytes; a name longer than that keeps its
/// head, which is what the renderer's debugging output shows — the
/// label is a debugging aid, and a truncated one beats a refused call.
fn to_context_name(name: &str) -> ContextName {
    let mut label = ContextName::new();
    for ch in name.chars() {
        if label.len() + ch.len_utf8() > MAX_CONTEXT_NAME {
            break;
        }
        label.push(ch);
    }
    label
}

impl<CpuImpl, Net, HostFs> StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    /// The queue into the 3D owner, for session `session` if it is the
    /// one open now.
    fn gpu_sender(&self, session: SessionId) -> Result<Gpu3dSender, Gpu3dServiceError> {
        Ok(self.device.gpu().session_of(session)?.sender())
    }

    /// The physical run `offset`/`length` name inside `commands`,
    /// checked against `session` — the live one's — and the buffer it
    /// lies in, for the request to hold while the device reads it.
    ///
    /// A buffer pinned under an ended session was shown in a set the
    /// end has already ended, so the refusal is the WIT's answer for an
    /// ended session, not a bounds one.
    pub(crate) fn command_range(
        &mut self,
        commands: &Resource<CommandBufferHandle>,
        offset: u64,
        length: u64,
        session: SessionId,
    ) -> wasmtime::Result<Result<SubmitCommands, gpu_wit::Error>> {
        let commands = self.table.get(commands)?;
        if commands.session != session {
            return Ok(Err(gpu_wit::Error::SessionEnded));
        }
        // The submission is a run of the buffer's own pages: the
        // physical run is what the device reads, and it has to lie
        // inside the run the buffer covers.
        let Some(end) = offset.checked_add(length) else {
            return Ok(Err(gpu_wit::Error::OutOfBounds));
        };
        if end > commands.buffer.bytes() {
            return Ok(Err(gpu_wit::Error::OutOfBounds));
        }
        let physical = commands.buffer.physical();
        Ok(Ok(SubmitCommands::Pages {
            buffer: commands.buffer.clone(),
            range: PhysicalRange::new(physical.start + offset, length),
        }))
    }

    /// End `view`, one of the session's set, if session `session` is
    /// still open.
    ///
    /// A handle that outlived its session names a view the end already
    /// ended with the rest of the set, and ending it again would be a
    /// view the table never made.
    fn end_session_view(&mut self, session: SessionId, view: ViewId) -> wasmtime::Result<()> {
        if self.device.gpu().is_current(session) {
            self.views.end(view).map_err(view_trap)?;
        }
        Ok(())
    }

    /// Show something in the set of session `session`, which is the
    /// last step of every call that shows a view.
    ///
    /// The session may have ended, or ended and been opened again,
    /// while the call was waiting on the owner task; a view in a later
    /// session's set would outlive the handle that names it.
    fn show_in_session(
        &mut self,
        session: SessionId,
        show: impl FnOnce(&mut ViewTable, ViewGroup) -> Result<ViewId, ViewError>,
    ) -> Result<ViewId, Gpu3dServiceError> {
        self.device.gpu().session_of(session)?;
        let group = self.device.gpu().room_for_view(&self.views)?;
        Ok(show(&mut self.views, group)?)
    }

    /// Check `at` of `memory` for an object of `bytes`, and the session's
    /// room for one more view: the early half of every call that shows
    /// a view, run before anything is pinned or the owner task is told
    /// anything, so a span the table would refuse costs nothing.
    fn check_session_span(
        &self,
        memory: GuestMemory,
        at: Span,
        bytes: u64,
    ) -> Result<(), Gpu3dServiceError> {
        self.device.gpu().room_for_view(&self.views)?;
        self.views.validate(memory, at, bytes)?;
        Ok(())
    }

    /// Check `at` as [`Self::check_session_span`] does, then pin a
    /// buffer of `bytes` for showing there.
    ///
    /// Called inside the host call, so the run is charged to the calling
    /// instance's pool. Nothing is shown.
    fn pin_session_buffer(
        &self,
        memory: GuestMemory,
        at: Span,
        bytes: u64,
    ) -> Result<Buffer, Gpu3dServiceError> {
        self.check_session_span(memory, at, bytes)?;
        Ok(Buffer::pinned(bytes)?)
    }
}

/// The calling instance's linear memory, read through `access`.
fn memory_of<U, CpuImpl, Net, HostFs>(
    access: &mut Access<'_, U, HasSelf<StoreData<CpuImpl, Net, HostFs>>>,
) -> Result<GuestMemory, Gpu3dServiceError>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let linear_memory = access.get().linear_memory;
    Ok(guest_memory(linear_memory, &*access)?)
}

/// The calling instance's memory in the last step of a call whose early
/// half already read it.
///
/// # Panics
///
/// Panics when the store has no unshared memory: the handle is recorded
/// once, when the instance is built, and the early half of the call
/// already found it.
fn checked_memory<U, CpuImpl, Net, HostFs>(
    access: &mut Access<'_, U, HasSelf<StoreData<CpuImpl, Net, HostFs>>>,
) -> GuestMemory
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    memory_of(access).expect("a store whose memory took the span in the call's first step has one")
}

/// Tell the owner task to let go of something this session made,
/// without the store and without waiting.
///
/// A resource drop and a host call the guest stopped waiting for are
/// both synchronous here and the engine is not, so the work is handed to
/// a task rather than awaited. It cannot be lost for good: if the task
/// cannot be spawned, or the instance dies before it runs, the
/// session's own end lets go of everything the session made.
fn spawn_gpu_cleanup<CpuImpl>(
    spawner: &crate::InstanceSpawner<CpuImpl>,
    sender: Gpu3dSender,
    request: Gpu3dRequest,
) where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
{
    let spawned = spawner.try_spawn_detached(async move {
        if let Err(error) = sender.control_oneway(request).await {
            tracing::warn!(
                target: "helios_kernel::gpu",
                %error,
                "a resource could not be handed back to the 3D owner"
            );
        }
    });
    if let Err(error) = spawned {
        // The task arena is full. Nothing is leaked — the session's own
        // end lets go of the same resource — but the engine holds it
        // until then, and that is worth saying.
        tracing::warn!(
            target: "helios_kernel::gpu",
            %error,
            "a resource stays with the 3D engine until the session ends"
        );
    }
}

/// An `UnmapBlob` nobody waits for the answer to.
fn unmap_request(blob: BlobId) -> Gpu3dRequest {
    let (reply, _) = oneshot::channel();
    Gpu3dRequest::UnmapBlob { blob, reply }
}

impl<CpuImpl, Net, HostFs> gpu_wit::Host for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn open(&mut self) -> wasmtime::Result<Result<Resource<GpuHandle>, gpu_wit::Error>> {
        let Some(service) = self.runtime_state.gpu3d_service() else {
            return Ok(Err(gpu_wit::Error::Unavailable));
        };
        // Without the GPU class the answer is the one a machine with no
        // 3D service gives, not `no-renderer`, which would say the
        // machine has a device that renders nothing.
        let Ok(cap) = self.process_authority().derive_gpu_open_cap() else {
            return Ok(Err(gpu_wit::Error::Unavailable));
        };
        let session = match self.device.open_gpu(&service, &mut self.views, cap) {
            Ok(session) => session,
            Err(error) => return Ok(Err(to_wit_error(error))),
        };
        let handle = self.table.push(GpuHandle { session })?;
        tracing::info!(
            target: "helios_kernel::gpu",
            session = session.raw(),
            "an instance holds the 3D engine"
        );
        Ok(Ok(handle))
    }
}

impl<CpuImpl, Net, HostFs> gpu_wit::HostGpu for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self, handle: Resource<GpuHandle>) -> wasmtime::Result<()> {
        let handle = self.table.delete(handle)?;
        // Dropping the last handle is a plugin saying it is done, and it
        // costs exactly what dying costs: every view of the session
        // ended, every context and blob it made released and every
        // aperture placement undone.
        self.device
            .gpu_mut()
            .close(handle.session, &mut self.views)
            .map_err(view_trap)
    }
}

impl<CpuImpl, Net, HostFs> gpu_wit::HostContext for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self, handle: Resource<ContextHandle>) -> wasmtime::Result<()> {
        let context = self.table.delete(handle)?;
        // The context is let go through the owner task, which destroys
        // it on the device; a context of a session that has ended has
        // already had that done for it.
        let Ok(sender) = self.gpu_sender(context.session) else {
            return Ok(());
        };
        spawn_gpu_cleanup(
            self.spawner(),
            sender,
            Gpu3dRequest::DestroyContext {
                context: context.shared.id(),
            },
        );
        Ok(())
    }
}

impl<CpuImpl, Net, HostFs> gpu_wit::HostCommandBuffer for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn drop(&mut self, handle: Resource<CommandBufferHandle>) -> wasmtime::Result<()> {
        let commands = self.table.delete(handle)?;
        // The span is the instance's ordinary memory again at once. The
        // run goes back to the pool when the last submission still
        // naming it has been answered, because each one holds it.
        self.end_session_view(commands.session, commands.view)
    }
}

impl<CpuImpl, Net, HostFs> gpu_wit::HostBlob for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn resource_id(&mut self, handle: Resource<BlobHandle>) -> wasmtime::Result<u32> {
        Ok(self.table.get(&handle)?.id.raw())
    }

    fn drop(&mut self, handle: Resource<BlobHandle>) -> wasmtime::Result<()> {
        let blob = self.table.delete(handle)?;
        // The instance's views end first — a span still showing the
        // aperture once the owner task takes the blob out of it would
        // name whatever the engine places there next — and then the
        // device is told through the owner task, which unmaps a
        // still-placed blob before it destroys it and drops its
        // reference to the blob's buffer after. A blob of a session that
        // has ended has already had all of that done for it.
        let Ok(sender) = self.gpu_sender(blob.session) else {
            return Ok(());
        };
        for view in [blob.mapped, blob.backing].into_iter().flatten() {
            self.views.end(view).map_err(view_trap)?;
        }
        spawn_gpu_cleanup(
            self.spawner(),
            sender,
            Gpu3dRequest::DestroyBlob { blob: blob.id },
        );
        Ok(())
    }
}

/// The session a gpu handle names, and the queue into the owner task.
///
/// Every call that reaches the display engine starts here, and the
/// store is given back before anything is awaited: an `Access` guard
/// held across a wait would hold the store for as long as the engine
/// takes.
fn session_of<U, CpuImpl, Net, HostFs>(
    mut access: Access<'_, U, HasSelf<StoreData<CpuImpl, Net, HostFs>>>,
    handle: &Resource<GpuHandle>,
) -> wasmtime::Result<Result<(SessionId, Gpu3dSender), gpu_wit::Error>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let data = access.get();
    let session = data.table.get(handle)?.session;
    Ok(data
        .gpu_sender(session)
        .map(|sender| (session, sender))
        .map_err(to_wit_error))
}

/// What a blob call needs from the store before it waits: the session,
/// the blob and the sender, checked together.
fn blob_session<U, CpuImpl, Net, HostFs>(
    mut access: Access<'_, U, HasSelf<StoreData<CpuImpl, Net, HostFs>>>,
    handle: &Resource<BlobHandle>,
) -> wasmtime::Result<Result<(SessionId, BlobId, Gpu3dSender), gpu_wit::Error>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let data = access.get();
    let blob = data.table.get(handle)?;
    let (session, id) = (blob.session, blob.id);
    Ok(data
        .gpu_sender(session)
        .map(|sender| (session, id, sender))
        .map_err(to_wit_error))
}

/// What a context call needs from the store before it waits: the
/// session, the context and the sender, checked together, and refused
/// `device-fault` once the device has faulted the context.
fn context_session<U, CpuImpl, Net, HostFs>(
    mut access: Access<'_, U, HasSelf<StoreData<CpuImpl, Net, HostFs>>>,
    handle: &Resource<ContextHandle>,
) -> wasmtime::Result<Result<(SessionId, ContextId, Gpu3dSender), gpu_wit::Error>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let data = access.get();
    let context = data.table.get(handle)?;
    let (session, id) = (context.session, context.shared.id());
    if context.shared.is_faulted() {
        return Ok(Err(gpu_wit::Error::DeviceFault));
    }
    Ok(data
        .gpu_sender(session)
        .map(|sender| (session, id, sender))
        .map_err(to_wit_error))
}

/// One attempt at queueing a submission, made in one step of the task
/// that owns the store.
enum SubmitStep {
    /// The submission is queued; this is its answer.
    Queued(oneshot::Receiver<Result<(), Gpu3dServiceError>>),
    /// The submit queue was full; nothing was queued or admitted.
    Full(Gpu3dSender),
    /// Refused before anything was queued.
    Refused(gpu_wit::Error),
}

/// Check one submission against its context and ring, and queue it, in
/// one step; wait for room and try again while the queue is full.
///
/// The fence is checked against everything already let onto its ring
/// and recorded on it only once the submission is in the queue, in the
/// same step, so the queue's order is the order fences were admitted
/// in, and a submission that waited for room is checked again against
/// whatever was admitted while it waited. `commands` builds the stream
/// afresh for each attempt, because a full queue drops the attempt
/// unsent.
async fn queue_submission<U, CpuImpl, Net, HostFs>(
    accessor: &Accessor<U, HasSelf<StoreData<CpuImpl, Net, HostFs>>>,
    handle: &Resource<ContextHandle>,
    ring: u8,
    fence: Option<u64>,
    mut commands: impl FnMut(
        &mut StoreData<CpuImpl, Net, HostFs>,
        SessionId,
    ) -> wasmtime::Result<Result<SubmitCommands, gpu_wit::Error>>,
) -> wasmtime::Result<Result<(), gpu_wit::Error>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    // The sender whose room this call was last woken for: a call that
    // then queues nothing passes its turn on.
    let mut woken: Option<Gpu3dSender> = None;
    loop {
        let step = accessor.with(|mut access| -> wasmtime::Result<SubmitStep> {
            let data = access.get();
            let context = data.table.get(handle)?;
            let (session, shared) = (context.session, context.shared.clone());
            let sender = match data.gpu_sender(session) {
                Ok(sender) => sender,
                Err(error) => return Ok(SubmitStep::Refused(to_wit_error(error))),
            };
            let (ring, fence, timeline) = match shared.admit(ring, fence) {
                Ok(admitted) => admitted,
                Err(error) => return Ok(SubmitStep::Refused(to_wit_error(error))),
            };
            let commands = match commands(data, session)? {
                Ok(commands) => commands,
                Err(error) => return Ok(SubmitStep::Refused(error)),
            };
            let (reply, answer) = oneshot::channel();
            let request = SubmitRequest {
                context: shared.clone(),
                commands,
                ring,
                fence,
                reply,
            };
            Ok(match sender.try_submit(request) {
                Ok(true) => {
                    if let Some(fence) = fence {
                        timeline.record_submitted(fence);
                    }
                    SubmitStep::Queued(answer)
                }
                Ok(false) => SubmitStep::Full(sender),
                Err(error) => SubmitStep::Refused(to_wit_error(error)),
            })
        })?;
        match step {
            // A reply dropped unanswered is the submit server dropping a
            // request whose session ended while it was queued.
            SubmitStep::Queued(answer) => {
                return Ok(answer
                    .await
                    .unwrap_or(Err(Gpu3dServiceError::SessionEnded))
                    .map_err(to_wit_error));
            }
            SubmitStep::Full(sender) => {
                sender.submit_room().await;
                woken = Some(sender);
            }
            SubmitStep::Refused(error) => {
                if let Some(sender) = woken {
                    sender.pass_submit_room();
                }
                return Ok(Err(error));
            }
        }
    }
}

/// The stream a plugin reads one ring's fences from.
struct FenceStreamProducer {
    context: Arc<ContextShared>,
    ring: RingIndex,
    waiter: Option<crate::exec::NotifyWaiter>,
    last_seen: u64,
}

impl Unpin for FenceStreamProducer {}

impl<T: 'static> StreamProducer<T> for FenceStreamProducer {
    type Item = u64;
    type Buffer = Option<Self::Item>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _store: StoreContextMut<'_, T>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }
        let producer = &mut *self;
        let signal = producer
            .context
            .timeline(producer.ring)
            .expect("the stream was opened on one of the context's rings")
            .signal();
        let waiter = producer.waiter.get_or_insert_with(|| signal.waiter());
        match signal.poll_next(cx, waiter, &mut producer.last_seen) {
            Poll::Ready((fence, _nanos)) => {
                destination.set_buffer(Some(fence));
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<CpuImpl, Net, HostFs, U> gpu_wit::HostGpuWithStore<U>
    for HasSelf<StoreData<CpuImpl, Net, HostFs>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    async fn capsets(
        accessor: &Accessor<U, Self>,
        handle: Resource<GpuHandle>,
    ) -> wasmtime::Result<Result<Vec<gpu_wit::CapsetInfo>, gpu_wit::Error>> {
        let sender = match accessor.with(|access| session_of(access, &handle))? {
            Ok((_, sender)) => sender,
            Err(error) => return Ok(Err(error)),
        };
        let (reply, answer) = oneshot::channel();
        let outcome = sender
            .control(Gpu3dRequest::Capsets { reply }, answer)
            .await;
        Ok(outcome
            .map(|capsets| {
                capsets
                    .iter()
                    .map(|info| gpu_wit::CapsetInfo {
                        id: info.id.raw(),
                        max_version: info.max_version,
                        max_size: info.max_size,
                    })
                    .collect()
            })
            .map_err(to_wit_error))
    }

    async fn capset(
        accessor: &Accessor<U, Self>,
        handle: Resource<GpuHandle>,
        id: u32,
        version: u32,
    ) -> wasmtime::Result<Result<Vec<u8>, gpu_wit::Error>> {
        let sender = match accessor.with(|access| session_of(access, &handle))? {
            Ok((_, sender)) => sender,
            Err(error) => return Ok(Err(error)),
        };
        let (reply, answer) = oneshot::channel();
        let outcome = sender
            .control(
                Gpu3dRequest::Capset {
                    id: CapsetId::new(id),
                    version,
                    reply,
                },
                answer,
            )
            .await;
        Ok(outcome.map_err(to_wit_error))
    }

    async fn create_context(
        accessor: &Accessor<U, Self>,
        handle: Resource<GpuHandle>,
        capset: u32,
        rings: u8,
        name: String,
    ) -> wasmtime::Result<Result<Resource<ContextHandle>, gpu_wit::Error>> {
        let (session, sender) = match accessor.with(|access| session_of(access, &handle))? {
            Ok(pair) => pair,
            Err(error) => return Ok(Err(error)),
        };
        let Some(rings) = RingCount::new(rings) else {
            return Ok(Err(gpu_wit::Error::BadRing));
        };
        let (reply, answer) = oneshot::channel();
        let record = sender
            .control(
                Gpu3dRequest::CreateContext {
                    capset: CapsetId::new(capset),
                    rings,
                    name: to_context_name(&name),
                    reply,
                },
                answer,
            )
            .await;
        let record = match record {
            Ok(record) => record,
            Err(error) => return Ok(Err(to_wit_error(error))),
        };
        let context = accessor.with(|mut access| {
            access.get().table.push(ContextHandle {
                session,
                shared: record.shared,
            })
        })?;
        Ok(Ok(context))
    }

    async fn create_blob(
        accessor: &Accessor<U, Self>,
        handle: Resource<GpuHandle>,
        context: Resource<ContextHandle>,
        memory: gpu_wit::BlobMemory,
        usage: gpu_wit::BlobUsage,
        size: u64,
        blob_id: u64,
        buffer: Option<gpu_wit::Span>,
    ) -> wasmtime::Result<Result<Resource<BlobHandle>, gpu_wit::Error>> {
        let (session, sender) = match accessor.with(|access| session_of(access, &handle))? {
            Ok(pair) => pair,
            Err(error) => return Ok(Err(error)),
        };
        let (context_id, spawner) = accessor.with(
            |mut access| -> wasmtime::Result<(ContextId, crate::InstanceSpawner<CpuImpl>)> {
                let data = access.get();
                Ok((
                    data.table.get(&context)?.shared.id(),
                    data.spawner().clone(),
                ))
            },
        )?;
        let memory_kind = from_wit_blob_memory(memory);
        // A guest-backed blob's pages are the instance's own, pinned
        // before the device is asked: the resource the device creates
        // names their physical run directly. The span is checked first,
        // so a span the table would refuse costs neither a run nor a
        // request, and the pages are shown only in the call's last step,
        // after the device has answered. A host-3D blob has no pages to
        // pin and takes no span.
        let backing = match (memory_kind, buffer) {
            (BlobMemory::Host3d, None) => None,
            (BlobMemory::Guest | BlobMemory::Host3dGuest, Some(at)) => {
                if size == 0 {
                    return Ok(Err(gpu_wit::Error::InvalidBlob));
                }
                let at = span(at);
                let pinned = accessor.with(|mut access| {
                    let memory = memory_of(&mut access)?;
                    access.get().pin_session_buffer(memory, at, size)
                });
                match pinned {
                    Ok(buffer) => Some((at, buffer)),
                    Err(error) => return Ok(Err(to_wit_error(error))),
                }
            }
            (BlobMemory::Host3d, Some(_)) | (BlobMemory::Guest | BlobMemory::Host3dGuest, None) => {
                return Ok(Err(to_wit_error(Gpu3dServiceError::BlobSpanMismatch)));
            }
        };
        let (reply, answer) = oneshot::channel();
        // A blob the device makes for a call that is dropped before its
        // last step goes back the way a dropped blob handle's does.
        let cleanup = sender.clone();
        let mut pending = PendingReply::new(answer, move |blob: Arc<BlobShared>| {
            spawn_gpu_cleanup(
                &spawner,
                cleanup,
                Gpu3dRequest::DestroyBlob { blob: blob.id() },
            );
        });
        let queued = sender
            .control_oneway(Gpu3dRequest::CreateBlob {
                spec: BlobSpec {
                    context: context_id,
                    memory: memory_kind,
                    usage: from_wit_blob_usage(usage),
                    size,
                    blob_id,
                    backing: backing.as_ref().map(|(_, buffer)| buffer.clone()),
                },
                reply,
            })
            .await;
        let id = match queued {
            Ok(()) => pending.reply(Gpu3dServiceError::Closed).await,
            Err(error) => Err(error),
        };
        let shared = match id {
            Ok(shared) => shared,
            // Nothing of the device's names these pages and nothing
            // showed them, so the run goes back with this call's
            // reference.
            Err(error) => return Ok(Err(to_wit_error(error))),
        };

        // The last step, and the only one that shows anything: the view
        // of the blob's pages and the handle that owns it are recorded
        // together, with no wait in between that a cancellation could
        // land in.
        let recorded = accessor.with(|mut access| {
            let shown = match &backing {
                None => Ok(None),
                Some((at, buffer)) => {
                    let memory = checked_memory(&mut access);
                    access
                        .get()
                        .show_in_session(session, |views, group| {
                            views.show_buffer(memory, *at, buffer, Some(group))
                        })
                        .map(Some)
                }
            };
            let backing_view = match shown {
                Ok(view) => view,
                Err(error) => return Ok(Err(error)),
            };
            let resource = access.get().table.push(BlobHandle {
                session,
                id: shared.id(),
                shared: shared.clone(),
                backing: backing_view,
                mapped: None,
            })?;
            Ok::<_, wasmtime::Error>(Ok(resource))
        })?;
        match recorded {
            Ok(resource) => {
                pending.keep();
                Ok(Ok(resource))
            }
            Err(error) => {
                // The span was taken while the device was creating the
                // blob, or the session ended. The blob goes back before
                // the refusal does; the pending reply stays armed across
                // the wait, so a call dropped in it still hands the blob
                // back, and a send that completed is never repeated.
                if let Err(closed) = sender
                    .control_oneway(Gpu3dRequest::DestroyBlob { blob: shared.id() })
                    .await
                {
                    tracing::warn!(
                        target: "helios_kernel::gpu",
                        %closed,
                        "a blob whose span was taken could not be handed back to the 3D owner"
                    );
                }
                pending.keep();
                Ok(Err(to_wit_error(error)))
            }
        }
    }
}

impl<CpuImpl, Net, HostFs, U> gpu_wit::HostContextWithStore<U>
    for HasSelf<StoreData<CpuImpl, Net, HostFs>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    async fn commands(
        accessor: &Accessor<U, Self>,
        handle: Resource<ContextHandle>,
        buffer: gpu_wit::Span,
    ) -> wasmtime::Result<Result<Resource<CommandBufferHandle>, gpu_wit::Error>> {
        let session = match accessor.with(|access| context_session(access, &handle))? {
            Ok((session, _, _)) => session,
            Err(error) => return Ok(Err(error)),
        };
        // A command buffer is as long as the span it is shown at: the
        // span is the only size the call carries.
        let at = span(buffer);
        // Nothing is awaited here, so the buffer is pinned, shown and
        // recorded in the handle that owns the view in one step.
        accessor.with(|mut access| {
            let shown = memory_of(&mut access).and_then(|memory| {
                let data = access.get();
                let buffer = data.pin_session_buffer(memory, at, at.length)?;
                let view = data.show_in_session(session, |views, group| {
                    views.show_buffer(memory, at, &buffer, Some(group))
                })?;
                Ok((buffer, view))
            });
            let (buffer, view) = match shown {
                Ok(shown) => shown,
                Err(error) => return Ok(Err(to_wit_error(error))),
            };
            let resource = access.get().table.push(CommandBufferHandle {
                session,
                buffer,
                view,
            })?;
            Ok(Ok(resource))
        })
    }

    async fn submit(
        accessor: &Accessor<U, Self>,
        handle: Resource<ContextHandle>,
        commands: Resource<CommandBufferHandle>,
        offset: u64,
        length: u64,
        ring: u8,
        fence: Option<u64>,
    ) -> wasmtime::Result<Result<(), gpu_wit::Error>> {
        queue_submission(accessor, &handle, ring, fence, |data, session| {
            data.command_range(&commands, offset, length, session)
        })
        .await
    }

    async fn execute(
        accessor: &Accessor<U, Self>,
        handle: Resource<ContextHandle>,
        commands: Vec<u8>,
        ring: u8,
        fence: Option<u64>,
    ) -> wasmtime::Result<Result<(), gpu_wit::Error>> {
        let Ok(inline) = InlineCommands::try_from(commands.as_slice()) else {
            // Longer than the kernel carries inline: such a stream
            // belongs in a command buffer.
            debug_assert!(commands.len() > MAX_EXECUTE_BYTES);
            return Ok(Err(gpu_wit::Error::OutOfBounds));
        };
        queue_submission(accessor, &handle, ring, fence, |_, _| {
            Ok(Ok(SubmitCommands::Inline(inline.clone())))
        })
        .await
    }

    fn fences(
        mut accessor: Access<'_, U, Self>,
        handle: Resource<ContextHandle>,
        ring: u8,
    ) -> wasmtime::Result<Result<StreamReader<u64>, gpu_wit::Error>> {
        let data = accessor.get();
        let context = data.table.get(&handle)?;
        // A context whose session ended has nothing left to retire:
        // its stream would be one nothing publishes to again, and a
        // reader would hang on a fence that never comes.
        if let Err(error) = data.gpu_sender(context.session) {
            return Ok(Err(to_wit_error(error)));
        }
        let shared = context.shared.clone();
        let Some(ring) = RingIndex::new(ring).filter(|ring| shared.rings().contains(*ring)) else {
            return Ok(Err(gpu_wit::Error::BadRing));
        };
        // From the stream's creation on, not from the context's first
        // submission: a reader that asks for a stream mid-render-loop
        // wants the fences that retire next, and the newest retired
        // fence answers for everything before it on the ring anyway.
        let last_seen = shared
            .timeline(ring)
            .expect("the ring was just checked against the context's count")
            .signal()
            .sequence();
        Ok(Ok(StreamReader::new(
            &mut accessor,
            FenceStreamProducer {
                context: shared,
                ring,
                waiter: None,
                last_seen,
            },
        )?))
    }

    async fn attach(
        accessor: &Accessor<U, Self>,
        handle: Resource<ContextHandle>,
        resource: Resource<BlobHandle>,
    ) -> wasmtime::Result<Result<(), gpu_wit::Error>> {
        attach_or_detach(accessor, handle, resource, true).await
    }

    async fn detach(
        accessor: &Accessor<U, Self>,
        handle: Resource<ContextHandle>,
        resource: Resource<BlobHandle>,
    ) -> wasmtime::Result<Result<(), gpu_wit::Error>> {
        attach_or_detach(accessor, handle, resource, false).await
    }
}

/// Both halves of the resource-attachment call, which differ only in
/// which message they send.
async fn attach_or_detach<U, CpuImpl, Net, HostFs>(
    accessor: &Accessor<U, HasSelf<StoreData<CpuImpl, Net, HostFs>>>,
    handle: Resource<ContextHandle>,
    resource: Resource<BlobHandle>,
    attach: bool,
) -> wasmtime::Result<Result<(), gpu_wit::Error>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let (_, context, sender) = match accessor.with(|access| context_session(access, &handle))? {
        Ok(values) => values,
        Err(error) => return Ok(Err(error)),
    };
    let blob = accessor.with(|mut access| -> wasmtime::Result<BlobId> {
        Ok(access.get().table.get(&resource)?.id)
    })?;
    let (reply, answer) = oneshot::channel();
    let request = if attach {
        Gpu3dRequest::AttachResource {
            context,
            blob,
            reply,
        }
    } else {
        Gpu3dRequest::DetachResource {
            context,
            blob,
            reply,
        }
    };
    Ok(sender.control(request, answer).await.map_err(to_wit_error))
}

impl<CpuImpl, Net, HostFs, U> gpu_wit::HostBlobWithStore<U>
    for HasSelf<StoreData<CpuImpl, Net, HostFs>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    async fn map_blob(
        accessor: &Accessor<U, Self>,
        handle: Resource<BlobHandle>,
        at: gpu_wit::Span,
    ) -> wasmtime::Result<Result<(), gpu_wit::Error>> {
        let (session, blob, sender) = match accessor.with(|access| blob_session(access, &handle))? {
            Ok(values) => values,
            Err(error) => return Ok(Err(error)),
        };
        let at = span(at);
        // A blob shown once is refused a second span here, before the
        // device is asked; the owner task's record refuses a second
        // placement that raced this check. The span is checked here too,
        // for everything but its length — the placement's size is the
        // device's answer — so a span the table would refuse never
        // costs the renderer an aperture slot.
        let checked = accessor.with(
            |mut access| -> wasmtime::Result<Result<crate::InstanceSpawner<CpuImpl>, _>> {
                if access.get().table.get(&handle)?.mapped.is_some() {
                    return Ok(Err(Gpu3dServiceError::AlreadyMapped));
                }
                let checked = memory_of(&mut access).and_then(|memory| {
                    let data = access.get();
                    data.check_session_span(memory, at, at.length)?;
                    Ok(data.spawner().clone())
                });
                Ok(checked)
            },
        )?;
        let spawner = match checked {
            Ok(spawner) => spawner,
            Err(error) => return Ok(Err(to_wit_error(error))),
        };
        let (reply, answer) = oneshot::channel();
        // A placement the device makes for a call that is dropped before
        // its last step goes back out of the aperture: a placed blob the
        // guest cannot reach is a slot the renderer holds for nothing.
        let cleanup = sender.clone();
        let mut pending = PendingReply::new(answer, move |_: DeviceRegion| {
            spawn_gpu_cleanup(&spawner, cleanup, unmap_request(blob));
        });
        let queued = sender
            .control_oneway(Gpu3dRequest::MapBlob { blob, reply })
            .await;
        let region = match queued {
            Ok(()) => pending.reply(Gpu3dServiceError::Closed).await,
            Err(error) => Err(error),
        };
        let region = match region {
            Ok(region) => region,
            Err(error) => return Ok(Err(to_wit_error(error))),
        };

        // The last step, and the only one that shows anything: the view
        // of the placement — under the memory type the host asked for —
        // and the handle's record of it are made together, with no wait
        // in between that a cancellation could land in.
        let recorded = accessor.with(|mut access| {
            let memory = checked_memory(&mut access);
            let data = access.get();
            let view = match data.show_in_session(session, |views, group| {
                views.show_device(memory, at, region, Some(group))
            }) {
                Ok(view) => view,
                Err(error) => return Ok(Err(error)),
            };
            data.table.get_mut(&handle)?.mapped = Some(view);
            Ok::<_, wasmtime::Error>(Ok(()))
        })?;
        match recorded {
            Ok(()) => {
                pending.keep();
                Ok(Ok(()))
            }
            Err(error) => {
                // The span was taken while the device was placing the
                // blob, or the session ended. The placement goes back
                // before the refusal does, with the pending reply armed
                // across the wait for the reason `create-blob` gives.
                if let Err(closed) = sender.control_oneway(unmap_request(blob)).await {
                    tracing::warn!(
                        target: "helios_kernel::gpu",
                        %closed,
                        "a placement whose span was taken could not be handed back to the 3D owner"
                    );
                }
                pending.keep();
                Ok(Err(to_wit_error(error)))
            }
        }
    }

    async fn unmap_blob(
        accessor: &Accessor<U, Self>,
        handle: Resource<BlobHandle>,
    ) -> wasmtime::Result<Result<(), gpu_wit::Error>> {
        let (_, blob, sender) = match accessor.with(|access| blob_session(access, &handle))? {
            Ok(values) => values,
            Err(error) => return Ok(Err(error)),
        };
        // The instance's view ends first — a guest that can no longer
        // reach the storage cannot be racing the engine for it — and the
        // aperture placement is handed back after.
        let ended = accessor.with(|mut access| -> wasmtime::Result<bool> {
            let data = access.get();
            let Some(view) = data.table.get_mut(&handle)?.mapped.take() else {
                return Ok(false);
            };
            data.views.end(view).map_err(view_trap)?;
            Ok(true)
        })?;
        if !ended {
            return Ok(Err(gpu_wit::Error::NotMapped));
        }
        let (reply, answer) = oneshot::channel();
        Ok(sender
            .control(Gpu3dRequest::UnmapBlob { blob, reply }, answer)
            .await
            .map_err(to_wit_error))
    }
}

/// A `ComponentStoreData` on the kernel's own test doubles, for the
/// tests that exercise these host functions against a real resource
/// table.
///
/// Lives inside the adapter because the store type names the adapter's
/// debug filesystem, and the adapter's own names are not allowed to
/// appear in `kernel/src` outside it (`kernel/tests/hal_layering.rs`).
#[cfg(test)]
pub(crate) mod test_store {
    use alloc::vec::Vec;

    use helios_hal::cpu::{Cpu, ProcessorId};
    use helios_hal::watchdog::ProgressCounter;
    use triomphe::Arc;
    use wasmtime::component::ResourceTable;

    use crate::component::{ComponentOutputMode, ComponentStoreData};
    use crate::gpu::{ContextShared, Gpu3dService, SessionId};
    use crate::test_support::{TestCpu, TestNetworkService};
    use crate::{
        Executor, InstanceRegistry, ProcessAuthority, TaskFunding, UnsupportedHostFileSystem,
    };

    use crate::wasmtime_adapter::component_host::{HostRuntimeState, StoreData};

    /// The store data these tests exercise: the component host's own
    /// generics, on the fixtures a kernel unit test carries.
    pub(crate) type TestStoreData =
        StoreData<TestCpu, TestNetworkService, UnsupportedHostFileSystem>;

    /// The serial port the fixture's writer names: the store data has
    /// to carry one, and nothing the tests do is about what it writes.
    pub(crate) struct DiscardPort;

    impl helios_hal::serial::ByteSerial for DiscardPort {
        fn try_read_byte(&self) -> Option<u8> {
            None
        }

        fn write_bytes(&self, _bytes: &[u8]) {}

        fn enable_receive_interrupt(&self) {}

        fn disable_receive_interrupt(&self) {}
    }

    static DISCARD_CONSOLE: crate::DebugConsole = crate::DebugConsole::new();

    impl crate::DebugSerialAccess for DiscardPort {
        type Port = Self;

        fn port() -> Self {
            Self
        }

        fn console() -> &'static crate::DebugConsole {
            &DISCARD_CONSOLE
        }
    }

    fn read_nothing(_: &mut Vec<u8>, _: u32) {}

    /// A context resource made under `session`, as a store that opened
    /// one through `create-context` would carry.
    pub(crate) fn context_handle(
        session: SessionId,
        shared: Arc<ContextShared>,
    ) -> super::ContextHandle {
        super::ContextHandle { session, shared }
    }

    /// A store on a runtime state that publishes `service`, with no
    /// session open yet.
    pub(crate) fn store_data(service: &Gpu3dService) -> TestStoreData {
        store_data_with(|runtime_state| runtime_state.install_gpu3d_service(service.clone()))
    }

    /// A store on a runtime state that `configure` has set up, over a
    /// fresh debug filesystem of its own.
    pub(crate) fn store_data_with(
        configure: impl FnOnce(
            &HostRuntimeState<TestCpu, TestNetworkService, UnsupportedHostFileSystem>,
        ),
    ) -> TestStoreData {
        store_data_authorized(ProcessAuthority::empty(), configure)
    }

    /// As [`store_data_with`], for an instance running under
    /// `authority`: what an `open` test needs to tell an instance that
    /// holds the GPU class from an ordinary program.
    pub(crate) fn store_data_authorized(
        authority: ProcessAuthority,
        configure: impl FnOnce(
            &HostRuntimeState<TestCpu, TestNetworkService, UnsupportedHostFileSystem>,
        ),
    ) -> TestStoreData {
        let cpu = TestCpu::with_entropy(0x5a);
        let runtime_state: HostRuntimeState<
            TestCpu,
            TestNetworkService,
            UnsupportedHostFileSystem,
        > = crate::RuntimeState::new(cpu.timebase(), 1, 0, crate::BootPayload::for_tests());
        runtime_state.install_root_entropy(Arc::new(
            crate::RootEntropy::from_platform(
                &cpu,
                None,
                None,
                crate::MachineGeneration::new(None),
            )
            .expect("the fixture CPU has an entropy source"),
        ));
        configure(&runtime_state);
        let executor = Executor::new(
            ProgressCounter::new(),
            crate::test_support::single_node_pool(1),
            ProcessorId::new(0),
        );
        let registry = InstanceRegistry::new();
        let instance = registry.register("gpu-store", 0);
        ComponentStoreData::new(
            ResourceTable::new(),
            cpu,
            executor.timer(cpu),
            executor
                .spawner(cpu)
                .instance_spawner(TaskFunding::Instance, &instance),
            runtime_state.clone(),
            registry,
            instance,
            None,
            runtime_state.root_namespace().descend(),
            Vec::new(),
            Vec::new(),
            authority,
            ComponentOutputMode::Serial,
            read_nothing,
            crate::DebugSerialWriter::of::<DiscardPort>(),
        )
    }
}
