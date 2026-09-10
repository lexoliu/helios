//! The rendering half of the virtio-gpu driver.
//!
//! The same device, the same control queue, a different contract. Where
//! the 2D half owns scanouts and a cursor plane, this half owns what a
//! host renderer needs to be driven: the capability sets the host
//! publishes, the contexts a guest opens against one of them, the blob
//! resources a context works on, and the fences that say a submission
//! has been consumed.
//!
//! Nothing here interprets a byte. A capability set is read out of the
//! device and handed on; a command buffer is put on the wire exactly as
//! the guest wrote it. The driver's whole job is to move those bytes,
//! to place a host allocation in the engine's aperture when asked, and
//! to say when a fence has passed.
//!
//! # Where a mapped blob is
//!
//! In the device's *host-visible aperture*: a span of the machine's
//! physical address space that the display engine, not the guest,
//! decodes (virtio 1.2 §4.1.4.7). The transport publishes it as
//! shared-memory region [`SHM_ID_HOST_VISIBLE`], and this module hands
//! out offsets inside it. A device that publishes no such region maps
//! nothing, and says so, rather than being handed an address it does
//! not answer on.
//!
//! # Fences
//!
//! A fenced command is answered by the device only once its fence has
//! passed (virtio 1.2 §5.7.6.6), so the used-ring completion *is* the
//! fence signal and there is no second path to keep in step with it.
//! [`super::VirtioGpuDevice::submit`] records the fence on its
//! context's timeline the moment the completion arrives — before the
//! response code is looked at, so a command the device refuses still
//! retires its fence — and wakes every reader of that timeline.
//!
//! # Concurrency contract
//!
//! Every entry point takes `&self` and may be called from any
//! processor. Commands go up the same shared control queue the 2D half
//! uses, through the same `submit_chain` / `await_completion` pair, so
//! the queue lock is held only long enough to publish a chain and never
//! across an await. The context table and the blob table are spin
//! mutexes over fixed arrays, taken for a lookup or an insertion and
//! never held across an await; the fence notification is a broadcast,
//! armed by a reader before it re-reads the timeline it is following.

use core::sync::atomic::Ordering;

use arrayvec::ArrayVec;

use helios_hal::device::{DeviceRegion, DeviceRegionAttributes};
use helios_hal::display::{
    BlobId, BlobMemory, BlobRequest, BlobUsage, CapsetId, CapsetInfo, CapsetList, ContextId,
    ContextName, FenceId, Gpu3d, Gpu3dError, Gpu3dResult, MAX_BACKING_RANGES, MAX_CONTEXT_NAME,
};
use helios_hal::io::IoError;
use helios_hal::iommu::PhysicalRange;

use crate::transport::VirtioTransport;

use super::{
    CTRL_HEADER_BYTES, MemEntryError, RESP_ERR_OUT_OF_MEMORY, RESP_ERR_UNSPEC, RESP_OK_NODATA,
    VirtioGpuDevice, encode_mem_entries, write_header,
};

/// `VIRTIO_GPU_F_VIRGL`: the device carries a host renderer, and every
/// command in the 0x0200 block exists.
pub(super) const GPU_FEATURE_VIRGL: u64 = 1 << 0;
/// `VIRTIO_GPU_F_RESOURCE_UUID`: a resource can be given a UUID other
/// devices name it by.
pub(super) const GPU_FEATURE_RESOURCE_UUID: u64 = 1 << 2;
/// `VIRTIO_GPU_F_RESOURCE_BLOB`: resources whose storage is the host's,
/// the guest's, or both, rather than a 2D surface.
pub(super) const GPU_FEATURE_RESOURCE_BLOB: u64 = 1 << 3;
/// `VIRTIO_GPU_F_CONTEXT_INIT`: a context names the renderer it is for
/// when it is created, instead of every context being virgl's.
pub(super) const GPU_FEATURE_CONTEXT_INIT: u64 = 1 << 4;

/// The shared-memory region a device publishes its host-visible
/// aperture as (virtio 1.2 §5.7.4).
pub(super) const SHM_ID_HOST_VISIBLE: u8 = 0;

/// Contexts one device holds at once.
///
/// One kernel plugin owns the renderer and opens a context per thing it
/// is driving — a Vulkan device, a media session — so the bound is what
/// keeps the table a value rather than an allocation whose size a guest
/// chooses.
pub(super) const MAX_CONTEXTS: usize = 8;

/// Blob resources one device holds at once.
pub(super) const MAX_BLOBS: usize = 64;

/// Alignment every aperture offset is rounded to.
///
/// The aperture is mapped into a guest's address space by the kernel,
/// at whatever granule that address space changes mappings at, and the
/// driver does not know which. 64 KiB is the largest granule any
/// Helios target maps at, so an offset aligned to it is aligned for
/// every one of them; a smaller alignment would hand back a region
/// whose first page carried another blob's bytes.
pub(super) const APERTURE_ALIGN: u64 = 64 << 10;

/// Bytes one submission carries.
///
/// The bound is the ring's rather than the renderer's: a command buffer
/// travels as one readable descriptor, whose length field is 32 bits
/// wide, and a chain that could not be described would be refused by
/// the queue with nothing said about why. Four mebibytes is past the
/// largest batch venus or gfxstream emits in one submission.
pub(super) const MAX_COMMAND_BYTES: u64 = 4 << 20;

/// `VIRTIO_GPU_FLAG_FENCE`: the device answers this command only once
/// its fence has passed.
pub(super) const CTRL_FLAG_FENCE: u32 = 1 << 0;

/// Command codes of the 3D block (virtio 1.2 §5.7.6.7).
pub(super) const CMD_GET_CAPSET_INFO: u32 = 0x0108;
pub(super) const CMD_GET_CAPSET: u32 = 0x0109;
pub(super) const CMD_RESOURCE_CREATE_BLOB: u32 = 0x010c;
pub(super) const CMD_CTX_CREATE: u32 = 0x0200;
pub(super) const CMD_CTX_DESTROY: u32 = 0x0201;
pub(super) const CMD_CTX_ATTACH_RESOURCE: u32 = 0x0202;
pub(super) const CMD_CTX_DETACH_RESOURCE: u32 = 0x0203;
pub(super) const CMD_SUBMIT_3D: u32 = 0x0207;
pub(super) const CMD_RESOURCE_MAP_BLOB: u32 = 0x0208;
pub(super) const CMD_RESOURCE_UNMAP_BLOB: u32 = 0x0209;

pub(super) const RESP_OK_CAPSET_INFO: u32 = 0x1102;
pub(super) const RESP_OK_CAPSET: u32 = 0x1103;
pub(super) const RESP_OK_MAP_INFO: u32 = 0x1106;
pub(super) const RESP_ERR_INVALID_CONTEXT_ID: u32 = 0x1204;
const RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
const RESP_ERR_INVALID_PARAMETER: u32 = 0x1205;

/// `struct virtio_gpu_get_capset_info`: an index and its padding.
const GET_CAPSET_INFO_BYTES: usize = CTRL_HEADER_BYTES + 8;
/// `struct virtio_gpu_resp_capset_info`: an id, a version, a size and
/// padding.
pub(super) const RESP_CAPSET_INFO_BYTES: usize = CTRL_HEADER_BYTES + 16;
/// `struct virtio_gpu_get_capset`: an id and a version.
const GET_CAPSET_BYTES: usize = CTRL_HEADER_BYTES + 8;
/// `struct virtio_gpu_ctx_create`: a name length, the context type, and
/// the fixed-width debug name.
const CTX_CREATE_BYTES: usize = CTRL_HEADER_BYTES + 8 + MAX_CONTEXT_NAME;
/// `struct virtio_gpu_ctx_resource`: a resource id and its padding.
const CTX_RESOURCE_BYTES: usize = CTRL_HEADER_BYTES + 8;
/// `struct virtio_gpu_cmd_submit`: the command buffer's length and its
/// padding. The buffer itself follows as a descriptor of its own.
const SUBMIT_3D_BYTES: usize = CTRL_HEADER_BYTES + 8;
/// `struct virtio_gpu_resource_create_blob`: a resource id, the memory
/// kind, the usage flags, the entry count, the renderer's own handle
/// and the size.
const CREATE_BLOB_BYTES: usize = CTRL_HEADER_BYTES + 16 + 16;
/// `struct virtio_gpu_resource_map_blob`: a resource id, its padding
/// and the aperture offset.
const MAP_BLOB_BYTES: usize = CTRL_HEADER_BYTES + 8 + 8;
/// `struct virtio_gpu_resp_map_info`: the caching the host requires and
/// its padding.
pub(super) const RESP_MAP_INFO_BYTES: usize = CTRL_HEADER_BYTES + 8;
/// `struct virtio_gpu_resource_unmap_blob`: a resource id and its
/// padding.
const UNMAP_BLOB_BYTES: usize = CTRL_HEADER_BYTES + 8;

/// `VIRTIO_GPU_BLOB_MEM_*`: whose memory a blob's storage is.
pub(super) const BLOB_MEM_GUEST: u32 = 0x0001;
pub(super) const BLOB_MEM_HOST3D: u32 = 0x0002;
pub(super) const BLOB_MEM_HOST3D_GUEST: u32 = 0x0003;

/// `VIRTIO_GPU_MAP_CACHE_*`: how the host requires a mapped blob to be
/// accessed. The kernel maps the aperture as ordinary memory, so a host
/// that requires anything else is refused rather than mapped wrongly.
const MAP_CACHE_NONE: u32 = 0x0000;
pub(super) const MAP_CACHE_CACHED: u32 = 0x0001;

/// One context the device holds, and where its completion timeline has
/// reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ContextRecord {
    pub(super) id: ContextId,
    /// The renderer this context speaks to.
    pub(super) capset: CapsetId,
    /// The highest fence the device has retired on it. Increasing, so a
    /// reader that passes back what it last saw never misses one.
    pub(super) signalled: FenceId,
}

/// One blob the device holds, and where it sits in the aperture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct BlobRecord {
    pub(super) id: BlobId,
    pub(super) context: ContextId,
    /// How many bytes the resource covers, rounded up to the alignment
    /// every aperture offset takes.
    pub(super) bytes: u64,
    pub(super) usage: BlobUsage,
    /// Where in the aperture it is placed, once it has been mapped.
    pub(super) placed_at: Option<u64>,
}

impl<T: VirtioTransport> VirtioGpuDevice<T> {
    /// Runs one 3D control command and checks its response header.
    async fn render_command(
        &self,
        inputs: &[&[u8]],
        response: &mut [u8],
        expected: u32,
        subject: RenderSubject,
    ) -> Gpu3dResult<u32> {
        let written = self.control_exchange(inputs, &mut [response]).await?;
        if (written as usize) < CTRL_HEADER_BYTES {
            return Err(IoError::DeviceFault.into());
        }
        check_render_response(response, expected, subject)?;
        Ok(written)
    }

    /// The record for `context`, or the error that says it is not this
    /// device's.
    fn context_record(&self, context: ContextId) -> Gpu3dResult<ContextRecord> {
        if !self.renders() {
            return Err(Gpu3dError::Unsupported);
        }
        self.contexts
            .lock()
            .iter()
            .find(|record| record.id == context)
            .copied()
            .ok_or(Gpu3dError::UnknownContext(context))
    }

    /// The record for `blob`.
    fn blob_record(&self, blob: BlobId) -> Gpu3dResult<BlobRecord> {
        if !self.renders() {
            return Err(Gpu3dError::Unsupported);
        }
        self.blobs
            .lock()
            .iter()
            .find(|record| record.id == blob)
            .copied()
            .ok_or(Gpu3dError::UnknownBlob(blob))
    }

    /// Records that `fence` has retired on `context` and wakes every
    /// reader of that timeline.
    ///
    /// Called on the completion of a fenced command whatever the device
    /// answered, so a reader is never left waiting for a point the
    /// device has already decided will not come.
    fn retire_fence(&self, context: ContextId, fence: FenceId) {
        {
            let mut contexts = self.contexts.lock();
            let Some(record) = contexts.iter_mut().find(|record| record.id == context) else {
                return;
            };
            if fence <= record.signalled {
                return;
            }
            record.signalled = fence;
        }
        // Outside the lock: a waker runs executor code, and no executor
        // may be entered from inside a spin mutex.
        self.fences_signalled.notify_all();
    }

    /// Reserves a span of the host-visible aperture for `bytes`.
    ///
    /// First fit over the blobs already placed, at
    /// [`APERTURE_ALIGN`]. The set is bounded by [`MAX_BLOBS`], so the
    /// scan is bounded too, and a caller that has filled the aperture
    /// is told rather than handed an overlapping offset.
    fn reserve_aperture(&self, bytes: u64) -> Gpu3dResult<u64> {
        let aperture = self.host_visible.ok_or(Gpu3dError::ApertureExhausted)?;
        let blobs = self.blobs.lock();
        let mut candidate = 0_u64;
        // Every pass either accepts the candidate or moves it past one
        // more placed blob, and there are at most `MAX_BLOBS` of those.
        for _ in 0..=MAX_BLOBS {
            let end = candidate
                .checked_add(bytes)
                .ok_or(Gpu3dError::ApertureExhausted)?;
            if end > aperture.bytes {
                return Err(Gpu3dError::ApertureExhausted);
            }
            let clash = blobs
                .iter()
                .filter_map(|record| record.placed_at.map(|at| (at, record.bytes)))
                .find(|(at, held)| candidate < at + held && *at < end);
            match clash {
                Some((at, held)) => {
                    candidate = (at + held)
                        .checked_next_multiple_of(APERTURE_ALIGN)
                        .ok_or(Gpu3dError::ApertureExhausted)?;
                }
                None => return Ok(candidate),
            }
        }
        Err(Gpu3dError::ApertureExhausted)
    }
}

/// What a 3D request named, so an `ERR_INVALID_*` answer can say which
/// of them the device did not recognise.
#[derive(Clone, Copy, Debug, Default)]
struct RenderSubject {
    context: Option<ContextId>,
    blob: Option<BlobId>,
}

impl RenderSubject {
    const fn none() -> Self {
        Self {
            context: None,
            blob: None,
        }
    }

    const fn context(context: ContextId) -> Self {
        Self {
            context: Some(context),
            blob: None,
        }
    }

    const fn blob(blob: BlobId) -> Self {
        Self {
            context: None,
            blob: Some(blob),
        }
    }

    const fn both(context: ContextId, blob: BlobId) -> Self {
        Self {
            context: Some(context),
            blob: Some(blob),
        }
    }
}

impl<T: VirtioTransport> Gpu3d for VirtioGpuDevice<T> {
    fn renders(&self) -> bool {
        self.features.device(GPU_FEATURE_VIRGL)
    }

    async fn capsets(&self) -> Gpu3dResult<CapsetList> {
        let mut capsets = CapsetList::new();
        if !self.renders() {
            // Not a refusal: an engine with no renderer carries no
            // capability set, and the empty list is the whole answer.
            return Ok(capsets);
        }
        for index in 0..self.capset_count {
            if capsets.is_full() {
                // The device carries more capability sets than the
                // table holds. Answering with the ones that were read
                // would be wrong in a way a caller cannot see.
                return Err(Gpu3dError::TooMany {
                    resource: "capability sets",
                    limit: capsets.capacity(),
                });
            }
            let request = encode_get_capset_info(index);
            let mut response = [0_u8; RESP_CAPSET_INFO_BYTES];
            self.render_command(
                &[&request],
                &mut response,
                RESP_OK_CAPSET_INFO,
                RenderSubject::none(),
            )
            .await?;
            capsets.push(decode_capset_info(&response)?);
        }
        Ok(capsets)
    }

    async fn capset(&self, id: CapsetId, version: u32, out: &mut [u8]) -> Gpu3dResult<usize> {
        if !self.renders() {
            return Err(Gpu3dError::Unsupported);
        }
        let request = encode_get_capset(id, version);
        // The reply is a header followed by the bytes themselves, and
        // the bytes land in the caller's buffer rather than in one of
        // this driver's: a capability set runs to tens of kilobytes and
        // copying it twice would be the largest thing this driver did.
        let mut header = [0_u8; CTRL_HEADER_BYTES];
        let written = self
            .control_exchange(&[&request], &mut [&mut header, out])
            .await?;
        if (written as usize) < CTRL_HEADER_BYTES {
            return Err(IoError::DeviceFault.into());
        }
        check_render_response(&header, RESP_OK_CAPSET, RenderSubject::none())?;
        Ok((written as usize) - CTRL_HEADER_BYTES)
    }

    async fn create_context(&self, capset: CapsetId, name: ContextName) -> Gpu3dResult<ContextId> {
        if !self.renders() {
            return Err(Gpu3dError::Unsupported);
        }
        // Which capability sets exist is the caller's to know — it read
        // them — and a context type the host does not speak is refused
        // by the device rather than guessed at here.
        //
        // Context zero is the device's own "no context", so the counter
        // starts at one and never reuses: a stale id then names nothing
        // rather than naming somebody else's renderer state.
        let context = ContextId::new(self.next_context.fetch_add(1, Ordering::Relaxed));
        // The record is claimed before the device is told, so a second
        // task cannot fill the last slot while this one is in flight
        // and leave a context the device holds and this table does not.
        {
            let mut contexts = self.contexts.lock();
            if contexts.is_full() {
                return Err(Gpu3dError::TooMany {
                    resource: "contexts",
                    limit: MAX_CONTEXTS,
                });
            }
            contexts.push(ContextRecord {
                id: context,
                capset,
                signalled: FenceId::START,
            });
        }
        let request = encode_ctx_create(context, capset, &name, self.context_init_supported());
        let mut response = [0_u8; CTRL_HEADER_BYTES];
        match self
            .render_command(
                &[&request],
                &mut response,
                RESP_OK_NODATA,
                RenderSubject::context(context),
            )
            .await
        {
            Ok(_) => Ok(context),
            Err(error) => {
                self.contexts.lock().retain(|record| record.id != context);
                Err(error)
            }
        }
    }

    async fn destroy_context(&self, context: ContextId) -> Gpu3dResult<()> {
        let record = self.context_record(context)?;
        let request = encode_context_only(CMD_CTX_DESTROY, record.id);
        let mut response = [0_u8; CTRL_HEADER_BYTES];
        self.render_command(
            &[&request],
            &mut response,
            RESP_OK_NODATA,
            RenderSubject::context(record.id),
        )
        .await?;
        // Only once the renderer has let it go: until then a fence may
        // still retire on this timeline.
        self.contexts.lock().retain(|held| held.id != context);
        Ok(())
    }

    async fn create_blob(&self, request: BlobRequest<'_>) -> Gpu3dResult<BlobId> {
        if !self.features.device(GPU_FEATURE_RESOURCE_BLOB) {
            return Err(Gpu3dError::Unsupported);
        }
        // The context is checked first: a blob names the renderer that
        // owns it, and one that names nothing would be a resource
        // nothing could ever reach.
        let context = self.context_record(request.context)?.id;
        if request.backing.len() > MAX_BACKING_RANGES {
            return Err(Gpu3dError::TooManyBackingRanges {
                ranges: request.backing.len(),
                limit: MAX_BACKING_RANGES,
            });
        }
        if !request.is_coherent() {
            return Err(Gpu3dError::InvalidBlob);
        }
        let entries = encode_mem_entries(request.backing).map_err(|error| match error {
            MemEntryError::Empty => Gpu3dError::InvalidBlob,
            MemEntryError::Unrepresentable => IoError::DeviceFault.into(),
        })?;
        let bytes = request
            .size
            .checked_next_multiple_of(APERTURE_ALIGN)
            .ok_or(Gpu3dError::InvalidBlob)?;

        let blob = BlobId::new(self.next_resource.fetch_add(1, Ordering::Relaxed));
        {
            let mut blobs = self.blobs.lock();
            if blobs.is_full() {
                return Err(Gpu3dError::TooMany {
                    resource: "blobs",
                    limit: MAX_BLOBS,
                });
            }
            blobs.push(BlobRecord {
                id: blob,
                context,
                bytes,
                usage: request.usage,
                placed_at: None,
            });
        }
        let create = encode_create_blob(blob, &request, request.backing.len());
        let mut response = [0_u8; CTRL_HEADER_BYTES];
        let inputs: [&[u8]; 2] = [&create, &entries];
        let inputs = if entries.is_empty() {
            &inputs[..1]
        } else {
            &inputs[..]
        };
        match self
            .render_command(
                inputs,
                &mut response,
                RESP_OK_NODATA,
                RenderSubject::both(context, blob),
            )
            .await
        {
            Ok(_) => Ok(blob),
            Err(error) => {
                self.blobs.lock().retain(|record| record.id != blob);
                Err(error)
            }
        }
    }

    async fn destroy_blob(&self, blob: BlobId) -> Gpu3dResult<()> {
        let record = self.blob_record(blob)?;
        if record.placed_at.is_some() {
            // Nothing may take a resource back while a guest still has
            // a path to its bytes: the aperture would then decode to
            // whatever the renderer put there next.
            return Err(Gpu3dError::NotMappable(blob));
        }
        let request = super::encode_resource_only(
            super::CMD_RESOURCE_UNREF,
            helios_hal::display::FramebufferId::new(blob.raw()),
        );
        let mut response = [0_u8; CTRL_HEADER_BYTES];
        self.render_command(
            &[&request],
            &mut response,
            RESP_OK_NODATA,
            RenderSubject::blob(blob),
        )
        .await?;
        self.blobs.lock().retain(|held| held.id != blob);
        Ok(())
    }

    async fn map_blob(&self, blob: BlobId) -> Gpu3dResult<DeviceRegion> {
        let record = self.blob_record(blob)?;
        if !record.usage.contains(BlobUsage::MAPPABLE) || record.placed_at.is_some() {
            return Err(Gpu3dError::NotMappable(blob));
        }
        let aperture = self.host_visible.ok_or(Gpu3dError::ApertureExhausted)?;
        let offset = self.reserve_aperture(record.bytes)?;
        // Recorded before the device is told, so a second task cannot
        // reserve the same span while this one is in flight.
        self.place_blob(blob, Some(offset));

        let request = encode_map_blob(blob, offset);
        let mut response = [0_u8; RESP_MAP_INFO_BYTES];
        let mapped = self
            .render_command(
                &[&request],
                &mut response,
                RESP_OK_MAP_INFO,
                RenderSubject::blob(blob),
            )
            .await;
        if let Err(error) = mapped {
            self.place_blob(blob, None);
            return Err(error);
        }
        let map_info = u32::from_le_bytes(
            response[CTRL_HEADER_BYTES..CTRL_HEADER_BYTES + 4]
                .try_into()
                .expect("a four-byte window of a fixed-size response"),
        );
        if map_info != MAP_CACHE_NONE && map_info != MAP_CACHE_CACHED {
            // The host wants this span accessed uncached or
            // write-combining, and the kernel maps the aperture as
            // ordinary memory. Mapping it anyway would be a silent
            // disagreement about coherency between the renderer and
            // whoever reads the blob.
            self.unmap_blob_at(blob).await?;
            return Err(Gpu3dError::UnsupportedCaching { map_info });
        }
        Ok(DeviceRegion::new(
            PhysicalRange::new(aperture.start + offset, record.bytes),
            // Normal memory, not a register file: the bytes behind a
            // mapped blob are a renderer's buffer, with no read side
            // effects and nothing that ordering rules have to be
            // relaxed for.
            DeviceRegionAttributes::PREFETCHABLE_MEMORY,
        ))
    }

    async fn unmap_blob(&self, blob: BlobId) -> Gpu3dResult<()> {
        let record = self.blob_record(blob)?;
        if record.placed_at.is_none() {
            return Err(Gpu3dError::NotMappable(blob));
        }
        self.unmap_blob_at(blob).await
    }

    async fn attach_resource(&self, context: ContextId, blob: BlobId) -> Gpu3dResult<()> {
        self.context_resource(CMD_CTX_ATTACH_RESOURCE, context, blob)
            .await
    }

    async fn detach_resource(&self, context: ContextId, blob: BlobId) -> Gpu3dResult<()> {
        self.context_resource(CMD_CTX_DETACH_RESOURCE, context, blob)
            .await
    }

    async fn submit(
        &self,
        context: ContextId,
        commands: PhysicalRange,
        fence: FenceId,
    ) -> Gpu3dResult<()> {
        let record = self.context_record(context)?;
        if commands.bytes == 0 || commands.bytes > MAX_COMMAND_BYTES {
            return Err(Gpu3dError::CommandBufferLength {
                bytes: commands.bytes,
            });
        }
        let request = encode_submit_3d(record.id, commands.bytes, fence);
        let mut response = [0_u8; CTRL_HEADER_BYTES];
        // The guest's pages go on the wire by address: the device reads
        // the buffer where the guest wrote it, and this driver never
        // holds a copy of a command stream.
        let taken = self
            .control_exchange_with_payload(&[&request], Some(commands), &mut [&mut response])
            .await;
        // The fence is retired before the answer is read, and whatever
        // the answer is: a fenced command is completed only once its
        // fence has passed, so the completion *is* the signal, and a
        // reader of this context's timeline must not be left waiting on
        // a command the device refused.
        self.retire_fence(record.id, fence);
        let written = taken?;
        if (written as usize) < CTRL_HEADER_BYTES {
            return Err(IoError::DeviceFault.into());
        }
        check_render_response(&response, RESP_OK_NODATA, RenderSubject::context(record.id))
    }

    async fn fences(&self, context: ContextId, after: FenceId) -> Gpu3dResult<FenceId> {
        loop {
            // Armed before the timeline is read, so a fence that
            // retires between the read and the park is observed by this
            // future rather than slept through.
            let notified = self.fences_signalled.notified();
            let record = self.context_record(context)?;
            if record.signalled > after {
                return Ok(record.signalled);
            }
            notified.await;
        }
    }
}

impl<T: VirtioTransport> VirtioGpuDevice<T> {
    /// Whether a context may name the renderer it is for.
    ///
    /// Without `VIRTIO_GPU_F_CONTEXT_INIT` every context is virgl's, so
    /// the field stays zero and a caller asking for anything else is
    /// refused before the command is built.
    fn context_init_supported(&self) -> bool {
        self.features.device(GPU_FEATURE_CONTEXT_INIT)
    }

    /// Records where a blob sits in the aperture, or that it sits
    /// nowhere.
    fn place_blob(&self, blob: BlobId, offset: Option<u64>) {
        if let Some(record) = self.blobs.lock().iter_mut().find(|held| held.id == blob) {
            record.placed_at = offset;
        }
    }

    /// Takes a blob out of the aperture and forgets where it was.
    async fn unmap_blob_at(&self, blob: BlobId) -> Gpu3dResult<()> {
        let request = encode_unmap_blob(blob);
        let mut response = [0_u8; CTRL_HEADER_BYTES];
        self.render_command(
            &[&request],
            &mut response,
            RESP_OK_NODATA,
            RenderSubject::blob(blob),
        )
        .await?;
        // Only once the device has stopped decoding the span: until
        // then the aperture still answers on it.
        self.place_blob(blob, None);
        Ok(())
    }

    /// `CTX_ATTACH_RESOURCE` and `CTX_DETACH_RESOURCE` share a body.
    async fn context_resource(
        &self,
        command: u32,
        context: ContextId,
        blob: BlobId,
    ) -> Gpu3dResult<()> {
        let record = self.context_record(context)?;
        let held = self.blob_record(blob)?;
        let request = encode_ctx_resource(command, record.id, held.id);
        let mut response = [0_u8; CTRL_HEADER_BYTES];
        self.render_command(
            &[&request],
            &mut response,
            RESP_OK_NODATA,
            RenderSubject::both(record.id, held.id),
        )
        .await
        .map(|_| ())
    }
}

/// The feature mask a virtio-gpu asks for, given what it was offered.
///
/// The 3D bits are asked for only where the device offers a renderer:
/// blob resources and context types are the renderer's vocabulary, and
/// a driver that negotiated them against a device with no host renderer
/// would have told it to expect commands nothing will ever send. A
/// device that offers no `VIRTIO_GPU_F_VIRGL` therefore negotiates
/// exactly what it negotiated before this half of the driver existed.
pub(super) const fn wanted_features(offered: u64, base: u64) -> u64 {
    let mut wanted = base | (offered & GPU_FEATURE_RESOURCE_UUID);
    if offered & GPU_FEATURE_VIRGL != 0 {
        wanted |=
            GPU_FEATURE_VIRGL | (offered & (GPU_FEATURE_RESOURCE_BLOB | GPU_FEATURE_CONTEXT_INIT));
    }
    wanted
}

fn encode_get_capset_info(index: u32) -> [u8; GET_CAPSET_INFO_BYTES] {
    let mut bytes = [0_u8; GET_CAPSET_INFO_BYTES];
    write_header(&mut bytes, CMD_GET_CAPSET_INFO);
    bytes[CTRL_HEADER_BYTES..CTRL_HEADER_BYTES + 4].copy_from_slice(&index.to_le_bytes());
    bytes
}

fn encode_get_capset(id: CapsetId, version: u32) -> [u8; GET_CAPSET_BYTES] {
    let mut bytes = [0_u8; GET_CAPSET_BYTES];
    write_header(&mut bytes, CMD_GET_CAPSET);
    let body = &mut bytes[CTRL_HEADER_BYTES..];
    body[0..4].copy_from_slice(&id.raw().to_le_bytes());
    body[4..8].copy_from_slice(&version.to_le_bytes());
    bytes
}

/// `struct virtio_gpu_ctx_create`.
///
/// The context type goes in `context_init` only where the device
/// negotiated `VIRTIO_GPU_F_CONTEXT_INIT`; without it the field is
/// reserved and every context is virgl's, which is what the caller has
/// already been told by the capability set it selected.
fn encode_ctx_create(
    context: ContextId,
    capset: CapsetId,
    name: &ContextName,
    context_init: bool,
) -> [u8; CTX_CREATE_BYTES] {
    let mut bytes = [0_u8; CTX_CREATE_BYTES];
    write_render_header(&mut bytes, CMD_CTX_CREATE, 0, FenceId::START, context);
    let body = &mut bytes[CTRL_HEADER_BYTES..];
    body[0..4].copy_from_slice(&(name.len() as u32).to_le_bytes());
    if context_init {
        body[4..8].copy_from_slice(&capset.raw().to_le_bytes());
    }
    body[8..8 + name.len()].copy_from_slice(name.as_bytes());
    bytes
}

/// `CTX_DESTROY`, whose body is the header's context id and nothing
/// else.
fn encode_context_only(command: u32, context: ContextId) -> [u8; CTRL_HEADER_BYTES] {
    let mut bytes = [0_u8; CTRL_HEADER_BYTES];
    write_render_header(&mut bytes, command, 0, FenceId::START, context);
    bytes
}

fn encode_ctx_resource(command: u32, context: ContextId, blob: BlobId) -> [u8; CTX_RESOURCE_BYTES] {
    let mut bytes = [0_u8; CTX_RESOURCE_BYTES];
    write_render_header(&mut bytes, command, 0, FenceId::START, context);
    bytes[CTRL_HEADER_BYTES..CTRL_HEADER_BYTES + 4].copy_from_slice(&blob.raw().to_le_bytes());
    bytes
}

fn encode_submit_3d(context: ContextId, length: u64, fence: FenceId) -> [u8; SUBMIT_3D_BYTES] {
    let mut bytes = [0_u8; SUBMIT_3D_BYTES];
    write_render_header(&mut bytes, CMD_SUBMIT_3D, CTRL_FLAG_FENCE, fence, context);
    bytes[CTRL_HEADER_BYTES..CTRL_HEADER_BYTES + 4].copy_from_slice(
        &u32::try_from(length)
            .expect("a command buffer is bounded by MAX_COMMAND_BYTES")
            .to_le_bytes(),
    );
    bytes
}

fn encode_create_blob(
    blob: BlobId,
    request: &BlobRequest<'_>,
    entries: usize,
) -> [u8; CREATE_BLOB_BYTES] {
    let mut bytes = [0_u8; CREATE_BLOB_BYTES];
    write_render_header(
        &mut bytes,
        CMD_RESOURCE_CREATE_BLOB,
        0,
        FenceId::START,
        request.context,
    );
    let body = &mut bytes[CTRL_HEADER_BYTES..];
    body[0..4].copy_from_slice(&blob.raw().to_le_bytes());
    body[4..8].copy_from_slice(&wire_blob_memory(request.memory).to_le_bytes());
    body[8..12].copy_from_slice(&request.usage.bits().to_le_bytes());
    body[12..16].copy_from_slice(
        &u32::try_from(entries)
            .expect("the range count is bounded by MAX_BACKING_RANGES")
            .to_le_bytes(),
    );
    body[16..24].copy_from_slice(&request.host_id.to_le_bytes());
    body[24..32].copy_from_slice(&request.size.to_le_bytes());
    bytes
}

fn encode_map_blob(blob: BlobId, offset: u64) -> [u8; MAP_BLOB_BYTES] {
    let mut bytes = [0_u8; MAP_BLOB_BYTES];
    write_header(&mut bytes, CMD_RESOURCE_MAP_BLOB);
    let body = &mut bytes[CTRL_HEADER_BYTES..];
    body[0..4].copy_from_slice(&blob.raw().to_le_bytes());
    body[8..16].copy_from_slice(&offset.to_le_bytes());
    bytes
}

fn encode_unmap_blob(blob: BlobId) -> [u8; UNMAP_BLOB_BYTES] {
    let mut bytes = [0_u8; UNMAP_BLOB_BYTES];
    write_header(&mut bytes, CMD_RESOURCE_UNMAP_BLOB);
    bytes[CTRL_HEADER_BYTES..CTRL_HEADER_BYTES + 4].copy_from_slice(&blob.raw().to_le_bytes());
    bytes
}

/// `struct virtio_gpu_ctrl_hdr` for a 3D command: the type, the flags,
/// the fence the device answers after, and the context the command
/// belongs to.
pub(super) fn write_render_header(
    bytes: &mut [u8],
    command: u32,
    flags: u32,
    fence: FenceId,
    context: ContextId,
) {
    bytes[0..4].copy_from_slice(&command.to_le_bytes());
    bytes[4..8].copy_from_slice(&flags.to_le_bytes());
    bytes[8..16].copy_from_slice(&fence.raw().to_le_bytes());
    bytes[16..20].copy_from_slice(&context.raw().to_le_bytes());
}

const fn wire_blob_memory(memory: BlobMemory) -> u32 {
    match memory {
        BlobMemory::Guest => BLOB_MEM_GUEST,
        BlobMemory::Host3d => BLOB_MEM_HOST3D,
        BlobMemory::Host3dGuest => BLOB_MEM_HOST3D_GUEST,
    }
}

/// `struct virtio_gpu_resp_capset_info`.
fn decode_capset_info(response: &[u8]) -> Gpu3dResult<CapsetInfo> {
    let word = |offset: usize| -> Gpu3dResult<u32> {
        Ok(u32::from_le_bytes(
            response
                .get(offset..offset + 4)
                .and_then(|slice| slice.try_into().ok())
                .ok_or(IoError::DeviceFault)?,
        ))
    };
    Ok(CapsetInfo {
        id: CapsetId::new(word(CTRL_HEADER_BYTES)?),
        max_version: word(CTRL_HEADER_BYTES + 4)?,
        max_size: word(CTRL_HEADER_BYTES + 8)?,
    })
}

/// Turns a response header into either "this is the answer that was
/// asked for" or the typed refusal it carries.
fn check_render_response(
    response: &[u8],
    expected: u32,
    subject: RenderSubject,
) -> Gpu3dResult<()> {
    let code = u32::from_le_bytes(
        response
            .get(0..4)
            .and_then(|slice| slice.try_into().ok())
            .ok_or(IoError::DeviceFault)?,
    );
    if code == expected {
        return Ok(());
    }
    Err(match (code, subject) {
        (RESP_ERR_UNSPEC, _) => Gpu3dError::Unspecified,
        (RESP_ERR_OUT_OF_MEMORY, _) => Gpu3dError::OutOfMemory,
        (RESP_ERR_INVALID_PARAMETER, _) => Gpu3dError::InvalidParameter,
        (
            RESP_ERR_INVALID_CONTEXT_ID,
            RenderSubject {
                context: Some(context),
                ..
            },
        ) => Gpu3dError::UnknownContext(context),
        (
            RESP_ERR_INVALID_RESOURCE_ID,
            RenderSubject {
                blob: Some(blob), ..
            },
        ) => Gpu3dError::UnknownBlob(blob),
        (code, _) => Gpu3dError::UnexpectedResponse { code },
    })
}

/// The blob table one device holds.
pub(super) type BlobRecords = ArrayVec<BlobRecord, MAX_BLOBS>;
/// The context table one device holds.
pub(super) type ContextRecords = ArrayVec<ContextRecord, MAX_CONTEXTS>;
