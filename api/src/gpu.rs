//! The machine's 3D engine, and the command buffers a program renders
//! through.
//!
//! Exactly one program holds the engine at a time. [`Gpu::claim`] takes
//! it; dropping what that returns — or dying — hands every context and
//! blob back.
//!
//! Nothing the renderer speaks goes through a call. A command buffer is
//! whole pages of this program's own heap that the kernel backs with
//! pinned frames for as long as the buffer lives, so
//! [`CommandBuffer::bytes`] is an ordinary mutable slice and filling it
//! is writing to it. [`Context::submit`] then costs one round trip and
//! copies nothing: the display engine reads the same bytes, and the
//! bytes themselves are the renderer's protocol — virgl's, Venus's,
//! gfxstream's — which this contract carries and never interprets.
//!
//! Completion is a fence stream, not a return value: `submit` resolves
//! when the device has taken the buffer, and [`Context::fences`]
//! carries the fence itself when the device has finished the work.

use std::vec::Vec;

use thiserror::Error;

use crate::bindings::helios::system::gpu as raw;
use crate::memory::{Backed, Block, BlockError};
use crate::wit_bindgen::StreamReader;

pub use crate::bindings::helios::system::gpu::{BlobMemory, BlobUsage, CapsetInfo};
pub use crate::memory::Span;

/// Why a 3D request was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum GpuError {
    #[error("this machine has no display device")]
    Unavailable,
    #[error("this machine's display engine offers no 3D support")]
    NoRenderer,
    #[error("another program already holds the 3D engine")]
    AlreadyClaimed,
    #[error("this program does not hold the 3D engine")]
    NotClaimed,
    #[error("this display engine carries no such capability set")]
    NoSuchCapset,
    #[error("the host renderer does not speak this context type")]
    UnsupportedContext,
    #[error("this claim already holds as many contexts as it may")]
    TooManyContexts,
    #[error("this claim already holds as many blobs as it may")]
    TooManyBlobs,
    #[error("this claim already shows as many buffers in this instance's memory as it may")]
    TooManyViews,
    #[error("the blob request does not describe a resource this engine can create")]
    InvalidBlob,
    #[error("the display engine's host-visible aperture has no room left")]
    ApertureExhausted,
    #[error("the submission does not lie inside the command buffer")]
    OutOfBounds,
    #[error("a fence must come after every fence this context has already submitted")]
    StaleFence,
    #[error("the pages are not a whole-page run of this program's memory, or do not suit the blob")]
    BadSpan,
    #[error("the blob's host storage is already shown in this program's memory")]
    AlreadyMapped,
    #[error("the blob's host storage is not shown in this program's memory")]
    NotMapped,
    #[error("no contiguous run of memory left for a command buffer")]
    OutOfMemory,
    #[error("the display engine faulted")]
    DeviceFault,
    #[error("this program's heap cannot supply the pages: {0}")]
    Heap(#[from] BlockError),
}

impl From<raw::Error> for GpuError {
    fn from(error: raw::Error) -> Self {
        match error {
            raw::Error::Unavailable => Self::Unavailable,
            raw::Error::NoRenderer => Self::NoRenderer,
            raw::Error::AlreadyClaimed => Self::AlreadyClaimed,
            raw::Error::NotClaimed => Self::NotClaimed,
            raw::Error::NoSuchCapset => Self::NoSuchCapset,
            raw::Error::UnsupportedContext => Self::UnsupportedContext,
            raw::Error::TooManyContexts => Self::TooManyContexts,
            raw::Error::TooManyBlobs => Self::TooManyBlobs,
            raw::Error::TooManyViews => Self::TooManyViews,
            raw::Error::InvalidBlob => Self::InvalidBlob,
            raw::Error::ApertureExhausted => Self::ApertureExhausted,
            raw::Error::OutOfBounds => Self::OutOfBounds,
            raw::Error::StaleFence => Self::StaleFence,
            raw::Error::BadSpan => Self::BadSpan,
            raw::Error::AlreadyMapped => Self::AlreadyMapped,
            raw::Error::NotMapped => Self::NotMapped,
            raw::Error::OutOfMemory => Self::OutOfMemory,
            raw::Error::DeviceFault => Self::DeviceFault,
        }
    }
}

/// This program's hold on the machine's 3D engine.
pub struct Gpu {
    raw: raw::Gpu,
}

impl Gpu {
    /// Take exclusive ownership of the machine's 3D engine.
    ///
    /// The second caller is refused rather than queued, and a machine
    /// whose display engine renders nothing answers `NoRenderer` rather
    /// than handing out a claim that cannot draw.
    pub fn claim() -> Result<Self, GpuError> {
        raw::claim().map(|raw| Self { raw }).map_err(GpuError::from)
    }

    /// Every capability set the device carries, in its own order.
    pub async fn capsets(&self) -> Result<Vec<CapsetInfo>, GpuError> {
        self.raw.capsets().await.map_err(GpuError::from)
    }

    /// The bytes of one capability set, at the newest version the host
    /// speaks.
    ///
    /// The payload is the renderer's own encoding, carried whole: what
    /// a Venus host reports is not what a virglrenderer host reports,
    /// and this contract does not grow when a renderer does.
    pub async fn capset(&self, id: u32) -> Result<Vec<u8>, GpuError> {
        self.raw.capset(id).await.map_err(GpuError::from)
    }

    /// Open a renderer context of the type `capset` names.
    ///
    /// The capset identifier selects which renderer the context speaks;
    /// `name` is the debugging label the device carries and may be
    /// empty.
    pub async fn create_context(&self, capset: u32, name: &str) -> Result<Context, GpuError> {
        self.raw
            .create_context(capset, name.to_string())
            .await
            .map(|raw| Context { raw })
            .map_err(GpuError::from)
    }

    /// Create a blob resource of `size` bytes owned by `context`.
    ///
    /// `memory` says where its storage lives and `usage` what it may be
    /// used for; `host_id` lets the host share the resource with other
    /// guests and is `0` when it does not.
    ///
    /// A blob with pages of its own ([`BlobMemory::Guest`],
    /// [`BlobMemory::Host3dGuest`]) takes them from this program's heap,
    /// sized to `size`, and they start zeroed.
    pub async fn create_blob(
        &self,
        context: &Context,
        memory: BlobMemory,
        usage: BlobUsage,
        size: u64,
        host_id: u64,
    ) -> Result<Blob, GpuError> {
        let pages = match memory {
            BlobMemory::Guest | BlobMemory::Host3dGuest => Some(Block::zeroed(size)?),
            BlobMemory::Host3d => None,
        };
        let raw = self
            .raw
            .create_blob(
                &context.raw,
                memory,
                usage,
                size,
                host_id,
                pages.as_ref().map(Block::span),
            )
            .await
            .map_err(GpuError::from)?;
        Ok(Blob {
            raw,
            mapping: None,
            pages,
            size,
        })
    }
}

/// One renderer context.
pub struct Context {
    raw: raw::Context,
}

impl Context {
    /// Pin `bytes` of this program's memory as a command buffer.
    ///
    /// The kernel pins every page of the span, so the buffer is `bytes`
    /// rounded up to whole wasm pages ([`CommandBuffer::len`]); its
    /// pages come from this program's heap and start zeroed.
    pub async fn commands(&self, bytes: u64) -> Result<CommandBuffer, GpuError> {
        let block = Block::whole_pages(bytes)?;
        let raw = self
            .raw
            .commands(block.span())
            .await
            .map_err(GpuError::from)?;
        Ok(CommandBuffer {
            pages: Backed::new(raw, block),
        })
    }

    /// Hand the device `length` bytes of `commands` starting at
    /// `offset`, fenced at `fence`.
    ///
    /// Resolves when the device has taken the buffer, agreed or
    /// refused; the fence itself is retired on [`Context::fences`] at
    /// the same moment. `fence` must come after every fence this
    /// context has already submitted.
    pub async fn submit(
        &self,
        commands: &CommandBuffer,
        offset: u64,
        length: u64,
        fence: u64,
    ) -> Result<(), GpuError> {
        self.raw
            .submit(commands.pages.resource(), offset, length, fence)
            .await
            .map_err(GpuError::from)
    }

    /// Every fence this context retires, in order.
    ///
    /// A reader that lags sees the newest fence rather than every one
    /// between: retirement is monotone, so the newest answers for all
    /// that came before it.
    pub fn fences(&self) -> StreamReader<u64> {
        self.raw.fences()
    }

    /// Make `blob` reachable from this context's command stream.
    pub async fn attach(&self, blob: &Blob) -> Result<(), GpuError> {
        self.raw.attach(&blob.raw).await.map_err(GpuError::from)
    }

    /// Take `blob` back out of this context's command stream.
    pub async fn detach(&self, blob: &Blob) -> Result<(), GpuError> {
        self.raw.detach(&blob.raw).await.map_err(GpuError::from)
    }
}

/// One pinned command buffer.
///
/// The bytes at [`CommandBuffer::bytes`] are the renderer's command
/// stream, written with ordinary stores. The buffer is reusable: write
/// the commands, submit a run, and write the next over the same pages
/// once their fence has retired.
///
/// The kernel resource and the heap pages the buffer is shown in are one
/// value (see [`Backed`]): dropping the buffer makes the pages ordinary
/// memory again, and only then gives them back to the allocator.
pub struct CommandBuffer {
    pages: Backed<raw::CommandBuffer>,
}

impl CommandBuffer {
    /// The pages of this program's memory the buffer is shown at.
    pub fn span(&self) -> Span {
        self.pages.span()
    }

    /// How many bytes the buffer holds.
    pub const fn len(&self) -> usize {
        self.pages.len()
    }

    /// Whether the buffer holds no bytes, which it never does.
    pub const fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// The buffer itself.
    ///
    /// What is written here is what the display engine reads when
    /// [`Context::submit`] names it — there is no staging copy
    /// anywhere. The slice is this program's own heap, valid for as long
    /// as the borrow of the buffer.
    pub fn bytes(&mut self) -> &mut [u8] {
        self.pages.bytes_mut()
    }
}

/// One blob resource.
///
/// Where its bytes live depends on its memory kind: a guest-backed
/// blob's are this program's pinned pages, reached through
/// [`Blob::bytes`]; a host-3D blob's are the renderer's, reached through
/// [`Blob::map`]. A host-3D-guest blob has both.
///
/// The kernel resource comes first and the heap pages it is shown in
/// after it, for the reason [`Backed`] gives: dropping the blob
/// destroys it, which makes every span it was shown at ordinary memory
/// again, and only then are the pages freed. The pages are private and
/// never handed out except as a borrow of the blob.
pub struct Blob {
    raw: raw::Blob,
    /// Where the aperture storage is shown, while it is.
    mapping: Option<Block>,
    /// The blob's own pinned pages, for the guest-backed kinds.
    pages: Option<Block>,
    size: u64,
}

impl Blob {
    /// The pages of this program's memory the blob's own pages are shown
    /// at, for the guest-backed kinds; `None` for a host-3D blob, which
    /// has none.
    pub fn span(&self) -> Option<Span> {
        self.pages.as_ref().map(Block::span)
    }

    /// The guest-backed pages themselves; `None` for a host-3D blob.
    pub fn bytes(&mut self) -> Option<&mut [u8]> {
        self.pages.as_mut().map(Block::bytes_mut)
    }

    /// The bytes of this blob's host storage, while it is mapped.
    pub fn mapped(&mut self) -> Option<&mut [u8]> {
        self.mapping.as_mut().map(Block::bytes_mut)
    }

    /// Place this blob's host storage in the engine's host-visible
    /// aperture and show it in this program's memory.
    ///
    /// The pages are taken from this program's heap, sized to the blob.
    /// The bytes shown are a window onto memory the machine does not
    /// own: they are the renderer's, and this program's writes to them
    /// are the host's without a copy through either side. They stay
    /// until [`Blob::unmap`] or until the blob is dropped.
    pub async fn map(&mut self) -> Result<&mut [u8], GpuError> {
        let block = Block::zeroed(self.size)?;
        self.raw
            .map_blob(block.span())
            .await
            .map_err(GpuError::from)?;
        Ok(self.mapping.insert(block).bytes_mut())
    }

    /// Take this blob back out of the aperture.
    ///
    /// The pages it was shown at are this program's ordinary memory
    /// again, and are given back to the allocator.
    pub async fn unmap(&mut self) -> Result<(), GpuError> {
        self.raw.unmap_blob().await.map_err(GpuError::from)?;
        self.mapping = None;
        Ok(())
    }
}
