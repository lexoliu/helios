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
//! # Concurrency contract
//!
//! [`Gpu3dService`] is cloneable and every method on it may be called
//! from any processor. `claim` is a compare-and-exchange on one word;
//! the request queues are the kernel's bounded provider queues, whose
//! producers park on a permit-based notification rather than spinning.
//!
//! Two queues, because the work has two urgencies. Everything that
//! changes what the renderer holds — contexts, blobs, the mappings
//! between them — goes on the control queue, where a capset read of
//! tens of kilobytes may take the device a while to answer. Command
//! submissions go on the submit queue, so a submission never waits
//! behind one of those reads. The fence a submission carries is retired
//! by the submit server itself: the driver's completion is the signal,
//! and it is published on the context's [`SequenceSignal`] the request
//! carried, which is what a guest's `fences` stream reads.
//!
//! A claim's release is not a message, for the reason the display's is
//! not: the store that holds a claim is dropped by whatever kills its
//! instance, and a drop cannot await a queue that is full. The release
//! is one permit on a [`Notify`] the control server races against its
//! inbox, and the claim word moves to [`ClaimState::RELEASING`] at the
//! same moment, so nobody is handed the engine between the moment its
//! last owner let go and the moment the renderer has actually given the
//! resources back.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use concurrent_queue::ConcurrentQueue;
use futures::channel::oneshot;
use helios_hal::device::DeviceRegion;
use helios_hal::display::{
    BlobId, BlobMemory, BlobUsage, CapsetId, CapsetList, ContextId, ContextName, FenceId,
};
use helios_hal::iommu::PhysicalRange;
use helios_hal::pmm::PhysFrameRange;
use triomphe::Arc;

use crate::component::{ProviderError, ProviderSender};
use crate::display::SequenceSignal;
use crate::exec::Notify;

use super::Gpu3dServiceError;
use super::GpuPins;

/// Requests one claim may have in flight on each queue before its next
/// one waits for room.
///
/// The same bound the display path sets, for the same reason: a plugin
/// that has queued this many submissions is one the renderer has not
/// kept up with, and making it wait is the backpressure a command path
/// needs.
pub const REQUEST_QUEUE_DEPTH: usize = crate::display::REQUEST_QUEUE_DEPTH;

/// One renderer context a claim opened, as the store records it.
///
/// The fence signal is shared with the submit server, which publishes
/// every fence that retires on this context to it; a `fences` stream
/// the guest holds reads the same signal.
pub struct ContextRecord {
    /// The device-side context identifier.
    pub id: ContextId,
    /// The renderer this context speaks to.
    pub capset: CapsetId,
    /// Every fence retired on this context, published as its raw value.
    pub fences: Arc<SequenceSignal>,
}

/// Work for the task that owns what the renderer holds.
///
/// Every variant carries the claim generation it was made under. A
/// request that outlived its claim names a renderer its instance no
/// longer holds, and serving it against whoever holds the engine now
/// would let a dead plugin drive a live one's device.
pub(crate) enum Gpu3dRequest {
    /// Every capability set the device carries, in its own order.
    Capsets {
        generation: u64,
        reply: oneshot::Sender<Result<CapsetList, Gpu3dServiceError>>,
    },
    /// The bytes of one capability set, at the newest version the host
    /// speaks.
    Capset {
        generation: u64,
        id: CapsetId,
        reply: oneshot::Sender<Result<Vec<u8>, Gpu3dServiceError>>,
    },
    /// Open a renderer context of the type `capset` names.
    CreateContext {
        generation: u64,
        capset: CapsetId,
        name: ContextName,
        reply: oneshot::Sender<Result<ContextRecord, Gpu3dServiceError>>,
    },
    /// Let a context go. No reply: the store drops the handle and the
    /// renderer is told by the task that owns it.
    DestroyContext { generation: u64, context: ContextId },
    /// Create a blob resource.
    CreateBlob {
        generation: u64,
        spec: BlobSpec,
        reply: oneshot::Sender<Result<BlobId, Gpu3dServiceError>>,
    },
    /// Let a blob go. A mapped blob is taken out of the aperture first.
    DestroyBlob { generation: u64, blob: BlobId },
    /// Place a blob's host storage in the engine's host-visible
    /// aperture and report where it landed.
    MapBlob {
        generation: u64,
        blob: BlobId,
        reply: oneshot::Sender<Result<DeviceRegion, Gpu3dServiceError>>,
    },
    /// Take a blob back out of the aperture.
    UnmapBlob {
        generation: u64,
        blob: BlobId,
        reply: oneshot::Sender<Result<(), Gpu3dServiceError>>,
    },
    /// Make a blob reachable from a context's command stream.
    AttachResource {
        generation: u64,
        context: ContextId,
        blob: BlobId,
        reply: oneshot::Sender<Result<(), Gpu3dServiceError>>,
    },
    /// Take a blob back out of a context's command stream.
    DetachResource {
        generation: u64,
        context: ContextId,
        blob: BlobId,
        reply: oneshot::Sender<Result<(), Gpu3dServiceError>>,
    },
}

/// What one `CreateBlob` request describes.
///
/// `backing` carries the claiming instance's own pinned run for the
/// guest-backed kinds and is absent for a host-3D blob, whose storage
/// is the renderer's.
#[derive(Clone, Copy)]
pub(crate) struct BlobSpec {
    /// The renderer context the resource belongs to.
    pub(crate) context: ContextId,
    /// Where its storage lives.
    pub(crate) memory: BlobMemory,
    /// What it may be used for.
    pub(crate) usage: BlobUsage,
    /// Its size in bytes.
    pub(crate) size: u64,
    /// The host-side identifier the resource is shared under, or 0.
    pub(crate) host_id: u64,
    /// The claiming instance's own pinned run, for the guest-backed
    /// kinds.
    pub(crate) backing: Option<PhysFrameRange>,
}

impl Gpu3dRequest {
    pub(crate) const fn generation(&self) -> u64 {
        match self {
            Self::Capsets { generation, .. }
            | Self::Capset { generation, .. }
            | Self::CreateContext { generation, .. }
            | Self::DestroyContext { generation, .. }
            | Self::CreateBlob { generation, .. }
            | Self::DestroyBlob { generation, .. }
            | Self::MapBlob { generation, .. }
            | Self::UnmapBlob { generation, .. }
            | Self::AttachResource { generation, .. }
            | Self::DetachResource { generation, .. } => *generation,
        }
    }
}

/// Work for the task that serves the command streams.
///
/// The fence signal the context was created with travels with the
/// submission: when the device has taken the command buffer the server
/// publishes `fence` to it, whatever the device answered, because a
/// fenced command's completion is the signal and a reader must not be
/// left waiting on a command the device refused.
pub(crate) enum SubmitRequest {
    Submit {
        generation: u64,
        context: ContextId,
        /// The guest's own pinned pages the device is to read the
        /// command stream out of, by physical run.
        commands: PhysicalRange,
        fence: FenceId,
        fences: Arc<SequenceSignal>,
        reply: oneshot::Sender<Result<(), Gpu3dServiceError>>,
    },
}

/// What the 3D engine's claim word holds.
///
/// Three states rather than two, for the reason the display's word has
/// three: giving the resources back is asynchronous — contexts,
/// blobs and aperture placements all have to be told back to the
/// device — and until they have been, the engine is neither held by
/// anybody nor free for anybody.
pub(super) struct ClaimState;

impl ClaimState {
    /// Nobody holds the engine.
    pub(super) const FREE: u8 = 0;
    /// One instance holds it.
    pub(super) const HELD: u8 = 1;
    /// Its last owner let go and the owner task has not finished
    /// handing the resources back.
    pub(super) const RELEASING: u8 = 2;
}

/// What every holder of the 3D engine shares with the tasks that own it.
pub(super) struct Gpu3dShared {
    pub(super) control: ProviderSender<Gpu3dRequest>,
    pub(super) submit: ProviderSender<SubmitRequest>,
    /// One of [`ClaimState`]'s three values.
    pub(super) claim: AtomicU8,
    /// Bumped by every successful claim, so a request that outlived its
    /// claim can be told from one that did not.
    pub(super) generation: AtomicU64,
    /// One permit per claim that has been let go.
    pub(super) release: Notify,
    /// Whether the device renders at all. Read once at bring-up and
    /// never again: a renderer is a property of the host's build, not
    /// of anything the machine does while it runs.
    pub(super) renders: bool,
    /// The pinned pages of a claim that has been let go, waiting for
    /// the owner task to stop the renderer reading them.
    ///
    /// One slot, and one is provably enough: there is at most one claim
    /// at a time, and the claim word does not return to
    /// [`ClaimState::FREE`] until this has been drained.
    pub(super) returned: ConcurrentQueue<GpuPins>,
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

    /// Whether an instance holds the 3D engine right now.
    pub fn is_claimed(&self) -> bool {
        self.shared.claim.load(Ordering::Acquire) != ClaimState::FREE
    }

    /// Whether this machine's display engine renders at all.
    pub fn renders(&self) -> bool {
        self.shared.renders
    }

    /// Take exclusive ownership of the machine's 3D engine.
    ///
    /// Refused when the device renders nothing — there is no renderer
    /// to hold — when another instance holds it, and when the previous
    /// owner's resources are still being handed back: the engine is
    /// not free until the display engine says it is.
    pub fn claim(&self) -> Result<Gpu3dClaim, Gpu3dServiceError> {
        if !self.shared.renders {
            return Err(Gpu3dServiceError::NoRenderer);
        }
        self.shared
            .claim
            .compare_exchange(
                ClaimState::FREE,
                ClaimState::HELD,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| Gpu3dServiceError::AlreadyClaimed)?;
        let generation = self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        Ok(Gpu3dClaim {
            shared: self.shared.clone(),
            generation,
        })
    }
}

/// One instance's hold on the machine's 3D engine.
///
/// Dropping it — or dying while holding it — moves the claim word to
/// [`ClaimState::RELEASING`] and wakes the owner task, which releases
/// every context, blob and mapping this claim created before the engine
/// is offered to anyone else.
pub struct Gpu3dClaim {
    shared: Arc<Gpu3dShared>,
    generation: u64,
}

impl Gpu3dClaim {
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// A handle that can queue work for this claim.
    ///
    /// Cheap and cloneable, so a host call that has to keep sending
    /// after it has given the store back carries one instead of
    /// borrowing the claim. A sender that outlives its claim sends
    /// requests the owner task drops, because they no longer name the
    /// generation it is serving.
    pub fn sender(&self) -> Gpu3dSender {
        Gpu3dSender {
            shared: self.shared.clone(),
            generation: self.generation,
        }
    }

    /// Hand this claim's pinned pages to the owner task.
    ///
    /// The pages are not freed here: the renderer may still be reading
    /// a guest blob, and the owner task is what knows when it has
    /// stopped. Called by the store's own drop, so it neither allocates
    /// nor waits.
    ///
    /// # Panics
    ///
    /// Panics when the slot is already occupied, which would mean two
    /// claims existed at once — the one invariant the claim word is
    /// there to keep.
    pub(super) fn return_pins(&self, pins: GpuPins) {
        self.shared
            .returned
            .push(pins)
            .unwrap_or_else(|_| panic!("two 3D claims returned their pages at once"));
    }
}

/// The right to queue work for one claim.
#[derive(Clone)]
pub struct Gpu3dSender {
    shared: Arc<Gpu3dShared>,
    generation: u64,
}

impl Gpu3dSender {
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Queue `request` on the control queue and await its reply.
    pub(crate) async fn control<T>(
        &self,
        request: Gpu3dRequest,
        reply: oneshot::Receiver<Result<T, Gpu3dServiceError>>,
    ) -> Result<T, Gpu3dServiceError> {
        self.shared.control.send(request).await.map_err(closed)?;
        reply.await.map_err(|_| Gpu3dServiceError::Closed)?
    }

    /// Queue `request` on the submit queue and await its reply.
    pub(crate) async fn submit(
        &self,
        request: SubmitRequest,
        reply: oneshot::Receiver<Result<(), Gpu3dServiceError>>,
    ) -> Result<(), Gpu3dServiceError> {
        self.shared.submit.send(request).await.map_err(closed)?;
        reply.await.map_err(|_| Gpu3dServiceError::Closed)?
    }

    /// Tell the owner task about a resource this claim no longer wants,
    /// without waiting for it to be gone.
    ///
    /// The display engine still has to be told, and telling it is the
    /// owner task's work; what the caller gets back is only that the
    /// message is queued.
    pub(crate) async fn control_oneway(
        &self,
        request: Gpu3dRequest,
    ) -> Result<(), Gpu3dServiceError> {
        self.shared.control.send(request).await.map_err(closed)
    }
}

impl Drop for Gpu3dClaim {
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

const fn closed(error: ProviderError) -> Gpu3dServiceError {
    match error {
        ProviderError::Unavailable | ProviderError::Closed | ProviderError::Full => {
            Gpu3dServiceError::Closed
        }
    }
}
