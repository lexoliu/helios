//! The 3D contract of a display engine.
//!
//! A display engine that can render owns a second set of hardware facts
//! beside its scanouts: a set of *capability sets*, each naming a
//! renderer the host is willing to speak; *contexts*, which are that
//! renderer's per-client state; *blobs*, which are the resources a
//! context works on; and *fences*, which are how the device says a
//! submission has been consumed. None of it is a consumer's vocabulary
//! — a Vulkan driver, a compositor, a media stack all use the same four
//! — so the contract lives here and the concrete driver encodes it onto
//! its own wire format.
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
//!   for.
//! * [`BlobMemory::Host3dGuest`] — both: host storage the renderer
//!   works on, backed by pages the caller owns.
//!
//! # SMP contract
//!
//! Every method takes `&self` and may be called from any processor and
//! from several tasks at once; an implementation serialises access to
//! its own rings internally. Ordering between two concurrent calls is
//! the ordering the device sees, so a caller that needs one submission
//! to precede another awaits the first, or fences them.
//!
//! Fences are the exception, and the reason they exist: they are the
//! one ordering guarantee this contract makes without a caller waiting.
//! Every fence a context signals is delivered in increasing order on
//! that context, so a reader that asks for "the first fence after `n`"
//! never misses one and never sees one twice.

use core::future::Future;

use arrayvec::{ArrayString, ArrayVec};
use thiserror::Error;

use crate::device::DeviceRegion;
use crate::io::IoError;
use crate::iommu::PhysicalRange;
use crate::pmm::PhysFrameRange;

use super::MAX_BACKING_RANGES;

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
/// per-client object table, its command timeline, and the resources
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

/// One point on a context's completion timeline.
///
/// A caller chooses the number and the device hands it back once every
/// command submitted before it has been consumed. Increasing per
/// context, which is the whole of the ordering [`Gpu3d::fences`] rests
/// on.
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
    /// How many bytes it covers.
    pub size: u64,
    /// The renderer's own handle for the host allocation, for
    /// [`BlobMemory::Host3d`] and [`BlobMemory::Host3dGuest`]. It comes
    /// out of the command stream the guest submitted, so the kernel
    /// carries it and never mints one.
    pub host_id: u64,
    /// The caller's pages, for [`BlobMemory::Guest`] and
    /// [`BlobMemory::Host3dGuest`]. Empty for [`BlobMemory::Host3d`],
    /// whose storage is the host's.
    pub backing: &'a [PhysFrameRange],
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
    /// No such blob on this device.
    #[error("blob {} does not exist on this device", .0.raw())]
    UnknownBlob(BlobId),
    /// The device holds as many contexts, or as many blobs, as it has
    /// room to track.
    #[error("the display engine already holds its maximum of {limit} {resource}")]
    TooMany {
        resource: &'static str,
        limit: usize,
    },
    /// The blob's parameters do not describe a resource: no bytes, a
    /// host-3D blob with guest pages, a guest blob without them.
    #[error("the blob request does not describe a resource this engine can create")]
    InvalidBlob,
    /// The blob was not created mappable, or is already mapped, or is
    /// not mapped and was asked to be unmapped.
    #[error("blob {} cannot be mapped in its current state", .0.raw())]
    NotMappable(BlobId),
    /// The host requires a mapped blob to be accessed in a way the
    /// kernel does not map the aperture with.
    ///
    /// The aperture is ordinary memory in every address space Helios
    /// builds. A host that answers a mapping with "uncached" or
    /// "write-combining" is naming a coherency contract the guest would
    /// then be reading the blob under a different one from, and a
    /// silent disagreement about coherency is a corruption nobody can
    /// see, so the mapping is refused with the host's own answer in it.
    #[error(
        "the host requires a mapped blob to be accessed as {map_info:#x}, which is not how this kernel maps its aperture"
    )]
    UnsupportedCaching { map_info: u32 },
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
    /// A command buffer of no bytes, or longer than one submission
    /// carries.
    #[error("a command buffer of {bytes} bytes is not one this engine can submit")]
    CommandBufferLength { bytes: u64 },
    /// The transport underneath the display engine failed.
    #[error("display transport: {0}")]
    Transport(#[from] IoError),
}

pub type Gpu3dResult<T> = Result<T, Gpu3dError>;

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

    /// Writes the bytes of one capability set into `out`, and reports
    /// how many.
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

    /// Creates a renderer context of the type `capset` names.
    ///
    /// `name` is what the host renderer prints when this context
    /// faults, and is the only thing here a guest chooses that the host
    /// ever reads as text.
    fn create_context(
        &self,
        capset: CapsetId,
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
    /// between this call and [`Gpu3d::destroy_blob`].
    fn create_blob<'a>(
        &'a self,
        request: BlobRequest<'a>,
    ) -> impl Future<Output = Gpu3dResult<BlobId>> + Send + 'a;

    /// Releases a blob and hands any backing pages back.
    fn destroy_blob(&self, blob: BlobId) -> impl Future<Output = Gpu3dResult<()>> + Send + '_;

    /// Places a blob's host storage in the engine's host-visible
    /// aperture and reports where.
    ///
    /// The region is physical memory of the machine — the engine's own
    /// aperture, not the caller's pages — so it is a [`DeviceRegion`]
    /// and it is the caller's business to map it wherever its owner can
    /// reach it. Its memory kind is normal rather than device: the
    /// bytes behind it are a renderer's buffer, with no read side
    /// effects and nothing that ordering rules have to be relaxed for.
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

    /// Hands the command buffer at `commands` to `context`'s renderer,
    /// fenced at `fence`.
    ///
    /// The bytes are the renderer's own command format and are never
    /// read here — nor copied. `commands` is a physical run the caller
    /// owns and keeps owning, exactly as [`BlobRequest::backing`] is,
    /// because the buffer a guest wrote is the buffer the device reads:
    /// a display engine takes its command stream by address, and an
    /// implementation that took a slice would be one that had already
    /// copied a guest's pages into its own.
    ///
    /// The future resolves when the device has taken the buffer, which
    /// is not when the renderer has finished with it: that is what
    /// `fence` is for, and it is delivered on [`Gpu3d::fences`]. The
    /// caller keeps the pages until then.
    ///
    /// A submission the device refuses still signals its fence, so a
    /// reader of the fence stream never waits for a point the device
    /// has decided will not come.
    fn submit(
        &self,
        context: ContextId,
        commands: PhysicalRange,
        fence: FenceId,
    ) -> impl Future<Output = Gpu3dResult<()>> + Send + '_;

    /// The first fence `context` signals after `after`.
    ///
    /// This is the context's completion stream, read one point at a
    /// time: fences on one context are signalled in increasing order,
    /// so a reader that passes back what it last saw sees every fence
    /// once and in order however long it was away. A reader that has
    /// seen none passes [`FenceId::START`].
    fn fences(
        &self,
        context: ContextId,
        after: FenceId,
    ) -> impl Future<Output = Gpu3dResult<FenceId>> + Send + '_;
}

/// A shared 3D engine is a 3D engine.
///
/// A backend hands the same driver to two owners at once — the kernel
/// task that owns the display and the one that owns the renderer — so
/// the shared handle satisfies the contract without every backend
/// writing the same eleven forwarding methods, exactly as
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
        name: ContextName,
    ) -> impl Future<Output = Gpu3dResult<ContextId>> + Send + '_ {
        Device::create_context(self, capset, name)
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

    fn submit(
        &self,
        context: ContextId,
        commands: PhysicalRange,
        fence: FenceId,
    ) -> impl Future<Output = Gpu3dResult<()>> + Send + '_ {
        Device::submit(self, context, commands, fence)
    }

    fn fences(
        &self,
        context: ContextId,
        after: FenceId,
    ) -> impl Future<Output = Gpu3dResult<FenceId>> + Send + '_ {
        Device::fences(self, context, after)
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
            // be some.
            BlobMemory::Guest | BlobMemory::Host3dGuest => {
                !self.backing.is_empty() && self.backing.len() <= MAX_BACKING_RANGES
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BlobMemory, BlobRequest, BlobUsage, CapsetId, ContextId, FenceId};
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
            host_id: 7,
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
}
