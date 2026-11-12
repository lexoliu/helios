//! The 3D contract of a display engine.
//!
//! A display engine that can render owns a second set of hardware facts
//! beside its scanouts: a set of *capability sets*, each naming a
//! renderer the host is willing to speak; *contexts*, which are that
//! renderer's per-client state; *blobs*, which are the resources a
//! context works on; and *fences*, which are how the device says the
//! work before a point on one of a context's timelines has been
//! consumed. None of it is a consumer's vocabulary — a Vulkan driver, a
//! compositor, a media stack all use the same four — so the contract
//! lives here and the concrete driver encodes it onto its own wire
//! format.
//!
//! # What this contract does not do
//!
//! It never interprets a command stream and never decodes a single byte
//! of a capability set. A capability set is a device-issued opaque blob
//! whose meaning belongs to whichever renderer asked for it, and a
//! command buffer is the same. The engine moves bytes, maps memory and
//! signals fences; a guest that knows what those bytes mean is the one
//! that produced them.
//!
//! # Where a blob's memory is
//!
//! Three places, and which one is a property of the blob rather than a
//! choice the driver makes afterwards:
//!
//! * [`BlobMemory::Guest`] — pages the caller owns, published to the
//!   device as the resource's backing store, exactly as a 2D frame
//!   buffer's are.
//! * [`BlobMemory::Host3d`] — storage the renderer allocated on the
//!   host. The guest reaches it only by asking the device to place it in
//!   the engine's host-visible aperture, which is what
//!   [`Gpu3d::map_blob`] does and what it hands back a [`DeviceRegion`]
//!   for, carrying the memory type the renderer asked the mapping to
//!   have.
//! * [`BlobMemory::Host3dGuest`] — both: host storage the renderer
//!   works on, backed by pages the caller owns.
//!
//! # Timelines
//!
//! A context carries between one and [`MAX_RINGS`] fence timelines, its
//! *rings*, chosen when it is created. Ring 0 is the renderer's CPU
//! timeline, signalled once the renderer has processed a command; the
//! others are bound to the renderer's queues by the guest's own
//! protocol. A fence names a point on one ring of one context and
//! retires only work submitted there, so one context's progress — or
//! one queue's — never answers for another's.
//!
//! # SMP contract
//!
//! Every method takes `&self` and may be called from any processor and
//! from several tasks at once; an implementation serialises access to
//! its own rings internally. Ordering between two concurrent calls is
//! the ordering the device sees, so a caller that needs one submission
//! to precede another lets the first reach the device — its `posted`
//! signal — before it starts the second.
//!
//! Fences are the one ordering guarantee this contract makes without a
//! caller waiting. A ring's timeline is monotone, so the newest
//! signalled fence answers for every one before it: a reader that asks
//! for "the first fence after `n`" may be handed the newest of several
//! retired since it last looked, and has still missed nothing.

use core::future::Future;

use arrayvec::{ArrayString, ArrayVec};
use thiserror::Error;

use crate::device::{DeviceRegion, MemoryKind};
use crate::io::IoError;
use crate::iommu::PhysicalRange;
use crate::pmm::PhysFrameRange;

use super::{MAX_BACKING_RANGES, PixelFormat};

/// Capability sets one display engine may carry.
///
/// The bound is the hardware's: the renderers a virtio-gpu host exposes
/// are a fixed short list (virgl, virgl2, gfxstream-vulkan, venus,
/// cross-domain, drm), and no display engine Helios targets publishes
/// more than that. It is what makes [`CapsetList`] a value rather than
/// an allocation.
pub const MAX_CAPSETS: usize = 8;

/// Longest debug name a context may be created with.
///
/// The name is what a host renderer prints when it faults, so it is
/// bounded by what the wire format carries — virtio-gpu's `debug_name`
/// is 64 bytes — rather than by anything the kernel chooses.
pub const MAX_CONTEXT_NAME: usize = 64;

/// Fence timelines one context may carry.
///
/// Linux's virtgpu uAPI bounds `VIRTGPU_CONTEXT_PARAM_NUM_RINGS` at 64
/// (`virtgpu_ioctl.c`, `MAX_RINGS`), Mesa's Venus driver asks for
/// exactly that many (`vn_renderer_virtgpu.c`, `max_timeline_count`),
/// and the wire's `ring_idx` is one byte, so a ring index is always
/// below this.
pub const MAX_RINGS: usize = 64;

/// One renderer the host is willing to speak, as the device names it.
///
/// The number is the device's own, never interpreted here: it selects a
/// capability set to read and it is what a context is created against.
/// The constants below are the ones the virtio-gpu specification
/// assigns, given names so a caller reads rather than counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CapsetId(u32);

impl CapsetId {
    /// OpenGL through virglrenderer's first protocol.
    pub const VIRGL: Self = Self(1);
    /// OpenGL through virglrenderer's second protocol.
    pub const VIRGL2: Self = Self(2);
    /// Vulkan through gfxstream.
    pub const GFXSTREAM_VULKAN: Self = Self(3);
    /// Vulkan through venus.
    pub const VENUS: Self = Self(4);
    /// The cross-domain protocol.
    pub const CROSS_DOMAIN: Self = Self(5);
    /// The DRM native-context protocol.
    pub const DRM: Self = Self(6);

    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// One capability set the device carries.
///
/// Three facts and no payload: which renderer it describes, the newest
/// version of it the host speaks, and how many bytes reading it back
/// takes. The bytes themselves are read on demand, because the largest
/// of them runs to tens of kilobytes and nothing in the kernel looks at
/// one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapsetInfo {
    pub id: CapsetId,
    /// Newest version of this capability set the host speaks.
    pub max_version: u32,
    /// Bytes [`Gpu3d::capset`] writes for the newest version.
    pub max_size: u32,
}

/// Every capability set a device carries, in the device's own order.
pub type CapsetList = ArrayVec<CapsetInfo, MAX_CAPSETS>;

/// The debug name a context carries, which a host renderer prints when
/// that context faults.
pub type ContextName = ArrayString<MAX_CONTEXT_NAME>;

/// One renderer context the device holds for a caller.
///
/// The identifier is opaque and names device-side state: the renderer's
/// per-client object table, its command timelines, and the resources
/// attached to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContextId(u32);

impl ContextId {
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// One blob resource the device holds for a caller.
///
/// The number is the device's resource id itself — what a renderer's
/// own protocol names the resource by when it describes a ring, a reply
/// stream or a scanout — so it is handed to the guest as it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlobId(u32);

impl BlobId {
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// One of a context's fence timelines.
///
/// Always below [`MAX_RINGS`]; whether it is below the count a given
/// context was created with is [`RingCount::contains`]'s question.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RingIndex(u8);

impl RingIndex {
    /// The renderer's CPU timeline, which every context has.
    pub const CPU: Self = Self(0);

    /// The ring `index` names, or `None` when no context can carry one
    /// that high.
    pub const fn new(index: u8) -> Option<Self> {
        if (index as usize) < MAX_RINGS {
            Some(Self(index))
        } else {
            None
        }
    }

    pub const fn raw(self) -> u8 {
        self.0
    }

    /// The ring as an index into a per-ring table.
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// How many fence timelines a context carries: between one and
/// [`MAX_RINGS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RingCount(u8);

impl RingCount {
    /// One ring: the CPU timeline alone.
    pub const ONE: Self = Self(1);

    /// The count `rings` names, or `None` when it is zero or past
    /// [`MAX_RINGS`].
    pub const fn new(rings: u8) -> Option<Self> {
        if rings != 0 && (rings as usize) <= MAX_RINGS {
            Some(Self(rings))
        } else {
            None
        }
    }

    pub const fn get(self) -> u8 {
        self.0
    }

    /// Whether `ring` is one of this many.
    pub const fn contains(self, ring: RingIndex) -> bool {
        ring.0 < self.0
    }

    /// Every ring of this many, lowest first.
    pub fn rings(self) -> impl Iterator<Item = RingIndex> {
        (0..self.0).map(RingIndex)
    }
}

/// One point on one of a context's completion timelines.
///
/// A caller chooses the number and the device hands it back once every
/// command submitted before it on that ring has been consumed.
/// Increasing per ring, which is the whole of the ordering
/// [`Gpu3d::fences`] rests on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FenceId(u64);

impl FenceId {
    /// The point before any fence: what a reader that has seen nothing
    /// asks for the next fence after.
    pub const START: Self = Self(0);

    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Where a blob's storage lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobMemory {
    /// Pages the caller owns, published to the device as the resource's
    /// backing store.
    Guest,
    /// Storage the host renderer allocated, reachable from the guest
    /// only through [`Gpu3d::map_blob`].
    Host3d,
    /// Host storage backed by pages the caller owns.
    Host3dGuest,
}

impl BlobMemory {
    /// Whether the renderer owns storage of its own for the blob, which
    /// is what a renderer blob id names.
    pub const fn is_host(self) -> bool {
        matches!(self, Self::Host3d | Self::Host3dGuest)
    }

    /// Whether the caller's pages back the blob.
    pub const fn is_guest_backed(self) -> bool {
        matches!(self, Self::Guest | Self::Host3dGuest)
    }
}

bitflags::bitflags! {
    /// What the caller intends to do with a blob, which is what the
    /// host has to know before it decides where to put it.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct BlobUsage: u32 {
        /// The caller will ask for it in its own address space.
        const MAPPABLE = 1 << 0;
        /// The caller will export it to another context.
        const SHAREABLE = 1 << 1;
        /// The caller will hand it to another device.
        const CROSS_DEVICE = 1 << 2;
    }
}

/// Everything a blob is created with.
///
/// A record rather than eight positional arguments, because four of
/// them are numbers of the same width and a caller that transposed two
/// would be handing the renderer a resource of the wrong size against
/// the wrong host object.
#[derive(Clone, Copy, Debug)]
pub struct BlobRequest<'a> {
    /// The context whose renderer owns the blob. Every blob belongs to
    /// one: a host-3D allocation is the renderer's, and a guest blob is
    /// still named on that renderer's timeline.
    pub context: ContextId,
    /// Where its storage lives.
    pub memory: BlobMemory,
    /// What the caller intends to do with it.
    pub usage: BlobUsage,
    /// How many bytes it covers. The driver rounds what it asks the
    /// device for up to the device's own blob alignment.
    pub size: u64,
    /// The renderer's own name for the host allocation — virtio-gpu's
    /// `blob_id`. For Venus it is 0 for shared memory and the
    /// `VkDeviceMemory` object id otherwise; it comes out of the command
    /// stream the guest submitted, so the kernel carries it and never
    /// mints one. A blob with no host storage ([`BlobMemory::Guest`])
    /// has nothing for it to name and carries 0.
    pub blob_id: u64,
    /// The caller's pages, for [`BlobMemory::Guest`] and
    /// [`BlobMemory::Host3dGuest`]. Empty for [`BlobMemory::Host3d`],
    /// whose storage is the host's.
    pub backing: &'a [PhysFrameRange],
}

/// Where a submission's command stream is.
#[derive(Clone, Copy, Debug)]
pub enum CommandStream<'a> {
    /// A physical run the caller owns and keeps owning until the
    /// submission is answered: the device reads the bytes where the
    /// guest wrote them.
    Pages(PhysicalRange),
    /// A short stream held in the caller's own memory for as long as
    /// the submission future lives. The driver publishes the slice as
    /// it is; nothing is copied into a buffer of the driver's.
    Inline(&'a [u8]),
}

impl CommandStream<'_> {
    /// How many bytes the stream carries.
    pub const fn bytes(&self) -> u64 {
        match self {
            Self::Pages(range) => range.bytes,
            Self::Inline(bytes) => bytes.len() as u64,
        }
    }
}

/// One command stream for one context's renderer.
#[derive(Clone, Copy, Debug)]
pub struct Submission<'a> {
    /// The context whose renderer reads the stream.
    pub context: ContextId,
    /// The bytes, in the renderer's own command format.
    pub commands: CommandStream<'a>,
    /// The timeline the fence, if there is one, is a point on.
    pub ring: RingIndex,
    /// The point the device signals once it has consumed this stream and
    /// everything before it on `ring`; `None` for a stream nobody waits
    /// on, which the device answers as soon as it has processed it.
    pub fence: Option<FenceId>,
}

/// Which of a display engine's tables a [`Gpu3dError::TooMany`] is
/// about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gpu3dTable {
    /// Capability sets the device carries.
    Capsets,
    /// Renderer contexts the driver tracks.
    Contexts,
    /// Blob resources the driver tracks.
    Blobs,
}

impl core::fmt::Display for Gpu3dTable {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(match self {
            Self::Capsets => "capability sets",
            Self::Contexts => "contexts",
            Self::Blobs => "blobs",
        })
    }
}

/// Why a 3D operation did not happen.
///
/// Kept apart from [`super::DisplayError`] rather than folded into it:
/// the 3D half of a display engine refuses for reasons the 2D half has
/// no vocabulary for — a context type the host does not speak, a blob
/// the aperture has no room for — and a caller that has to tell them
/// apart cannot do it from a code that means "invalid parameter".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum Gpu3dError {
    /// This display engine renders nothing: it negotiated no 3D
    /// feature, so it carries no capability set and holds no context.
    #[error("this display engine offers no 3D support")]
    Unsupported,
    /// The device carries no capability set with that identifier, or
    /// not at that version.
    #[error("this display engine carries no capability set {} at version {version}", .id.raw())]
    UnknownCapset { id: CapsetId, version: u32 },
    /// No such context on this device.
    #[error("context {} does not exist on this device", .0.raw())]
    UnknownContext(ContextId),
    /// The context was created with fewer rings than the one named.
    #[error("context {} carries no ring {}", .context.raw(), .ring.raw())]
    UnknownRing { context: ContextId, ring: RingIndex },
    /// No such blob on this device.
    #[error("blob {} does not exist on this device", .0.raw())]
    UnknownBlob(BlobId),
    /// The device holds as many contexts, or as many blobs, as the
    /// driver has room to track.
    #[error("the display engine already holds its maximum of {limit} {table}")]
    TooMany { table: Gpu3dTable, limit: usize },
    /// The blob's parameters do not describe a resource: no bytes, a
    /// host-3D blob with guest pages, a guest blob without them or with
    /// a renderer blob id, or a size the pages do not cover once
    /// rounded to the device's alignment.
    #[error("the blob request does not describe a resource this engine can create")]
    InvalidBlob,
    /// The blob was not created mappable, or is already mapped, or is
    /// not mapped and was asked to be unmapped.
    #[error("blob {} cannot be mapped in its current state", .0.raw())]
    NotMappable(BlobId),
    /// The host answered a mapping with a caching mode virtio-gpu
    /// defines no meaning for, which is a device fault rather than a
    /// refusal. The placement is taken back before this is returned.
    #[error("the host answered a blob mapping with the undefined caching mode {map_info:#x}")]
    UnknownCaching { map_info: u32 },
    /// The engine's host-visible aperture has no room left for another
    /// mapping. This is the host's window, not the guest's memory: a
    /// caller unmaps a blob it has finished with.
    #[error("the display engine's host-visible aperture has no room left")]
    ApertureExhausted,
    /// The device answered a request with a code that does not belong
    /// to it, which is a device fault rather than a refusal.
    #[error("the display engine answered with the unexpected code {code:#x}")]
    UnexpectedResponse { code: u32 },
    /// The device refused the request without naming a reason.
    #[error("the display engine refused the request without naming a reason")]
    Unspecified,
    /// The device could not find room for the resource.
    #[error("the display engine is out of memory")]
    OutOfMemory,
    /// The device rejected one of the request's parameters.
    #[error("the display engine rejected a request parameter")]
    InvalidParameter,
    /// The backing store is built from more ranges than one request
    /// carries.
    #[error("a blob's backing may span at most {limit} ranges, not {ranges}")]
    TooManyBackingRanges { ranges: usize, limit: usize },
    /// A command stream of no bytes, or longer than one submission
    /// carries.
    #[error("a command stream of {bytes} bytes is not one this engine can submit")]
    CommandBufferLength { bytes: u64 },
    /// The transport underneath the display engine failed.
    #[error("display transport: {0}")]
    Transport(#[from] IoError),
}

impl Gpu3dError {
    /// The device's own response code, for the refusals that carry one,
    /// so a caller that records a refusal can name what the device said.
    pub const fn device_code(&self) -> Option<u32> {
        match self {
            Self::UnexpectedResponse { code } => Some(*code),
            Self::UnknownCaching { map_info } => Some(*map_info),
            _ => None,
        }
    }
}

pub type Gpu3dResult<T> = Result<T, Gpu3dError>;

/// The memory type a mapped blob's caching mode calls for.
///
/// A host renderer decides per allocation how the guest must reach its
/// storage, and the mapping carries that decision rather than one the
/// kernel chose: a write-combining or uncached answer names a coherency
/// contract the renderer relies on, and reading the blob under another
/// one would be a disagreement nobody can see.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobCaching {
    /// Ordinary cacheable memory.
    Cached,
    /// Memory every access must reach uncached.
    Uncached,
    /// Memory stores reach through a write-combining buffer.
    WriteCombining,
}

impl BlobCaching {
    /// The memory kind a mapping of this caching mode is shown with.
    pub const fn memory_kind(self) -> MemoryKind {
        match self {
            Self::Cached => MemoryKind::Normal,
            Self::Uncached => MemoryKind::Device,
            Self::WriteCombining => MemoryKind::WriteCombining,
        }
    }
}

/// The rendering half of a display engine.
///
/// A second trait beside [`super::DisplayDevice`] rather than more
/// methods on it, because they are two capabilities of one piece of
/// hardware and a machine may have the first without the second: every
/// virtio-gpu drives scanouts, and only a host built against a renderer
/// carries a capability set. A caller asks [`Gpu3d::capsets`] which it
/// has, and a device that answers with nothing renders nothing.
pub trait Gpu3d: Send + Sync + 'static {
    /// Whether this display engine renders at all.
    ///
    /// A hardware fact the device states about itself and the one
    /// question here that costs nothing to ask: an engine that answers
    /// `false` carries no capability set and holds no context, and
    /// every other method refuses with [`Gpu3dError::Unsupported`].
    /// It is what a machine decides whether to publish a 3D service on.
    fn renders(&self) -> bool;

    /// The capability sets this device carries, in its own order.
    ///
    /// A capability set is a property of the host's renderer rather
    /// than of anything a guest has done, so the answer does not change
    /// while the machine runs and a caller reads it once. Empty on
    /// every engine that renders nothing.
    fn capsets(&self) -> impl Future<Output = Gpu3dResult<CapsetList>> + Send + '_;

    /// Writes the bytes of one capability set, at `version`, into
    /// `out`, and reports how many.
    ///
    /// The bytes are the renderer's, opaque here and opaque in the
    /// kernel: they reach the guest that asked for them unchanged.
    /// `out` should be [`CapsetInfo::max_size`] bytes for the
    /// capability set named, which is the one figure
    /// [`Gpu3d::capsets`] reports so that a caller can size it; a
    /// shorter buffer is filled as far as it goes and the answer says
    /// how far, because the device writes what the chain it was given
    /// has room for.
    fn capset<'a>(
        &'a self,
        id: CapsetId,
        version: u32,
        out: &'a mut [u8],
    ) -> impl Future<Output = Gpu3dResult<usize>> + Send + 'a;

    /// Creates a renderer context of the type `capset` names, carrying
    /// `rings` fence timelines.
    ///
    /// `name` is what the host renderer prints when this context
    /// faults, and is the only thing here a guest chooses that the host
    /// ever reads as text.
    fn create_context(
        &self,
        capset: CapsetId,
        rings: RingCount,
        name: ContextName,
    ) -> impl Future<Output = Gpu3dResult<ContextId>> + Send + '_;

    /// Releases a context and everything the renderer held for it.
    fn destroy_context(
        &self,
        context: ContextId,
    ) -> impl Future<Output = Gpu3dResult<()>> + Send + '_;

    /// Creates a blob resource.
    ///
    /// The pages in [`BlobRequest::backing`], where there are any, stay
    /// the caller's: the device only reads and writes them, and only
    /// between this call and [`Gpu3d::destroy_blob`]. The device is not
    /// told to attach the blob to its context's command stream; that is
    /// [`Gpu3d::attach_resource`], for the caller to ask for.
    ///
    /// [`Gpu3dError::Transport`] means the request may have reached the
    /// device: it may hold the blob and every page of the backing, and
    /// the caller has no handle to release it by.
    fn create_blob<'a>(
        &'a self,
        request: BlobRequest<'a>,
    ) -> impl Future<Output = Gpu3dResult<BlobId>> + Send + 'a;

    /// Releases a blob and hands any backing pages back.
    ///
    /// The pages are the caller's again only when this resolves `Ok`.
    /// Any error leaves the device free to keep reading and writing
    /// them, and [`Gpu3dError::Transport`] in particular means the
    /// request may never have reached it.
    fn destroy_blob(&self, blob: BlobId) -> impl Future<Output = Gpu3dResult<()>> + Send + '_;

    /// Places a blob's host storage in the engine's host-visible
    /// aperture and reports where.
    ///
    /// The region is physical memory of the machine — the engine's own
    /// aperture, not the caller's pages — so it is a [`DeviceRegion`]
    /// and it is the caller's business to map it wherever its owner can
    /// reach it. Its memory kind is the one the renderer's caching
    /// answer calls for ([`BlobCaching::memory_kind`]): a renderer's
    /// buffer is ordinary memory unless the renderer said it must be
    /// reached write-combining or uncached.
    fn map_blob(&self, blob: BlobId)
    -> impl Future<Output = Gpu3dResult<DeviceRegion>> + Send + '_;

    /// Takes a blob back out of the aperture.
    ///
    /// On return the region [`Gpu3d::map_blob`] reported names nothing,
    /// so a caller unmaps it from its owner's memory first.
    fn unmap_blob(&self, blob: BlobId) -> impl Future<Output = Gpu3dResult<()>> + Send + '_;

    /// Makes `blob` reachable from `context`'s command stream.
    fn attach_resource(
        &self,
        context: ContextId,
        blob: BlobId,
    ) -> impl Future<Output = Gpu3dResult<()>> + Send + '_;

    /// Takes `blob` back out of `context`'s command stream.
    fn detach_resource(
        &self,
        context: ContextId,
        blob: BlobId,
    ) -> impl Future<Output = Gpu3dResult<()>> + Send + '_;

    /// Hands a command stream to its context's renderer.
    ///
    /// The bytes are the renderer's own command format and are never
    /// read here — nor copied. A [`CommandStream::Pages`] stream is a
    /// physical run the caller owns and keeps owning, exactly as
    /// [`BlobRequest::backing`] is, because the buffer a guest wrote is
    /// the buffer the device reads; a [`CommandStream::Inline`] one is
    /// published from where the caller holds it.
    ///
    /// Two moments matter, and the call reports both. `posted` runs
    /// once, as soon as the device's queue holds the command; it never
    /// runs for a call refused before that. The future then resolves
    /// with the device's answer, which for a fenced submission comes
    /// only once `fence` has passed on `ring`: the renderer has finished
    /// reading the stream by then, and a caller may release its pages
    /// on any answer other than [`Gpu3dError::Transport`]. The fence is
    /// retired on [`Gpu3d::fences`] with the answer, whatever the answer
    /// was, so a reader of the ring never waits for a point the device
    /// has decided will not come.
    ///
    /// [`Gpu3dError::Transport`] means no answer was obtained: the
    /// device may still hold the stream and may read it at any later
    /// time, so the caller must never release its pages.
    fn submit<'a, Posted>(
        &'a self,
        submission: Submission<'a>,
        posted: &'a mut Posted,
    ) -> impl Future<Output = Gpu3dResult<()>> + Send + 'a
    where
        Posted: FnMut() + Send;

    /// The newest fence `context` has signalled on `ring` past `after`.
    ///
    /// This is one ring's completion stream, read one point at a time:
    /// the timeline is monotone, so an implementation that has retired
    /// several fences since the reader last asked hands back the newest
    /// of them rather than every one between — the newest answers for
    /// all that came before it. A reader that has seen none passes
    /// [`FenceId::START`].
    fn fences(
        &self,
        context: ContextId,
        ring: RingIndex,
        after: FenceId,
    ) -> impl Future<Output = Gpu3dResult<FenceId>> + Send + '_;
}

/// A shared 3D engine is a 3D engine.
///
/// A backend hands the same driver to two owners at once — the kernel
/// task that owns the display and the one that owns the renderer — so
/// the shared handle satisfies the contract without every backend
/// writing the same forwarding methods, exactly as
/// [`super::DisplayDevice`]'s shared impl does.
impl<Device: Gpu3d + ?Sized> Gpu3d for alloc::sync::Arc<Device> {
    fn renders(&self) -> bool {
        Device::renders(self)
    }

    fn capsets(&self) -> impl Future<Output = Gpu3dResult<CapsetList>> + Send + '_ {
        Device::capsets(self)
    }

    fn capset<'a>(
        &'a self,
        id: CapsetId,
        version: u32,
        out: &'a mut [u8],
    ) -> impl Future<Output = Gpu3dResult<usize>> + Send + 'a {
        Device::capset(self, id, version, out)
    }

    fn create_context(
        &self,
        capset: CapsetId,
        rings: RingCount,
        name: ContextName,
    ) -> impl Future<Output = Gpu3dResult<ContextId>> + Send + '_ {
        Device::create_context(self, capset, rings, name)
    }

    fn destroy_context(
        &self,
        context: ContextId,
    ) -> impl Future<Output = Gpu3dResult<()>> + Send + '_ {
        Device::destroy_context(self, context)
    }

    fn create_blob<'a>(
        &'a self,
        request: BlobRequest<'a>,
    ) -> impl Future<Output = Gpu3dResult<BlobId>> + Send + 'a {
        Device::create_blob(self, request)
    }

    fn destroy_blob(&self, blob: BlobId) -> impl Future<Output = Gpu3dResult<()>> + Send + '_ {
        Device::destroy_blob(self, blob)
    }

    fn map_blob(
        &self,
        blob: BlobId,
    ) -> impl Future<Output = Gpu3dResult<DeviceRegion>> + Send + '_ {
        Device::map_blob(self, blob)
    }

    fn unmap_blob(&self, blob: BlobId) -> impl Future<Output = Gpu3dResult<()>> + Send + '_ {
        Device::unmap_blob(self, blob)
    }

    fn attach_resource(
        &self,
        context: ContextId,
        blob: BlobId,
    ) -> impl Future<Output = Gpu3dResult<()>> + Send + '_ {
        Device::attach_resource(self, context, blob)
    }

    fn detach_resource(
        &self,
        context: ContextId,
        blob: BlobId,
    ) -> impl Future<Output = Gpu3dResult<()>> + Send + '_ {
        Device::detach_resource(self, context, blob)
    }

    fn submit<'a, Posted>(
        &'a self,
        submission: Submission<'a>,
        posted: &'a mut Posted,
    ) -> impl Future<Output = Gpu3dResult<()>> + Send + 'a
    where
        Posted: FnMut() + Send,
    {
        Device::submit(self, submission, posted)
    }

    fn fences(
        &self,
        context: ContextId,
        ring: RingIndex,
        after: FenceId,
    ) -> impl Future<Output = Gpu3dResult<FenceId>> + Send + '_ {
        Device::fences(self, context, ring, after)
    }
}

impl BlobRequest<'_> {
    /// Whether the request describes a resource at all.
    ///
    /// Checked by the driver before a single byte reaches the wire, so
    /// that a blob whose parameters contradict each other is refused
    /// where the caller can still be told which of them did it, rather
    /// than as a bare `INVALID_PARAMETER` from the device.
    pub fn is_coherent(&self) -> bool {
        if self.size == 0 {
            return false;
        }
        match self.memory {
            // Host storage the renderer allocated. Pages of the
            // caller's would be memory the device is told about twice.
            BlobMemory::Host3d => self.backing.is_empty(),
            // The caller's pages *are* the resource, so there have to
            // be some; and a renderer blob id names host storage this
            // blob does not have (Linux refuses the same in
            // `virtgpu_ioctl.c`'s `verify_blob`).
            BlobMemory::Guest => {
                self.blob_id == 0
                    && !self.backing.is_empty()
                    && self.backing.len() <= MAX_BACKING_RANGES
            }
            BlobMemory::Host3dGuest => {
                !self.backing.is_empty() && self.backing.len() <= MAX_BACKING_RANGES
            }
        }
    }

    /// How many bytes the caller's pages cover.
    pub fn backing_bytes(&self) -> u64 {
        self.backing
            .iter()
            .map(|range| range.byte_size() as u64)
            .sum()
    }
}

/// How a blob's bytes are read as one scanout's frame.
///
/// A blob carries no geometry of its own — it is a run of bytes the
/// renderer filled — so the layout is named when it is scanned out:
/// the frame's mode and pixel format, where its first row starts, and
/// how far apart rows are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlobScanout {
    /// The blob whose bytes are the frame.
    pub blob: BlobId,
    /// The frame's width and height in pixels.
    pub mode: super::DisplayMode,
    /// How each pixel's bytes are read.
    pub format: PixelFormat,
    /// Where the first row starts, in bytes from the blob's start.
    pub offset: u32,
    /// Bytes from the start of one row to the start of the next.
    pub stride: u32,
}

impl BlobScanout {
    /// Whether the layout describes a frame inside a blob of `bytes`:
    /// rows at least as wide as the mode, and `height` whole strides
    /// from the offset ending inside the blob.
    ///
    /// Whole strides rather than a last row cut short at the mode's
    /// width, because that is the bound the device checks
    /// (`virtio_gpu_scanout_blob_to_fb` in QEMU's `hw/display/virtio-gpu.c`
    /// refuses `offset + stride * height > blob_size`), and a layout
    /// passed here that the device then refused would answer with an
    /// unspecified error instead of this one.
    pub fn fits_in(&self, bytes: u64) -> bool {
        let row = u64::from(self.mode.width) * self.format.bytes_per_pixel() as u64;
        if self.mode.width == 0 || self.mode.height == 0 || u64::from(self.stride) < row {
            return false;
        }
        let end = u64::from(self.offset) + u64::from(self.stride) * u64::from(self.mode.height);
        end <= bytes
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BlobCaching, BlobMemory, BlobRequest, BlobScanout, BlobUsage, CapsetId, ContextId, FenceId,
        MAX_RINGS, RingCount, RingIndex,
    };
    use crate::device::MemoryKind;
    use crate::display::{DisplayMode, PixelFormat};
    use crate::pmm::{PhysFrame, PhysFrameRange};

    fn pages() -> [PhysFrameRange; 1] {
        [PhysFrameRange {
            start: PhysFrame::from_phys_addr(0x4000_0000),
            frame_count: 4,
        }]
    }

    fn request<'a>(memory: BlobMemory, backing: &'a [PhysFrameRange]) -> BlobRequest<'a> {
        BlobRequest {
            context: ContextId::new(1),
            memory,
            usage: BlobUsage::MAPPABLE,
            size: 4096,
            blob_id: 0,
            backing,
        }
    }

    /// The three blob kinds differ in exactly one thing — whose memory
    /// the resource is — so a request that names the wrong one is
    /// refused before it reaches the wire.
    #[test]
    fn a_blob_names_the_memory_its_kind_requires() {
        assert!(request(BlobMemory::Host3d, &[]).is_coherent());
        assert!(!request(BlobMemory::Host3d, &pages()).is_coherent());
        assert!(request(BlobMemory::Guest, &pages()).is_coherent());
        assert!(!request(BlobMemory::Guest, &[]).is_coherent());
        assert!(request(BlobMemory::Host3dGuest, &pages()).is_coherent());
        assert!(!request(BlobMemory::Host3dGuest, &[]).is_coherent());
    }

    /// A renderer blob id names host storage; a guest blob has none, so
    /// one that carries an id is refused, while the two kinds with host
    /// storage carry theirs.
    #[test]
    fn only_a_blob_with_host_storage_carries_a_renderer_blob_id() {
        let backing = pages();
        let mut guest = request(BlobMemory::Guest, &backing);
        guest.blob_id = 9;
        assert!(!guest.is_coherent());
        let mut host = request(BlobMemory::Host3d, &[]);
        host.blob_id = 9;
        assert!(host.is_coherent());
        let mut both = request(BlobMemory::Host3dGuest, &backing);
        both.blob_id = 9;
        assert!(both.is_coherent());
    }

    #[test]
    fn a_blob_of_no_bytes_is_not_a_resource() {
        let backing = pages();
        let mut empty = request(BlobMemory::Guest, &backing);
        empty.size = 0;
        assert!(!empty.is_coherent());
    }

    /// The identifiers the virtio-gpu specification assigns, so that a
    /// caller selecting venus is reading a name rather than counting.
    #[test]
    fn the_named_capsets_are_the_numbers_the_specification_assigns() {
        assert_eq!(CapsetId::VIRGL.raw(), 1);
        assert_eq!(CapsetId::VIRGL2.raw(), 2);
        assert_eq!(CapsetId::GFXSTREAM_VULKAN.raw(), 3);
        assert_eq!(CapsetId::VENUS.raw(), 4);
        assert_eq!(CapsetId::CROSS_DOMAIN.raw(), 5);
        assert_eq!(CapsetId::DRM.raw(), 6);
    }

    /// A reader that has seen nothing asks for the fence after the
    /// start, and no device ever signals that one.
    #[test]
    fn the_fence_stream_starts_before_every_fence() {
        assert_eq!(FenceId::START.raw(), 0);
        assert!(FenceId::new(1) > FenceId::START);
    }

    /// A context carries one to sixty-four rings, and a ring is one of
    /// them only when its index is below the count.
    #[test]
    fn a_ring_belongs_to_a_context_only_below_its_count() {
        assert!(RingCount::new(0).is_none());
        assert!(RingCount::new(MAX_RINGS as u8 + 1).is_none());
        assert!(RingIndex::new(MAX_RINGS as u8).is_none());
        let two = RingCount::new(2).expect("two rings is a count");
        assert!(two.contains(RingIndex::CPU));
        assert!(two.contains(RingIndex::new(1).expect("an index")));
        assert!(!two.contains(RingIndex::new(2).expect("an index")));
        assert_eq!(two.rings().count(), 2);
    }

    /// The renderer's caching answer picks the memory kind the mapping
    /// is shown with, uncached being device memory.
    #[test]
    fn a_caching_answer_picks_the_mapping_s_memory_kind() {
        assert_eq!(BlobCaching::Cached.memory_kind(), MemoryKind::Normal);
        assert_eq!(BlobCaching::Uncached.memory_kind(), MemoryKind::Device);
        assert_eq!(
            BlobCaching::WriteCombining.memory_kind(),
            MemoryKind::WriteCombining
        );
    }

    /// A scanout layout fits a blob only when every row is at least the
    /// mode's width and `height` whole strides end inside it.
    #[test]
    fn a_scanout_layout_fits_only_inside_its_blob() {
        let layout = BlobScanout {
            blob: super::BlobId::new(3),
            mode: DisplayMode::new(4, 2),
            format: PixelFormat::Bgrx8888,
            offset: 0,
            stride: 16,
        };
        assert!(layout.fits_in(32));
        assert!(!layout.fits_in(31));
        assert!(
            !BlobScanout {
                stride: 15,
                ..layout
            }
            .fits_in(64)
        );
        assert!(
            BlobScanout {
                offset: 8,
                stride: 20,
                ..layout
            }
            .fits_in(48)
        );
        assert!(
            !BlobScanout {
                offset: 8,
                stride: 20,
                ..layout
            }
            .fits_in(47)
        );
    }
}
