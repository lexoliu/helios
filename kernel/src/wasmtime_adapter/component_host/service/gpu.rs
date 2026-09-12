//! `helios:system/gpu` for the component host.
//!
//! This is the whole of what a rendering plugin sees of the machine's
//! 3D engine, and none of it interprets what the plugin sends. A
//! command buffer is the plugin's own pinned pages; `submit` names a
//! run of them and the kernel hands the device their physical address.
//! A capability set is the renderer's bytes, carried whole. A host-3D
//! blob's storage is the renderer's, placed in the instance's linear
//! memory through the host-visible aperture.
//!
//! The claim lives in the instance's store, so it is single-owned: the
//! resource handles a plugin holds carry the claim generation to check
//! against and nothing else. A handle that outlived a release names an
//! engine its instance no longer holds and is refused, rather than
//! being answered against whoever holds it now.
//!
//! # Concurrency contract
//!
//! Every call here runs on the task that owns the store, so the claim
//! and its arena need no lock; a context's last-submitted fence is a
//! `Cell` for the same reason. What is shared is the two queues into
//! the owner tasks, which carry their own synchronisation, and the
//! fence signal each context was created with, which the submit server
//! publishes every retired fence to.

use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;
use core::pin::Pin;
use core::task::{Context, Poll};

use futures::channel::oneshot;
use helios_hal::display::{
    BlobId, BlobMemory, BlobUsage, CapsetId, ContextId, ContextName, FenceId, MAX_CONTEXT_NAME,
};
use helios_hal::iommu::PhysicalRange;
use triomphe::Arc;
use wasmtime::StoreContextMut;
use wasmtime::component::{
    Access, Accessor, Destination, HasSelf, Linker, Resource, StreamProducer, StreamReader,
    StreamResult,
};

use crate::ComponentHostNetwork;
use crate::display::SequenceSignal;
use crate::gpu::{BlobSpec, Gpu3dRequest, Gpu3dSender, Gpu3dServiceError, SubmitRequest};
use crate::pins::PinnedRun;
use crate::wasmtime_adapter::bindings::gpu::bindings::helios::system::gpu as gpu_wit;

use super::super::StoreData;

/// Register `helios:system/gpu` in a linker.
///
/// Every program linker gets it, for the reason the display interface
/// does: the claim is the capability, not the import. A program that
/// never claims the engine learns nothing from having the import, and
/// one that claims it on a machine with no renderer is told
/// `no-renderer`.
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

/// A plugin's hold on the 3D engine, as its store records it.
///
/// It carries the generation and nothing else: the claim itself is in
/// the store's [`crate::Gpu3dOwnership`], which is what the store's own
/// drop releases.
pub struct GpuHandle {
    generation: u64,
}

/// One renderer context, as its store records it.
pub struct ContextHandle {
    generation: u64,
    id: ContextId,
    /// Every fence retired on this context, shared with the submit
    /// server that publishes them.
    fences: Arc<SequenceSignal>,
    /// The newest fence this context has submitted. A `Cell` because
    /// every call here runs on the task that owns the store: the
    /// increasing-fence rule is the guest's own order, checked where
    /// the guest's calls are already serialised.
    submitted: Cell<u64>,
}

/// One pinned command buffer, as its store records it.
pub struct CommandBufferHandle {
    pub(crate) generation: u64,
    pub(crate) frame: PinnedRun,
}

/// One blob resource, as its store records it.
pub struct BlobHandle {
    pub(crate) generation: u64,
    pub(crate) id: BlobId,
    /// The pinned pages backing it, for the guest-backed kinds.
    pub(crate) backing: Option<PinnedRun>,
    /// The aperture placement the instance mapped, once `map-blob` has
    /// put one there.
    pub(crate) mapped: Option<PinnedRun>,
}

fn to_wit_error(error: Gpu3dServiceError) -> gpu_wit::Error {
    match error {
        Gpu3dServiceError::Unavailable => gpu_wit::Error::Unavailable,
        Gpu3dServiceError::NoRenderer => gpu_wit::Error::NoRenderer,
        Gpu3dServiceError::AlreadyClaimed => gpu_wit::Error::AlreadyClaimed,
        Gpu3dServiceError::NotClaimed => gpu_wit::Error::NotClaimed,
        Gpu3dServiceError::NoSuchCapset => gpu_wit::Error::NoSuchCapset,
        Gpu3dServiceError::UnsupportedContext => gpu_wit::Error::UnsupportedContext,
        Gpu3dServiceError::TooManyContexts => gpu_wit::Error::TooManyContexts,
        Gpu3dServiceError::TooManyBlobs => gpu_wit::Error::TooManyBlobs,
        Gpu3dServiceError::InvalidBlob => gpu_wit::Error::InvalidBlob,
        Gpu3dServiceError::ApertureExhausted => gpu_wit::Error::ApertureExhausted,
        Gpu3dServiceError::OutOfBounds => gpu_wit::Error::OutOfBounds,
        Gpu3dServiceError::FenceNotIncreasing => gpu_wit::Error::StaleFence,
        Gpu3dServiceError::WindowExhausted => gpu_wit::Error::WindowExhausted,
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

const fn to_wit_placement(frame: PinnedRun) -> gpu_wit::Placement {
    gpu_wit::Placement {
        offset: frame.offset,
        length: frame.bytes,
    }
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
    /// The queue into the 3D owner, checked against `generation`.
    fn gpu_sender(&self, generation: u64) -> Result<Gpu3dSender, Gpu3dServiceError> {
        let claim = self.device.gpu().claim_ref()?;
        if claim.generation() != generation {
            return Err(Gpu3dServiceError::NotClaimed);
        }
        Ok(claim.sender())
    }

    /// The physical run `offset`/`length` name inside `commands`,
    /// checked against `generation` — the live claim's.
    ///
    /// A buffer pinned under an older claim names a run the arena may
    /// already have handed to whoever holds the engine now, so the
    /// refusal is the WIT's answer for a dead claim, not a bounds one.
    pub(crate) fn command_range(
        &mut self,
        commands: &Resource<CommandBufferHandle>,
        offset: u64,
        length: u64,
        generation: u64,
    ) -> wasmtime::Result<Result<PhysicalRange, gpu_wit::Error>> {
        let buffer = self.table.get(commands)?;
        if buffer.generation != generation {
            return Ok(Err(gpu_wit::Error::NotClaimed));
        }
        // The submission is a run of the buffer's own pinned pages: the
        // physical run is what the device reads, and it has to lie
        // inside the run the buffer covers. An empty run is the
        // device's to refuse, not ours: its fence still has to retire,
        // and a fence spent here would never reach the stream the
        // reader waits on.
        let Some(end) = offset.checked_add(length) else {
            return Ok(Err(gpu_wit::Error::OutOfBounds));
        };
        if end > buffer.frame.bytes {
            return Ok(Err(gpu_wit::Error::OutOfBounds));
        }
        let physical = buffer.frame.physical();
        Ok(Ok(PhysicalRange::new(physical.start + offset, length)))
    }
}

impl<CpuImpl, Net, HostFs> gpu_wit::Host for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn claim(&mut self) -> wasmtime::Result<Result<Resource<GpuHandle>, gpu_wit::Error>> {
        let Some(service) = self.runtime_state.gpu3d_service() else {
            return Ok(Err(gpu_wit::Error::Unavailable));
        };
        if let Err(error) = self.device.claim_gpu(&service) {
            return Ok(Err(to_wit_error(error)));
        }
        let generation = self
            .device
            .gpu()
            .claim_ref()
            .expect("a successful claim leaves one")
            .generation();
        let handle = self.table.push(GpuHandle { generation })?;
        tracing::info!(
            target: "helios_kernel::gpu",
            generation,
            "an instance took ownership of the 3D engine"
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
        // Dropping the handle is a plugin saying it is done, and it
        // costs exactly what dying costs: every context and blob
        // released, every mapping undone and every page handed back
        // before the engine is offered to anyone else.
        if self
            .device
            .gpu()
            .claim_ref()
            .is_ok_and(|claim| claim.generation() == handle.generation)
        {
            self.device.gpu_mut().release();
        }
        Ok(())
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
        // it on the device; a context dropped by an instance that no
        // longer holds the engine has already had that done for it.
        let Ok(claim) = self.device.gpu().claim_ref() else {
            return Ok(());
        };
        if claim.generation() != context.generation {
            return Ok(());
        }
        let sender = claim.sender();
        let generation = context.generation;
        let id = context.id;
        let spawned = self.spawner().try_spawn_detached(async move {
            if let Err(error) = sender
                .control_oneway(Gpu3dRequest::DestroyContext {
                    generation,
                    context: id,
                })
                .await
            {
                tracing::warn!(
                    target: "helios_kernel::gpu",
                    %error,
                    "a dropped context could not be handed back to the 3D owner"
                );
            }
        });
        if let Err(error) = spawned {
            // The task arena is full. Nothing is leaked — the claim's
            // own release destroys the same context — but the engine
            // holds it until then, and that is worth saying.
            tracing::warn!(
                target: "helios_kernel::gpu",
                %error,
                "a dropped context stays with the 3D engine until the claim ends"
            );
        }
        Ok(())
    }
}

impl<CpuImpl, Net, HostFs> gpu_wit::HostCommandBuffer for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn buffer(
        &mut self,
        handle: Resource<CommandBufferHandle>,
    ) -> wasmtime::Result<gpu_wit::Placement> {
        let buffer = self.table.get(&handle)?;
        // `buffer` cannot say `not-claimed` — the WIT signature carries
        // no result — so a handle that outlived its claim is a trap:
        // the run it would name may already back the next claim's
        // buffers.
        self.gpu_sender(buffer.generation)
            .map_err(|error| wasmtime::Error::msg(alloc::format!("{error}")))?;
        Ok(to_wit_placement(buffer.frame))
    }

    fn drop(&mut self, handle: Resource<CommandBufferHandle>) -> wasmtime::Result<()> {
        let buffer = self.table.delete(handle)?;
        // The pages are this instance's own and nothing of the
        // device's: a dropped buffer's run goes back to the arena at
        // once. A buffer that outlived the claim goes back with the
        // release that already happened, which is the same call — the
        // arena may already be gone, and `unpin` on it is a no-op then.
        if self
            .device
            .gpu()
            .claim_ref()
            .is_ok_and(|claim| claim.generation() == buffer.generation)
        {
            self.device.gpu_mut().unpin(buffer.frame);
        }
        Ok(())
    }
}

impl<CpuImpl, Net, HostFs> gpu_wit::HostBlob for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn buffer(
        &mut self,
        handle: Resource<BlobHandle>,
    ) -> wasmtime::Result<Option<gpu_wit::Placement>> {
        let blob = self.table.get(&handle)?;
        // As for `command-buffer.buffer`: a stale handle names a run
        // that is no longer this claim's, and the infallible signature
        // leaves a trap as the only refusal.
        self.gpu_sender(blob.generation)
            .map_err(|error| wasmtime::Error::msg(alloc::format!("{error}")))?;
        Ok(blob.backing.map(to_wit_placement))
    }

    fn drop(&mut self, handle: Resource<BlobHandle>) -> wasmtime::Result<()> {
        let blob = self.table.delete(handle)?;
        // The instance's views go back first — the engine may still be
        // decoding the aperture — and then the device is told through
        // the owner task, which unmaps a still-placed blob before it
        // destroys it. A blob dropped by an instance that no longer
        // holds the engine has already had all of that done for it.
        let Ok(claim) = self.device.gpu().claim_ref() else {
            return Ok(());
        };
        if claim.generation() != blob.generation {
            return Ok(());
        }
        let sender = claim.sender();
        if let Some(mapped) = blob.mapped {
            self.device.gpu_mut().unpin(mapped);
        }
        if let Some(backing) = blob.backing {
            self.device.gpu_mut().unpin(backing);
        }
        let generation = blob.generation;
        let id = blob.id;
        let spawned = self.spawner().try_spawn_detached(async move {
            if let Err(error) = sender
                .control_oneway(Gpu3dRequest::DestroyBlob {
                    generation,
                    blob: id,
                })
                .await
            {
                tracing::warn!(
                    target: "helios_kernel::gpu",
                    %error,
                    "a dropped blob could not be handed back to the 3D owner"
                );
            }
        });
        if let Err(error) = spawned {
            tracing::warn!(
                target: "helios_kernel::gpu",
                %error,
                "a dropped blob stays with the 3D engine until the claim ends"
            );
        }
        Ok(())
    }
}

/// The claim generation a gpu handle names, and the queue into the
/// owner task.
///
/// Every call that reaches the display engine starts here, and the
/// store is given back before anything is awaited: an `Access` guard
/// held across a wait would hold the store for as long as the engine
/// takes.
fn claim_of<U, CpuImpl, Net, HostFs>(
    mut access: Access<'_, U, HasSelf<StoreData<CpuImpl, Net, HostFs>>>,
    handle: &Resource<GpuHandle>,
) -> wasmtime::Result<Result<(u64, Gpu3dSender), gpu_wit::Error>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let data = access.get();
    let generation = data.table.get(handle)?.generation;
    Ok(data
        .gpu_sender(generation)
        .map(|sender| (generation, sender))
        .map_err(to_wit_error))
}

/// What a blob call needs from the store before it waits: the
/// generation, the blob and the sender, checked together.
fn blob_claim<U, CpuImpl, Net, HostFs>(
    mut access: Access<'_, U, HasSelf<StoreData<CpuImpl, Net, HostFs>>>,
    handle: &Resource<BlobHandle>,
) -> wasmtime::Result<Result<(u64, BlobId, Gpu3dSender), gpu_wit::Error>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let data = access.get();
    let blob = data.table.get(handle)?;
    let (generation, id) = (blob.generation, blob.id);
    Ok(data
        .gpu_sender(generation)
        .map(|sender| (generation, id, sender))
        .map_err(to_wit_error))
}

/// What a context call needs from the store before it waits: the
/// generation and the sender, checked together.
fn context_claim<U, CpuImpl, Net, HostFs>(
    mut access: Access<'_, U, HasSelf<StoreData<CpuImpl, Net, HostFs>>>,
    handle: &Resource<ContextHandle>,
) -> wasmtime::Result<Result<(u64, ContextId, Gpu3dSender), gpu_wit::Error>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let data = access.get();
    let context = data.table.get(handle)?;
    let (generation, id) = (context.generation, context.id);
    Ok(data
        .gpu_sender(generation)
        .map(|sender| (generation, id, sender))
        .map_err(to_wit_error))
}

/// What `submit` needs from the store: the claim checked, the fence
/// rule applied, and the context's own signal.
///
/// The increasing-fence rule lives here rather than in the owner task
/// because the store is where the guest's submissions are already
/// serialised: `submitted` is the newest fence this context was given,
/// and a fence that does not come after it would let the guest write a
/// gap its own `fences` stream can never fill.
struct SubmitClaim {
    generation: u64,
    context: ContextId,
    sender: Gpu3dSender,
    fences: Arc<SequenceSignal>,
}

fn submit_claim<U, CpuImpl, Net, HostFs>(
    mut access: Access<'_, U, HasSelf<StoreData<CpuImpl, Net, HostFs>>>,
    handle: &Resource<ContextHandle>,
    fence: u64,
) -> wasmtime::Result<Result<SubmitClaim, gpu_wit::Error>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let data = access.get();
    let context = data.table.get(handle)?;
    if fence <= context.submitted.get() {
        return Ok(Err(gpu_wit::Error::StaleFence));
    }
    Ok(data
        .gpu_sender(context.generation)
        .map(|sender| SubmitClaim {
            generation: context.generation,
            context: context.id,
            sender,
            fences: context.fences.clone(),
        })
        .map_err(to_wit_error))
}

/// The stream a plugin reads one context's fences from.
struct FenceStreamProducer {
    signal: Arc<SequenceSignal>,
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
        let signal = producer.signal.clone();
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
        let (generation, sender) = match accessor.with(|access| claim_of(access, &handle))? {
            Ok(pair) => pair,
            Err(error) => return Ok(Err(error)),
        };
        let (reply, answer) = oneshot::channel();
        let outcome = sender
            .control(Gpu3dRequest::Capsets { generation, reply }, answer)
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
    ) -> wasmtime::Result<Result<Vec<u8>, gpu_wit::Error>> {
        let (generation, sender) = match accessor.with(|access| claim_of(access, &handle))? {
            Ok(pair) => pair,
            Err(error) => return Ok(Err(error)),
        };
        let (reply, answer) = oneshot::channel();
        let outcome = sender
            .control(
                Gpu3dRequest::Capset {
                    generation,
                    id: CapsetId::new(id),
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
        name: String,
    ) -> wasmtime::Result<Result<Resource<ContextHandle>, gpu_wit::Error>> {
        let (generation, sender) = match accessor.with(|access| claim_of(access, &handle))? {
            Ok(pair) => pair,
            Err(error) => return Ok(Err(error)),
        };
        let (reply, answer) = oneshot::channel();
        let record = sender
            .control(
                Gpu3dRequest::CreateContext {
                    generation,
                    capset: CapsetId::new(capset),
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
                generation,
                id: record.id,
                fences: record.fences,
                submitted: Cell::new(0),
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
        host_id: u64,
    ) -> wasmtime::Result<Result<Resource<BlobHandle>, gpu_wit::Error>> {
        let (generation, sender) = match accessor.with(|access| claim_of(access, &handle))? {
            Ok(pair) => pair,
            Err(error) => return Ok(Err(error)),
        };
        let context_id = accessor.with(|mut access| -> wasmtime::Result<ContextId> {
            Ok(access.get().table.get(&context)?.id)
        })?;
        let memory_kind = from_wit_blob_memory(memory);
        // A guest-backed blob's pages are the instance's own, committed
        // before the device is asked: the resource the device creates
        // names their physical run directly. A host-3D blob has no
        // backing to pin.
        let backing = match memory_kind {
            BlobMemory::Host3d => None,
            BlobMemory::Guest | BlobMemory::Host3dGuest => Some(
                match accessor.with(|mut access| access.get().device.gpu_mut().pin(size)) {
                    Ok(frame) => frame,
                    Err(error) => return Ok(Err(to_wit_error(error))),
                },
            ),
        };
        let (reply, answer) = oneshot::channel();
        let blob = sender
            .control(
                Gpu3dRequest::CreateBlob {
                    generation,
                    spec: BlobSpec {
                        context: context_id,
                        memory: memory_kind,
                        usage: from_wit_blob_usage(usage),
                        size,
                        host_id,
                        backing: backing.map(|frame| frame.backing),
                    },
                    reply,
                },
                answer,
            )
            .await;
        let id = match blob {
            Ok(id) => id,
            Err(error) => {
                // Nothing of the device's names these pages, so they go
                // straight back rather than waiting for a release that
                // will never come.
                if let Some(frame) = backing {
                    accessor.with(|mut access| access.get().device.gpu_mut().unpin(frame));
                }
                return Ok(Err(to_wit_error(error)));
            }
        };
        let resource = accessor.with(|mut access| {
            access.get().table.push(BlobHandle {
                generation,
                id,
                backing,
                mapped: None,
            })
        })?;
        Ok(Ok(resource))
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
        bytes: u64,
    ) -> wasmtime::Result<Result<Resource<CommandBufferHandle>, gpu_wit::Error>> {
        let generation = match accessor.with(|access| context_claim(access, &handle))? {
            Ok((generation, _, _)) => generation,
            Err(error) => return Ok(Err(error)),
        };
        if bytes == 0 {
            return Ok(Err(gpu_wit::Error::OutOfBounds));
        }
        let frame = match accessor.with(|mut access| access.get().device.gpu_mut().pin(bytes)) {
            Ok(frame) => frame,
            Err(error) => return Ok(Err(to_wit_error(error))),
        };
        let resource = accessor.with(|mut access| {
            access
                .get()
                .table
                .push(CommandBufferHandle { generation, frame })
        })?;
        Ok(Ok(resource))
    }

    async fn submit(
        accessor: &Accessor<U, Self>,
        handle: Resource<ContextHandle>,
        commands: Resource<CommandBufferHandle>,
        offset: u64,
        length: u64,
        fence: u64,
    ) -> wasmtime::Result<Result<(), gpu_wit::Error>> {
        let SubmitClaim {
            generation,
            context,
            sender,
            fences,
        } = match accessor.with(|access| submit_claim(access, &handle, fence))? {
            Ok(values) => values,
            Err(error) => return Ok(Err(error)),
        };
        let range = match accessor.with(|mut access| {
            access
                .get()
                .command_range(&commands, offset, length, generation)
        })? {
            Ok(range) => range,
            Err(error) => return Ok(Err(error)),
        };
        let (reply, answer) = oneshot::channel();
        let outcome = sender
            .submit(
                SubmitRequest::Submit {
                    generation,
                    context,
                    commands: range,
                    fence: FenceId::new(fence),
                    fences,
                    reply,
                },
                answer,
            )
            .await;
        // The fence was spent the moment the device answered, agreed or
        // refused: the next submission must come after it either way.
        accessor.with(|mut access| {
            if let Ok(context) = access.get().table.get(&handle) {
                context.submitted.set(fence);
            }
        });
        Ok(outcome.map_err(to_wit_error))
    }

    fn fences(
        mut accessor: Access<'_, U, Self>,
        handle: Resource<ContextHandle>,
    ) -> wasmtime::Result<StreamReader<u64>> {
        let data = accessor.get();
        let context = data.table.get(&handle)?;
        // As for `command-buffer.buffer`: the signature cannot carry
        // `not-claimed`, so a context that outlived its claim is a
        // trap. Answering it would hand back the dead claim's signal —
        // one nothing publishes to again — and the guest's stream read
        // would hang on a fence that never retires.
        data.gpu_sender(context.generation)
            .map_err(|error| wasmtime::Error::msg(alloc::format!("{error}")))?;
        let signal = context.fences.clone();
        // From the stream's creation on, not from the context's first
        // submission: a reader that asks for a stream mid-render-loop
        // wants the fences that retire next, and the newest retired
        // fence answers for everything before it anyway.
        let last_seen = signal.sequence();
        StreamReader::new(
            &mut accessor,
            FenceStreamProducer {
                signal,
                waiter: None,
                last_seen,
            },
        )
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
    let (generation, context, sender) =
        match accessor.with(|access| context_claim(access, &handle))? {
            Ok(values) => values,
            Err(error) => return Ok(Err(error)),
        };
    let blob = accessor.with(|mut access| -> wasmtime::Result<BlobId> {
        Ok(access.get().table.get(&resource)?.id)
    })?;
    let (reply, answer) = oneshot::channel();
    let request = if attach {
        Gpu3dRequest::AttachResource {
            generation,
            context,
            blob,
            reply,
        }
    } else {
        Gpu3dRequest::DetachResource {
            generation,
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
    ) -> wasmtime::Result<Result<gpu_wit::Placement, gpu_wit::Error>> {
        let (generation, blob, sender) =
            match accessor.with(|access| blob_claim(access, &handle))? {
                Ok(values) => values,
                Err(error) => return Ok(Err(error)),
            };
        let (reply, answer) = oneshot::channel();
        let region = match sender
            .control(
                Gpu3dRequest::MapBlob {
                    generation,
                    blob,
                    reply,
                },
                answer,
            )
            .await
        {
            Ok(region) => region,
            Err(error) => return Ok(Err(to_wit_error(error))),
        };
        // The device has placed the blob's storage in the aperture;
        // now the instance's own mapping of it lands in its window. If
        // the window cannot take it, the aperture placement is handed
        // back before the refusal is reported — a placed blob the
        // guest cannot reach is a slot the renderer is holding for
        // nothing.
        let frame = match accessor.with(|mut access| access.get().device.gpu_mut().map_blob(region))
        {
            Ok(frame) => frame,
            Err(error) => {
                let (reply, answer) = oneshot::channel();
                let _ = sender
                    .control(
                        Gpu3dRequest::UnmapBlob {
                            generation,
                            blob,
                            reply,
                        },
                        answer,
                    )
                    .await;
                return Ok(Err(to_wit_error(error)));
            }
        };
        let placement = to_wit_placement(frame);
        accessor.with(|mut access| {
            if let Ok(blob) = access.get().table.get_mut(&handle) {
                blob.mapped = Some(frame);
            }
        });
        Ok(Ok(placement))
    }

    async fn unmap_blob(
        accessor: &Accessor<U, Self>,
        handle: Resource<BlobHandle>,
    ) -> wasmtime::Result<Result<(), gpu_wit::Error>> {
        let (generation, blob, sender) =
            match accessor.with(|access| blob_claim(access, &handle))? {
                Ok(values) => values,
                Err(error) => return Ok(Err(error)),
            };
        // The instance's own mapping goes first — a guest that can no
        // longer reach the storage cannot be racing the engine for it —
        // and the aperture placement is handed back after.
        let frame = accessor.with(|mut access| {
            let data = access.get();
            let mapped = data
                .table
                .get_mut(&handle)
                .ok()
                .and_then(|blob| blob.mapped.take());
            if let Some(frame) = mapped {
                data.device.gpu_mut().unpin(frame);
            }
            mapped
        });
        if frame.is_none() {
            return Ok(Err(gpu_wit::Error::InvalidBlob));
        }
        let (reply, answer) = oneshot::channel();
        Ok(sender
            .control(
                Gpu3dRequest::UnmapBlob {
                    generation,
                    blob,
                    reply,
                },
                answer,
            )
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

    use helios_hal::cpu::ProcessorId;
    use helios_hal::watchdog::ProgressCounter;
    use triomphe::Arc;
    use wasmtime::component::ResourceTable;

    use crate::component::{ComponentOutputMode, ComponentStoreData};
    use crate::gpu::Gpu3dService;
    use crate::test_support::{TestCpu, TestNetworkService};
    use crate::wasmtime_adapter::wasi::DebugFileSystem;
    use crate::{
        Executor, InstanceRegistry, ProcessAuthority, TaskFunding, Timer, UnsupportedHostFileSystem,
    };

    use crate::wasmtime_adapter::component_host::{HostRuntimeState, StoreData};

    /// The store data these tests exercise: the component host's own
    /// generics, on the fixtures a kernel unit test carries.
    pub(crate) type TestStoreData =
        StoreData<TestCpu, TestNetworkService, UnsupportedHostFileSystem>;

    /// The serial port the fixture's writer names: the store data has
    /// to carry one, and nothing the tests do is about what it writes.
    struct DiscardPort;

    impl helios_hal::serial::ByteSerial for DiscardPort {
        fn try_read_byte(&self) -> Option<u8> {
            None
        }

        fn write_bytes(&self, _bytes: &[u8]) {}
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

    /// A context resource minted under `generation`, as a store that
    /// opened one through `create-context` would carry.
    pub(crate) fn context_handle(
        generation: u64,
        id: helios_hal::display::ContextId,
    ) -> super::ContextHandle {
        super::ContextHandle {
            generation,
            id,
            fences: Arc::new(crate::display::SequenceSignal::new()),
            submitted: core::cell::Cell::new(0),
        }
    }

    /// A store on a runtime state that publishes `service`, holding no
    /// claim yet.
    pub(crate) fn store_data(service: &Gpu3dService) -> TestStoreData {
        let cpu = TestCpu::with_entropy(0x5a);
        let runtime_state: HostRuntimeState<
            TestCpu,
            TestNetworkService,
            UnsupportedHostFileSystem,
        > = crate::RuntimeState::new(1_000_000, 1, 0);
        runtime_state.install_root_entropy(Arc::new(
            crate::RootEntropy::from_platform(&cpu, None, None)
                .expect("the fixture CPU has an entropy source"),
        ));
        runtime_state.install_gpu3d_service(service.clone());
        let executor = Executor::new(ProgressCounter::new(), 1, ProcessorId::new(0));
        let registry = InstanceRegistry::new();
        let instance = registry.register("gpu-store", 0);
        ComponentStoreData::new(
            ResourceTable::new(),
            cpu,
            Timer::new(cpu),
            executor
                .spawner(cpu)
                .instance_spawner(TaskFunding::Instance),
            runtime_state.clone(),
            registry,
            instance,
            None,
            DebugFileSystem::new(runtime_state),
            Vec::new(),
            Vec::new(),
            ProcessAuthority::empty(),
            ComponentOutputMode::Serial,
            read_nothing,
            crate::DebugSerialWriter::of::<DiscardPort>(),
        )
    }
}
