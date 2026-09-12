//! The machine's 3D engine, and the command buffers a program renders
//! through.
//!
//! Exactly one program holds the engine at a time. [`Gpu::claim`] takes
//! it; dropping what that returns — or dying — hands every context and
//! blob back.
//!
//! Nothing the renderer speaks goes through a call. A command buffer is
//! memory the kernel pinned inside this program's own linear memory, so
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
use crate::wit_bindgen::StreamReader;

pub use crate::bindings::helios::system::gpu::{BlobMemory, BlobUsage, CapsetInfo, Placement};

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
    #[error("the blob request does not describe a resource this engine can create")]
    InvalidBlob,
    #[error("the display engine's host-visible aperture has no room left")]
    ApertureExhausted,
    #[error("the submission does not lie inside the command buffer")]
    OutOfBounds,
    #[error("a fence must come after every fence this context has already submitted")]
    StaleFence,
    #[error("this program's 3D window has no room left")]
    WindowExhausted,
    #[error("no contiguous run of memory left for a command buffer")]
    OutOfMemory,
    #[error("the display engine faulted")]
    DeviceFault,
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
            raw::Error::InvalidBlob => Self::InvalidBlob,
            raw::Error::ApertureExhausted => Self::ApertureExhausted,
            raw::Error::OutOfBounds => Self::OutOfBounds,
            raw::Error::StaleFence => Self::StaleFence,
            raw::Error::WindowExhausted => Self::WindowExhausted,
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
    pub async fn create_blob(
        &self,
        context: &Context,
        memory: BlobMemory,
        usage: BlobUsage,
        size: u64,
        host_id: u64,
    ) -> Result<Blob, GpuError> {
        self.raw
            .create_blob(&context.raw, memory, usage, size, host_id)
            .await
            .map(|raw| Blob { raw })
            .map_err(GpuError::from)
    }
}

/// One renderer context.
pub struct Context {
    raw: raw::Context,
}

impl Context {
    /// Pin `bytes` of this program's memory as a command buffer.
    pub async fn commands(&self, bytes: u64) -> Result<CommandBuffer, GpuError> {
        self.raw
            .commands(bytes)
            .await
            .map(|raw| CommandBuffer { raw })
            .map_err(GpuError::from)
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
            .submit(&commands.raw, offset, length, fence)
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
pub struct CommandBuffer {
    raw: raw::CommandBuffer,
}

impl CommandBuffer {
    /// Where this buffer sits in this program's linear memory.
    pub fn placement(&self) -> Placement {
        self.raw.buffer()
    }

    /// The buffer itself.
    ///
    /// What is written here is what the display engine reads when
    /// [`Context::submit`] names it — there is no staging copy
    /// anywhere.
    ///
    /// # Panics
    ///
    /// Panics when the kernel placed the buffer somewhere this program
    /// cannot address, which would mean the two disagree about the size
    /// of a pointer.
    pub fn bytes(&mut self) -> &mut [u8] {
        let placement = self.raw.buffer();
        let offset = usize::try_from(placement.offset)
            .expect("the kernel places a command buffer inside this program's address space");
        let length = usize::try_from(placement.length)
            .expect("the kernel places a command buffer inside this program's address space");
        // SAFETY: the kernel pinned `length` bytes of physically
        // contiguous memory at `offset` in this program's linear memory
        // and mapped them readable and writable for as long as this
        // buffer exists. Nothing else in this program can reach them:
        // the offset is above everything the allocator can ever hand
        // out, because the kernel caps this program's memory growth
        // below it while it holds the 3D engine.
        unsafe { core::slice::from_raw_parts_mut(offset as *mut u8, length) }
    }
}

/// One blob resource.
///
/// Where its bytes live depends on its memory kind: a guest-backed
/// blob's are this program's pinned pages at [`Blob::pages`]; a host-3D
/// blob's are the renderer's, reached through [`Blob::map`].
pub struct Blob {
    raw: raw::Blob,
}

impl Blob {
    /// Where this blob's own pages sit in linear memory, for the
    /// guest-backed kinds; `None` for a host-3D blob, which has none.
    pub fn pages(&self) -> Option<Placement> {
        self.raw.buffer()
    }

    /// The guest-backed pages themselves, for the kinds that have them.
    ///
    /// # Panics
    ///
    /// Panics when this blob has no guest backing — a host-3D blob —
    /// or when the kernel placed it somewhere this program cannot
    /// address.
    pub fn bytes(&mut self) -> &mut [u8] {
        let placement = self
            .raw
            .buffer()
            .expect("a host-3D blob has no guest pages of its own");
        let offset = usize::try_from(placement.offset)
            .expect("the kernel places a blob inside this program's address space");
        let length = usize::try_from(placement.length)
            .expect("the kernel places a blob inside this program's address space");
        // SAFETY: the kernel pinned `length` bytes of physically
        // contiguous memory at `offset` in this program's linear memory
        // and mapped them readable and writable for as long as this
        // blob exists.
        unsafe { core::slice::from_raw_parts_mut(offset as *mut u8, length) }
    }

    /// Place this blob's host storage in the engine's host-visible
    /// aperture and report where it landed in this program's linear
    /// memory.
    ///
    /// The placement is a window onto memory the machine does not own:
    /// the bytes behind it are the renderer's, and this program's
    /// writes to them are the host's without a copy through either
    /// side.
    pub async fn map(&self) -> Result<Placement, GpuError> {
        self.raw.map_blob().await.map_err(GpuError::from)
    }

    /// Take this blob back out of the aperture.
    pub async fn unmap(&self) -> Result<(), GpuError> {
        self.raw.unmap_blob().await.map_err(GpuError::from)
    }
}
