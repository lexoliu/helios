//! virtio-gpu 2D driver: scanouts and the hardware cursor plane.
//!
//! The device is a display engine with a resource table. A frame buffer
//! is a *resource* the driver creates and then backs with pages the
//! caller owns; a scanout is pointed at a region of one resource; and a
//! flush is two commands, one that copies the caller's bytes into the
//! device's own copy of the resource and one that tells the scanouts
//! showing it to re-read. The cursor is the same kind of resource, put
//! on a separate plane through a queue of its own so pointer motion
//! never queues behind a frame.
//!
//! Two queues serve it (virtio 1.2 §5.7.2): the control queue carries
//! everything that changes the resource table or the scanout
//! configuration, and the cursor queue carries the two commands that
//! move and re-image the pointer. They are separate because they have
//! different urgency, and this driver keeps them separate: a cursor
//! command never waits behind a frame's transfer.
//!
//! `VIRTIO_GPU_F_EDID` is always asked for, because the monitor's own
//! preferred timing is a better answer than the geometry the host
//! happens to have published. The 3D features — `VIRTIO_GPU_F_VIRGL`,
//! `VIRTIO_GPU_F_RESOURCE_BLOB`, `VIRTIO_GPU_F_CONTEXT_INIT`,
//! `VIRTIO_GPU_F_RESOURCE_UUID` — are asked for only where the device
//! offers them, and the rendering half they open up lives in
//! [`render`]. A device that offers no renderer negotiates exactly what
//! a 2D-only driver did, which is what the boot line's `3d=none` says.
//!
//! The driver never allocates a frame buffer. Backing pages arrive from
//! the caller as physical ranges and are published to the device as they
//! are, which is also why a device behind a translation unit is refused
//! at bring-up: its caller's pages are not in its domain, and the first
//! scanout would fetch from an address the unit rejects.
//!
//! # Concurrency contract
//!
//! Every entry point takes `&self` and may be called from any processor.
//! Control commands go through the shared `submit_chain` /
//! `await_completion` pair, so the control queue lock is held only long
//! enough to publish a chain and is never held across an await: several
//! tasks have commands in flight at once and completions are routed back
//! by descriptor identifier. The cursor queue works the same way on its
//! own lock. The frame-buffer table is a spin mutex over a fixed array,
//! taken for a lookup or an insertion and never held across an await.
//! `handle_interrupt` runs in interrupt context: it acknowledges the
//! device, clears a display event if the device raised one, and wakes
//! waiters, and does nothing else.

use core::sync::atomic::{AtomicU32, Ordering};

use alloc::vec;
use alloc::vec::Vec;
use arrayvec::ArrayVec;
use async_lock::Mutex as AsyncMutex;
use spin::Mutex;

use helios_hal::display::{
    CursorImage, DisplayError, DisplayMode, DisplayResult, FramebufferId, MAX_BACKING_RANGES,
    MAX_SCANOUTS, PixelFormat, Point, Rect, ScanoutId, ScanoutInfo, ScanoutList,
};
use helios_hal::io::{IoError, IoResult};
use helios_hal::iommu::PhysicalRange;
use helios_hal::pmm::PhysFrameRange;

use crate::bus::{DeviceBus, DmaAddressing, DmaPool};
use crate::features::{NegotiatedFeatures, RING_FEATURES, negotiate_with};
use crate::inflight::{InFlight, await_completion, submit_chain, submit_chain_with_payload};
use crate::notify::Notify;
use crate::queue::{VirtQueue, negotiated_queue_size};
use crate::transport::{DeviceStatus, DeviceType, VirtioTransport};

/// The control queue (virtio 1.2 §5.7.2): every command that touches the
/// resource table or the scanout configuration.
const CONTROL_QUEUE_INDEX: u16 = 0;
/// The cursor queue: the two commands that place and move the pointer.
const CURSOR_QUEUE_INDEX: u16 = 1;

/// Depth the driver asks for on the control queue. A compositor has one
/// transfer and one flush per damaged region in flight per frame, across
/// however many surfaces it is presenting; this is what bounds that
/// without the ring becoming the thing that stalls.
const CONTROL_QUEUE_SIZE: u16 = 64;
/// The cursor queue carries one command per pointer event.
const CURSOR_QUEUE_SIZE: u16 = 16;

/// A control chain is the request, the memory-entry table that only
/// `RESOURCE_ATTACH_BACKING` carries, and the writable response.
const CONTROL_CHAIN_LIMIT: u16 = 3;
/// A cursor command is a single readable buffer: the device answers by
/// returning the descriptor, not by writing a reply.
const CURSOR_CHAIN_LIMIT: u16 = 1;

/// Frame buffers one device tracks at once.
///
/// The table exists because a transfer needs the resource's width and
/// format to turn a damaged rectangle into a byte offset, which the
/// caller should not have to repeat on every flush. The bound is the
/// number of surfaces a scanout set plus a cursor plane needs.
const MAX_FRAMEBUFFERS: usize = MAX_SCANOUTS + 8;

/// `VIRTIO_GPU_F_EDID`: the device answers `GET_EDID` with the attached
/// monitor's own descriptor block.
const GPU_FEATURE_EDID: u64 = 1 << 1;

/// Byte offsets in `struct virtio_gpu_config` (virtio 1.2 §5.7.4).
const CONFIG_EVENTS_READ: usize = 0;
const CONFIG_EVENTS_CLEAR: usize = 4;
const CONFIG_NUM_SCANOUTS: usize = 8;
const CONFIG_NUM_CAPSETS: usize = 12;

/// `VIRTIO_GPU_EVENT_DISPLAY`: the set of scanouts changed.
const EVENT_DISPLAY: u32 = 1 << 0;

/// Command and response codes (virtio 1.2 §5.7.6.7).
const CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const CMD_RESOURCE_UNREF: u32 = 0x0102;
const CMD_SET_SCANOUT: u32 = 0x0103;
const CMD_RESOURCE_FLUSH: u32 = 0x0104;
const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const CMD_RESOURCE_DETACH_BACKING: u32 = 0x0107;
const CMD_GET_EDID: u32 = 0x010a;
const CMD_UPDATE_CURSOR: u32 = 0x0300;
const CMD_MOVE_CURSOR: u32 = 0x0301;

const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const RESP_OK_EDID: u32 = 0x1104;
const RESP_ERR_UNSPEC: u32 = 0x1200;
const RESP_ERR_OUT_OF_MEMORY: u32 = 0x1201;
const RESP_ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
const RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
const RESP_ERR_INVALID_PARAMETER: u32 = 0x1205;

/// `struct virtio_gpu_ctrl_hdr`: type, flags, fence id, context id, ring
/// index and three padding bytes.
const CTRL_HEADER_BYTES: usize = 24;
/// `struct virtio_gpu_rect`: x, y, width, height.
const RECT_BYTES: usize = 16;
/// `struct virtio_gpu_display_one`: a rectangle, an enabled flag and a
/// flags word.
const DISPLAY_ONE_BYTES: usize = RECT_BYTES + 8;
/// `struct virtio_gpu_resp_display_info`.
const DISPLAY_INFO_BYTES: usize = CTRL_HEADER_BYTES + DISPLAY_ONE_BYTES * MAX_SCANOUTS;
/// `struct virtio_gpu_get_edid`: a scanout id and its padding.
const GET_EDID_BYTES: usize = CTRL_HEADER_BYTES + 8;
/// Largest EDID block the device returns, per `struct
/// virtio_gpu_resp_edid`.
const EDID_BLOCK_BYTES: usize = 1024;
/// `struct virtio_gpu_resp_edid`: a size, its padding, and the block.
const EDID_RESPONSE_BYTES: usize = CTRL_HEADER_BYTES + 8 + EDID_BLOCK_BYTES;
/// `struct virtio_gpu_resource_create_2d`.
const CREATE_2D_BYTES: usize = CTRL_HEADER_BYTES + 16;
/// `struct virtio_gpu_resource_unref` and `..._detach_backing`: a
/// resource id and its padding.
const RESOURCE_ONLY_BYTES: usize = CTRL_HEADER_BYTES + 8;
/// `struct virtio_gpu_set_scanout` and `struct
/// virtio_gpu_resource_flush`: a rectangle and two words.
const RECT_COMMAND_BYTES: usize = CTRL_HEADER_BYTES + RECT_BYTES + 8;
/// `struct virtio_gpu_transfer_to_host_2d`: a rectangle, a 64-bit
/// offset, a resource id and its padding.
const TRANSFER_2D_BYTES: usize = CTRL_HEADER_BYTES + RECT_BYTES + 16;
/// `struct virtio_gpu_resource_attach_backing`: a resource id and an
/// entry count.
const ATTACH_BACKING_BYTES: usize = CTRL_HEADER_BYTES + 8;
/// `struct virtio_gpu_mem_entry`: an address, a length and padding.
const MEM_ENTRY_BYTES: usize = 16;
/// `struct virtio_gpu_update_cursor`: a cursor position, a resource id,
/// a hotspot and padding.
const UPDATE_CURSOR_BYTES: usize = CTRL_HEADER_BYTES + 16 + 16;

/// The `struct virtio_gpu_mem_entry` table one attach request carries.
type MemEntryTable = ArrayVec<u8, { MAX_BACKING_RANGES * MEM_ENTRY_BYTES }>;

/// The resource id that means "none": no scanout shows it and no cursor
/// plane carries it, which is how both are switched off.
const RESOURCE_NONE: u32 = 0;

/// Where an EDID block's first detailed timing descriptor starts, and
/// how long one is (VESA E-EDID, §3.10).
const EDID_DETAILED_TIMING_OFFSET: usize = 54;
const EDID_DETAILED_TIMING_BYTES: usize = 18;
/// The eight-byte header every EDID block opens with.
const EDID_MAGIC: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];

/// The display topology the bring-up path read out of the device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayTopology {
    /// Every scanout the device presents.
    pub scanouts: ScanoutList,
    /// The mode the device would rather its first scanout were driven
    /// at.
    pub preferred: DisplayMode,
}

/// One frame buffer the device holds for a caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FramebufferRecord {
    id: FramebufferId,
    mode: DisplayMode,
    format: PixelFormat,
}

/// What a request named, so that an `ERR_INVALID_*` answer can say which
/// of them the device did not recognise.
///
/// A device that answers "invalid scanout" to a request naming no
/// scanout has not refused anything the caller asked for; it has
/// answered the wrong question, and that is a fault rather than a
/// refusal.
#[derive(Clone, Copy, Debug, Default)]
struct RequestSubject {
    scanout: Option<ScanoutId>,
    framebuffer: Option<FramebufferId>,
}

impl RequestSubject {
    const fn none() -> Self {
        Self {
            scanout: None,
            framebuffer: None,
        }
    }

    const fn scanout(scanout: ScanoutId) -> Self {
        Self {
            scanout: Some(scanout),
            framebuffer: None,
        }
    }

    const fn framebuffer(framebuffer: FramebufferId) -> Self {
        Self {
            scanout: None,
            framebuffer: Some(framebuffer),
        }
    }

    const fn both(scanout: ScanoutId, framebuffer: FramebufferId) -> Self {
        Self {
            scanout: Some(scanout),
            framebuffer: Some(framebuffer),
        }
    }
}

pub struct VirtioGpuDevice<T: VirtioTransport> {
    transport: T,
    control: AsyncMutex<VirtQueue<T>>,
    control_inflight: InFlight<{ CONTROL_QUEUE_SIZE as usize }>,
    cursor: AsyncMutex<VirtQueue<T>>,
    cursor_inflight: InFlight<{ CURSOR_QUEUE_SIZE as usize }>,
    /// Raised by every device interrupt: a completion may be waiting on
    /// either queue.
    completions: Notify,
    /// Raised when the device announces `VIRTIO_GPU_EVENT_DISPLAY`.
    display_events: Notify,
    /// Frame buffers this device holds, and the geometry each was
    /// created with.
    framebuffers: Mutex<ArrayVec<FramebufferRecord, MAX_FRAMEBUFFERS>>,
    /// Next resource id to hand out. Resource ids are the driver's to
    /// choose and `0` is reserved, so the counter starts at one and
    /// never reuses: a stale id then names nothing rather than naming
    /// somebody else's frame buffer.
    next_resource: AtomicU32,
    scanout_count: u32,
    capset_count: u32,
    features: NegotiatedFeatures,
    /// Next context id to hand out. Context zero is the device's own
    /// "no context", so the counter starts at one and never reuses.
    next_context: AtomicU32,
    /// The renderer contexts this device holds, and where each one's
    /// completion timeline has reached.
    contexts: Mutex<render::ContextRecords>,
    /// The blob resources this device holds, and where in the aperture
    /// each mapped one sits.
    blobs: Mutex<render::BlobRecords>,
    /// Raised whenever a fence retires on any context. A broadcast,
    /// because every reader of a timeline is owed the same event; each
    /// re-reads the timeline it is following for itself.
    fences_signalled: Notify,
    /// The device's host-visible aperture, where a host-3D blob is
    /// mapped. Absent on a device that publishes no such window, which
    /// is every device with no renderer.
    host_visible: Option<PhysicalRange>,
}

impl<T: VirtioTransport> VirtioGpuDevice<T> {
    /// Programs the device and its two queues.
    ///
    /// No queue traffic happens here: the display topology is read by
    /// [`VirtioGpuDevice::read_topology`], which the bring-up
    /// constructors call once they have named the transport.
    pub fn new(transport: T) -> IoResult<Self> {
        if transport.device_type() != DeviceType::Gpu {
            return Err(IoError::Unsupported);
        }
        // A frame buffer's pages belong to the caller and are published
        // to the device exactly as they are. Behind a translation unit
        // they would have to be mapped into this device's domain first,
        // and nothing here owns that domain, so the first scanout would
        // fetch from an address the unit refuses.
        if transport.bus().dma().addressing() != DmaAddressing::Physical {
            return Err(IoError::InvalidDeviceConfig(
                "virtio-gpu behind a translation unit has no domain for its caller-owned frame buffers",
            ));
        }

        let features = negotiate_with(&transport, |offered| {
            render::wanted_features(offered, RING_FEATURES | GPU_FEATURE_EDID)
        })?;

        let control_size =
            negotiated_queue_size(&transport, CONTROL_QUEUE_INDEX, CONTROL_QUEUE_SIZE)?;
        let cursor_size = negotiated_queue_size(&transport, CURSOR_QUEUE_INDEX, CURSOR_QUEUE_SIZE)?;
        let control = VirtQueue::new(
            &transport,
            CONTROL_QUEUE_INDEX,
            control_size,
            CONTROL_CHAIN_LIMIT,
            features,
        )?;
        let cursor = VirtQueue::new(
            &transport,
            CURSOR_QUEUE_INDEX,
            cursor_size,
            CURSOR_CHAIN_LIMIT,
            features,
        )?;

        let scanout_count = transport.read_config_u32(CONFIG_NUM_SCANOUTS);
        if scanout_count == 0 || scanout_count as usize > MAX_SCANOUTS {
            return Err(IoError::InvalidDeviceConfig(
                "virtio-gpu reports a scanout count outside the one to sixteen the specification allows",
            ));
        }
        let capset_count = transport.read_config_u32(CONFIG_NUM_CAPSETS);

        transport.set_status(
            DeviceStatus::ACKNOWLEDGE
                | DeviceStatus::DRIVER
                | DeviceStatus::FEATURES_OK
                | DeviceStatus::DRIVER_OK,
        );

        // The aperture is read here rather than on first use: it is a
        // property of the transport's own capability list, and the
        // configuration space that carries it is no longer reachable
        // once bring-up has handed the transport over.
        let host_visible = transport.shared_memory_region(render::SHM_ID_HOST_VISIBLE);

        Ok(Self {
            transport,
            control: AsyncMutex::new(control),
            control_inflight: InFlight::new(),
            cursor: AsyncMutex::new(cursor),
            cursor_inflight: InFlight::new(),
            completions: Notify::new(),
            display_events: Notify::new(),
            framebuffers: Mutex::new(ArrayVec::new()),
            next_resource: AtomicU32::new(1),
            scanout_count,
            capset_count,
            features,
            next_context: AtomicU32::new(1),
            contexts: Mutex::new(ArrayVec::new()),
            blobs: Mutex::new(ArrayVec::new()),
            fences_signalled: Notify::new(),
            host_visible,
        })
    }

    /// The feature set this device negotiated.
    pub fn features(&self) -> NegotiatedFeatures {
        self.features
    }

    /// How many scanouts the device says it presents.
    pub fn scanout_count(&self) -> u32 {
        self.scanout_count
    }

    /// How many capability sets the device says it carries.
    ///
    /// The device's own count. What each of them describes is read at
    /// bring-up and answered by
    /// [`helios_hal::display::Gpu3d::capsets`], which is empty on a
    /// device that offers no renderer however many the count claims.
    pub fn capset_count(&self) -> u32 {
        self.capset_count
    }

    /// The device's host-visible aperture, where a host-3D blob is
    /// mapped, or `None` on a device that publishes none.
    pub fn host_visible_aperture(&self) -> Option<PhysicalRange> {
        self.host_visible
    }

    /// Whether the device answers `GET_EDID`.
    pub fn edid_supported(&self) -> bool {
        self.features.device(GPU_FEATURE_EDID)
    }

    /// Reads the display topology on the bring-up path, before the
    /// executor exists.
    ///
    /// The round trip is a device handshake rather than a wait on
    /// software state: the device answers `GET_DISPLAY_INFO` out of its
    /// own model without needing anything from this machine first, which
    /// is why the used ring may be polled for it here the way
    /// virtio-net's control commands and virtio-iommu's requests are.
    /// Every later command goes through the asynchronous path and parks
    /// on the device's interrupt.
    pub fn read_topology(&self) -> DisplayResult<DisplayTopology> {
        let scanouts = self.read_display_info_blocking()?;
        let first = *scanouts.first().ok_or(DisplayError::Unspecified)?;
        let preferred = match self.read_edid_mode_blocking(first.id)? {
            Some(mode) => mode,
            None => DisplayMode::new(first.geometry.width, first.geometry.height),
        };
        Ok(DisplayTopology {
            scanouts,
            preferred,
        })
    }

    /// Acknowledges the device's interrupt and wakes whoever it was for.
    ///
    /// Two kinds of waiter exist: tasks parked on a queue completion,
    /// and the task following the display topology. A configuration
    /// change is the second one's signal, and the event word has to be
    /// cleared here — the device keeps the bit set until the driver
    /// writes it back — so that the next change raises a fresh
    /// interrupt.
    pub fn handle_interrupt(&self) {
        let status = self.transport.ack_interrupt();
        if status.config_change {
            let events = self.transport.read_config_u32(CONFIG_EVENTS_READ);
            if events & EVENT_DISPLAY != 0 {
                self.transport
                    .write_config_u32(CONFIG_EVENTS_CLEAR, EVENT_DISPLAY);
                self.display_events.notify_all();
            }
        }
        self.completions.notify_all();
    }

    /// Publishes one chain on the control queue and waits for the
    /// device to finish with it, reporting how many bytes it wrote.
    ///
    /// The shape both halves of this driver share: the 2D commands, and
    /// the rendering commands in [`render`]. The queue lock is held
    /// only long enough to publish the chain and is never held across
    /// an await, so several commands are in flight at once and
    /// completions are routed back by descriptor identifier.
    pub(crate) async fn control_exchange(
        &self,
        inputs: &[&[u8]],
        outputs: &mut [&mut [u8]],
    ) -> IoResult<u32> {
        self.control_exchange_with_payload(inputs, None, outputs)
            .await
    }

    /// [`Self::control_exchange`], with a run of memory this driver
    /// holds no pointer to carried between the request and the reply.
    ///
    /// The one command that has such a body is `SUBMIT_3D`, whose
    /// command buffer is the guest's own pinned pages: the device reads
    /// them where the guest wrote them.
    pub(crate) async fn control_exchange_with_payload(
        &self,
        inputs: &[&[u8]],
        payload: Option<PhysicalRange>,
        outputs: &mut [&mut [u8]],
    ) -> IoResult<u32> {
        let token = submit_chain_with_payload(
            &self.control_inflight,
            &self.control,
            &self.transport,
            inputs,
            payload,
            outputs,
        )
        .await?;
        Ok(
            await_completion(&self.control_inflight, &self.control, token, || {
                self.completions.notified()
            })
            .await,
        )
    }

    /// Runs one 2D control command and checks the response header.
    async fn control_command(
        &self,
        request: &[u8],
        entries: &[u8],
        response: &mut [u8],
        expected: u32,
        subject: RequestSubject,
    ) -> DisplayResult<()> {
        let all_inputs: [&[u8]; 2] = [request, entries];
        let inputs = if entries.is_empty() {
            &all_inputs[..1]
        } else {
            &all_inputs[..]
        };
        let written = self.control_exchange(inputs, &mut [&mut *response]).await?;
        if (written as usize) < CTRL_HEADER_BYTES {
            return Err(IoError::DeviceFault.into());
        }
        check_response(response, expected, subject)
    }

    /// Runs one cursor-queue command.
    ///
    /// The cursor queue carries no reply: the device consumes the
    /// command and returns the descriptor, so the completion is the
    /// whole answer.
    async fn cursor_command(&self, request: &[u8]) -> DisplayResult<()> {
        let mut outputs: [&mut [u8]; 0] = [];
        let token = submit_chain(
            &self.cursor_inflight,
            &self.cursor,
            &self.transport,
            &[request],
            &mut outputs,
        )
        .await?;
        await_completion(&self.cursor_inflight, &self.cursor, token, || {
            self.completions.notified()
        })
        .await;
        Ok(())
    }

    /// One control round trip on the bring-up path, polled rather than
    /// awaited. See [`VirtioGpuDevice::read_topology`].
    ///
    /// Both buffers are kernel-heap allocations rather than locals, and
    /// that is not a style choice: this runs on the boot stack, which
    /// the platform maps outside the window the device's DMA pool
    /// translates, so a device handed a stack address reads and writes
    /// memory that is not this buffer and answers with a completion
    /// whose reply never arrives. Every later command builds its buffers
    /// inside a task future, which lives in the kernel's task arena and
    /// translates correctly.
    fn blocking_command(
        &self,
        request: &[u8],
        response_bytes: usize,
        expected: u32,
    ) -> DisplayResult<Vec<u8>> {
        let request = request.to_vec();
        let mut response = vec![0_u8; response_bytes];
        let mut queue = self
            .control
            .try_lock()
            .expect("virtio-gpu bring-up runs alone on the bootstrap processor");
        let token = queue.submit(
            &self.transport,
            &[request.as_slice()],
            &mut [response.as_mut_slice()],
        )?;
        queue.notify(&self.transport);
        let written = queue.reap_blocking(&self.transport, token);
        drop(queue);
        if (written as usize) < CTRL_HEADER_BYTES {
            return Err(IoError::DeviceFault.into());
        }
        check_response(&response, expected, RequestSubject::none())?;
        Ok(response)
    }

    fn read_display_info_blocking(&self) -> DisplayResult<ScanoutList> {
        let request = encode_header(CMD_GET_DISPLAY_INFO);
        let response = self.blocking_command(&request, DISPLAY_INFO_BYTES, RESP_OK_DISPLAY_INFO)?;
        decode_display_info(&response, self.scanout_count)
    }

    fn read_edid_mode_blocking(&self, scanout: ScanoutId) -> DisplayResult<Option<DisplayMode>> {
        if !self.edid_supported() {
            return Ok(None);
        }
        let request = encode_get_edid(scanout);
        let response = self.blocking_command(&request, EDID_RESPONSE_BYTES, RESP_OK_EDID)?;
        Ok(decode_edid_preferred_mode(&response))
    }

    /// The record for `framebuffer`, or the error that says it is not
    /// this device's.
    fn record(&self, framebuffer: FramebufferId) -> DisplayResult<FramebufferRecord> {
        self.framebuffers
            .lock()
            .iter()
            .find(|record| record.id == framebuffer)
            .copied()
            .ok_or(DisplayError::UnknownFramebuffer(framebuffer))
    }

    /// Copies `region` of `framebuffer` into the device's own copy of
    /// the resource.
    async fn transfer_to_host(&self, record: FramebufferRecord, region: Rect) -> DisplayResult<()> {
        if !region.fits_in(record.mode) {
            return Err(DisplayError::RegionOutOfBounds {
                x: region.x,
                y: region.y,
                width: region.width,
                height: region.height,
                buffer_width: record.mode.width,
                buffer_height: record.mode.height,
            });
        }
        // Where the region's first pixel sits in the flat backing store.
        // 64-bit throughout: a 4K frame buffer's last row is already
        // past what a 32-bit product would hold for a taller mode.
        let bytes_per_pixel =
            u64::try_from(record.format.bytes_per_pixel()).map_err(|_| IoError::DeviceFault)?;
        let offset = (u64::from(region.y) * u64::from(record.mode.width) + u64::from(region.x))
            * bytes_per_pixel;
        let request = encode_transfer_to_host_2d(record.id, region, offset);
        let mut response = [0_u8; CTRL_HEADER_BYTES];
        self.control_command(
            &request,
            &[],
            &mut response,
            RESP_OK_NODATA,
            RequestSubject::framebuffer(record.id),
        )
        .await
    }
}

impl<T: VirtioTransport> helios_hal::display::DisplayDevice for VirtioGpuDevice<T> {
    async fn scanouts(&self) -> DisplayResult<ScanoutList> {
        let request = encode_header(CMD_GET_DISPLAY_INFO);
        let mut response = [0_u8; DISPLAY_INFO_BYTES];
        self.control_command(
            &request,
            &[],
            &mut response,
            RESP_OK_DISPLAY_INFO,
            RequestSubject::none(),
        )
        .await?;
        decode_display_info(&response, self.scanout_count)
    }

    async fn preferred_mode(&self, scanout: ScanoutId) -> DisplayResult<DisplayMode> {
        if self.edid_supported() {
            let request = encode_get_edid(scanout);
            let mut response = [0_u8; EDID_RESPONSE_BYTES];
            self.control_command(
                &request,
                &[],
                &mut response,
                RESP_OK_EDID,
                RequestSubject::scanout(scanout),
            )
            .await?;
            if let Some(mode) = decode_edid_preferred_mode(&response) {
                return Ok(mode);
            }
        }
        // Without an EDID — or with one that carries no detailed timing
        // — the host's own published geometry is the display's answer.
        let scanouts = helios_hal::display::DisplayDevice::scanouts(self).await?;
        scanouts
            .iter()
            .find(|info| info.id == scanout)
            .map(|info| DisplayMode::new(info.geometry.width, info.geometry.height))
            .ok_or(DisplayError::UnknownScanout(scanout))
    }

    async fn create_framebuffer(
        &self,
        mode: DisplayMode,
        format: PixelFormat,
        backing: &[PhysFrameRange],
    ) -> DisplayResult<FramebufferId> {
        if backing.len() > MAX_BACKING_RANGES {
            return Err(DisplayError::TooManyBackingRanges {
                ranges: backing.len(),
                limit: MAX_BACKING_RANGES,
            });
        }
        let frame_bytes = mode
            .frame_bytes(format)
            .ok_or(DisplayError::InvalidParameter)?;
        let backing_bytes: usize = backing.iter().map(|range| range.byte_size()).sum();
        if frame_bytes == 0 || backing_bytes < frame_bytes {
            return Err(DisplayError::InvalidParameter);
        }
        let entries = encode_mem_entries(backing).map_err(|error| match error {
            MemEntryError::Empty => DisplayError::InvalidParameter,
            MemEntryError::Unrepresentable => DisplayError::Transport(IoError::DeviceFault),
        })?;

        let id = FramebufferId::new(self.next_resource.fetch_add(1, Ordering::Relaxed));
        // The record is claimed before the device is told, so a second
        // task cannot fill the last slot while this one is in flight and
        // leave a resource the device holds and this table does not.
        {
            let mut framebuffers = self.framebuffers.lock();
            if framebuffers.is_full() {
                return Err(DisplayError::TooManyFramebuffers {
                    limit: MAX_FRAMEBUFFERS,
                });
            }
            framebuffers.push(FramebufferRecord { id, mode, format });
        }

        let create = encode_resource_create_2d(id, format, mode);
        let mut response = [0_u8; CTRL_HEADER_BYTES];
        let created = self
            .control_command(
                &create,
                &[],
                &mut response,
                RESP_OK_NODATA,
                RequestSubject::framebuffer(id),
            )
            .await;
        let attached = match created {
            Ok(()) => {
                let attach = encode_attach_backing(id, backing.len());
                self.control_command(
                    &attach,
                    &entries,
                    &mut response,
                    RESP_OK_NODATA,
                    RequestSubject::framebuffer(id),
                )
                .await
            }
            Err(error) => Err(error),
        };
        if let Err(error) = attached {
            self.framebuffers.lock().retain(|record| record.id != id);
            return Err(error);
        }
        Ok(id)
    }

    async fn destroy_framebuffer(&self, framebuffer: FramebufferId) -> DisplayResult<()> {
        let record = self.record(framebuffer)?;
        let mut response = [0_u8; CTRL_HEADER_BYTES];
        let detach = encode_resource_only(CMD_RESOURCE_DETACH_BACKING, record.id);
        self.control_command(
            &detach,
            &[],
            &mut response,
            RESP_OK_NODATA,
            RequestSubject::framebuffer(record.id),
        )
        .await?;
        let unref = encode_resource_only(CMD_RESOURCE_UNREF, record.id);
        self.control_command(
            &unref,
            &[],
            &mut response,
            RESP_OK_NODATA,
            RequestSubject::framebuffer(record.id),
        )
        .await?;
        // Only once the device has let the resource go: until then the
        // caller's pages are still on loan to it.
        self.framebuffers
            .lock()
            .retain(|held| held.id != framebuffer);
        Ok(())
    }

    async fn set_scanout(
        &self,
        scanout: ScanoutId,
        framebuffer: FramebufferId,
        source: Rect,
    ) -> DisplayResult<()> {
        let record = self.record(framebuffer)?;
        if !source.fits_in(record.mode) {
            return Err(DisplayError::RegionOutOfBounds {
                x: source.x,
                y: source.y,
                width: source.width,
                height: source.height,
                buffer_width: record.mode.width,
                buffer_height: record.mode.height,
            });
        }
        let request = encode_set_scanout(scanout, record.id, source);
        let mut response = [0_u8; CTRL_HEADER_BYTES];
        self.control_command(
            &request,
            &[],
            &mut response,
            RESP_OK_NODATA,
            RequestSubject::both(scanout, record.id),
        )
        .await
    }

    async fn blank_scanout(&self, scanout: ScanoutId) -> DisplayResult<()> {
        // virtio-gpu spells "show nothing" as a `SET_SCANOUT` naming the
        // reserved resource id and an empty rectangle (virtio 1.2
        // §5.7.6.8): there is no separate command, and a driver that
        // simply destroyed the resource would leave the device scanning
        // out memory it no longer has a reference to.
        let request =
            encode_set_scanout(scanout, FramebufferId::new(RESOURCE_NONE), Rect::default());
        let mut response = [0_u8; CTRL_HEADER_BYTES];
        self.control_command(
            &request,
            &[],
            &mut response,
            RESP_OK_NODATA,
            RequestSubject::scanout(scanout),
        )
        .await
    }

    async fn flush(&self, framebuffer: FramebufferId, region: Rect) -> DisplayResult<()> {
        let record = self.record(framebuffer)?;
        self.transfer_to_host(record, region).await?;
        let request = encode_resource_flush(record.id, region);
        let mut response = [0_u8; CTRL_HEADER_BYTES];
        self.control_command(
            &request,
            &[],
            &mut response,
            RESP_OK_NODATA,
            RequestSubject::framebuffer(record.id),
        )
        .await
    }

    async fn set_cursor(
        &self,
        scanout: ScanoutId,
        position: Point,
        image: CursorImage,
    ) -> DisplayResult<()> {
        let record = self.record(image.framebuffer)?;
        if record.mode != CursorImage::MODE {
            return Err(DisplayError::CursorSize {
                width: record.mode.width,
                height: record.mode.height,
            });
        }
        // The pointer's pixels have to be in the device's copy of the
        // resource before the plane is told to show it; a cursor is
        // never flushed, because nothing scans it out.
        self.transfer_to_host(record, Rect::of(CursorImage::MODE))
            .await?;
        let request = encode_update_cursor(
            CMD_UPDATE_CURSOR,
            scanout,
            position,
            record.id.raw(),
            image.hotspot,
        );
        self.cursor_command(&request).await
    }

    async fn hide_cursor(&self, scanout: ScanoutId) -> DisplayResult<()> {
        let request = encode_update_cursor(
            CMD_UPDATE_CURSOR,
            scanout,
            Point::new(0, 0),
            RESOURCE_NONE,
            Point::new(0, 0),
        );
        self.cursor_command(&request).await
    }

    async fn move_cursor(&self, scanout: ScanoutId, position: Point) -> DisplayResult<()> {
        let request = encode_update_cursor(
            CMD_MOVE_CURSOR,
            scanout,
            position,
            RESOURCE_NONE,
            Point::new(0, 0),
        );
        self.cursor_command(&request).await
    }

    async fn display_changed(&self) {
        self.display_events.notified().await;
    }
}

impl<T: VirtioTransport> Drop for VirtioGpuDevice<T> {
    fn drop(&mut self) {
        self.control.get_mut().shutdown(&self.transport);
        self.cursor.get_mut().shutdown(&self.transport);
    }
}

/// Reads the display topology and names the device on the boot log.
///
/// Both bring-up constructors do it, so the shape of the line — and the
/// fact that it is emitted once the device can actually answer for
/// itself — lives here rather than once per transport and once per
/// backend.
pub(crate) fn report_gpu_online<T: VirtioTransport>(
    device: &VirtioGpuDevice<T>,
    transport: &str,
) -> IoResult<DisplayTopology> {
    let topology = device.read_topology().map_err(|error| {
        tracing::error!(
            %error,
            "virtio-gpu did not answer the display-info request at bring-up"
        );
        IoError::DeviceFault
    })?;
    let edid = if device.edid_supported() { "on" } else { "off" };
    let scanouts = topology.scanouts.len();
    let width = topology.preferred.width;
    let height = topology.preferred.height;
    let features = device.features();
    // The renderer half, named on the same line as the scanouts,
    // because "this machine has no 3D" is a fact a lane reads off the
    // boot log rather than deduces from the absence of one.
    let three_d = if features.device(render::GPU_FEATURE_VIRGL) {
        "virgl"
    } else {
        "none"
    };
    let blob = on_off(features.device(render::GPU_FEATURE_RESOURCE_BLOB));
    let context_init = on_off(features.device(render::GPU_FEATURE_CONTEXT_INIT));
    tracing::info!(
        "virtio-gpu online transport={transport} scanouts={scanouts} \
         preferred={width}x{height} edid={edid} 3d={three_d} blob={blob} \
         context-init={context_init}"
    );
    Ok(topology)
}

const fn on_off(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

/// `struct virtio_gpu_ctrl_hdr` for a command that carries no payload.
fn encode_header(command: u32) -> [u8; CTRL_HEADER_BYTES] {
    let mut bytes = [0_u8; CTRL_HEADER_BYTES];
    bytes[0..4].copy_from_slice(&command.to_le_bytes());
    bytes
}

pub(crate) fn write_header(bytes: &mut [u8], command: u32) {
    bytes[0..4].copy_from_slice(&command.to_le_bytes());
}

fn write_rect(bytes: &mut [u8], rect: Rect) {
    bytes[0..4].copy_from_slice(&rect.x.to_le_bytes());
    bytes[4..8].copy_from_slice(&rect.y.to_le_bytes());
    bytes[8..12].copy_from_slice(&rect.width.to_le_bytes());
    bytes[12..16].copy_from_slice(&rect.height.to_le_bytes());
}

fn encode_get_edid(scanout: ScanoutId) -> [u8; GET_EDID_BYTES] {
    let mut bytes = [0_u8; GET_EDID_BYTES];
    write_header(&mut bytes, CMD_GET_EDID);
    bytes[CTRL_HEADER_BYTES..CTRL_HEADER_BYTES + 4].copy_from_slice(&scanout.index().to_le_bytes());
    bytes
}

fn encode_resource_create_2d(
    resource: FramebufferId,
    format: PixelFormat,
    mode: DisplayMode,
) -> [u8; CREATE_2D_BYTES] {
    let mut bytes = [0_u8; CREATE_2D_BYTES];
    write_header(&mut bytes, CMD_RESOURCE_CREATE_2D);
    let body = &mut bytes[CTRL_HEADER_BYTES..];
    body[0..4].copy_from_slice(&resource.raw().to_le_bytes());
    body[4..8].copy_from_slice(&wire_format(format).to_le_bytes());
    body[8..12].copy_from_slice(&mode.width.to_le_bytes());
    body[12..16].copy_from_slice(&mode.height.to_le_bytes());
    bytes
}

/// `RESOURCE_UNREF` and `RESOURCE_DETACH_BACKING` share a body: one
/// resource id and its padding.
pub(crate) fn encode_resource_only(
    command: u32,
    resource: FramebufferId,
) -> [u8; RESOURCE_ONLY_BYTES] {
    let mut bytes = [0_u8; RESOURCE_ONLY_BYTES];
    write_header(&mut bytes, command);
    bytes[CTRL_HEADER_BYTES..CTRL_HEADER_BYTES + 4].copy_from_slice(&resource.raw().to_le_bytes());
    bytes
}

fn encode_attach_backing(resource: FramebufferId, entries: usize) -> [u8; ATTACH_BACKING_BYTES] {
    let mut bytes = [0_u8; ATTACH_BACKING_BYTES];
    write_header(&mut bytes, CMD_RESOURCE_ATTACH_BACKING);
    let body = &mut bytes[CTRL_HEADER_BYTES..];
    body[0..4].copy_from_slice(&resource.raw().to_le_bytes());
    body[4..8].copy_from_slice(
        &u32::try_from(entries)
            .expect("the backing range count is bounded by MAX_BACKING_RANGES")
            .to_le_bytes(),
    );
    bytes
}

/// Why a backing store could not be put on the wire.
///
/// Two answers rather than one, because the callers spell them
/// differently: a range that describes no memory is the caller's
/// mistake, and a range no `virtio_gpu_mem_entry` can carry is a
/// machine whose memory does not fit the wire format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MemEntryError {
    /// One of the ranges covers no bytes.
    Empty,
    /// One of the ranges has an address or a length the entry cannot
    /// carry.
    Unrepresentable,
}

/// The `struct virtio_gpu_mem_entry` table that follows an attach or
/// blob-creation request.
///
/// The caller has already refused a backing store of more than
/// [`MAX_BACKING_RANGES`] ranges, which is what makes the table a value
/// rather than an allocation.
pub(crate) fn encode_mem_entries(
    backing: &[PhysFrameRange],
) -> Result<MemEntryTable, MemEntryError> {
    assert!(
        backing.len() <= MAX_BACKING_RANGES,
        "a backing store of {} ranges reached the encoder past the {MAX_BACKING_RANGES}-range check",
        backing.len()
    );
    let mut table = MemEntryTable::new();
    for range in backing {
        if range.is_empty() {
            return Err(MemEntryError::Empty);
        }
        let address =
            u64::try_from(range.start.phys_addr()).map_err(|_| MemEntryError::Unrepresentable)?;
        let length =
            u32::try_from(range.byte_size()).map_err(|_| MemEntryError::Unrepresentable)?;
        let mut entry = [0_u8; MEM_ENTRY_BYTES];
        entry[0..8].copy_from_slice(&address.to_le_bytes());
        entry[8..12].copy_from_slice(&length.to_le_bytes());
        table
            .try_extend_from_slice(&entry)
            .expect("the range count is bounded by MAX_BACKING_RANGES");
    }
    Ok(table)
}

fn encode_set_scanout(
    scanout: ScanoutId,
    resource: FramebufferId,
    source: Rect,
) -> [u8; RECT_COMMAND_BYTES] {
    let mut bytes = [0_u8; RECT_COMMAND_BYTES];
    write_header(&mut bytes, CMD_SET_SCANOUT);
    write_rect(&mut bytes[CTRL_HEADER_BYTES..], source);
    let tail = &mut bytes[CTRL_HEADER_BYTES + RECT_BYTES..];
    tail[0..4].copy_from_slice(&scanout.index().to_le_bytes());
    tail[4..8].copy_from_slice(&resource.raw().to_le_bytes());
    bytes
}

fn encode_resource_flush(resource: FramebufferId, region: Rect) -> [u8; RECT_COMMAND_BYTES] {
    let mut bytes = [0_u8; RECT_COMMAND_BYTES];
    write_header(&mut bytes, CMD_RESOURCE_FLUSH);
    write_rect(&mut bytes[CTRL_HEADER_BYTES..], region);
    bytes[CTRL_HEADER_BYTES + RECT_BYTES..CTRL_HEADER_BYTES + RECT_BYTES + 4]
        .copy_from_slice(&resource.raw().to_le_bytes());
    bytes
}

fn encode_transfer_to_host_2d(
    resource: FramebufferId,
    region: Rect,
    offset: u64,
) -> [u8; TRANSFER_2D_BYTES] {
    let mut bytes = [0_u8; TRANSFER_2D_BYTES];
    write_header(&mut bytes, CMD_TRANSFER_TO_HOST_2D);
    write_rect(&mut bytes[CTRL_HEADER_BYTES..], region);
    let tail = &mut bytes[CTRL_HEADER_BYTES + RECT_BYTES..];
    tail[0..8].copy_from_slice(&offset.to_le_bytes());
    tail[8..12].copy_from_slice(&resource.raw().to_le_bytes());
    bytes
}

fn encode_update_cursor(
    command: u32,
    scanout: ScanoutId,
    position: Point,
    resource: u32,
    hotspot: Point,
) -> [u8; UPDATE_CURSOR_BYTES] {
    let mut bytes = [0_u8; UPDATE_CURSOR_BYTES];
    write_header(&mut bytes, command);
    let pos = &mut bytes[CTRL_HEADER_BYTES..];
    pos[0..4].copy_from_slice(&scanout.index().to_le_bytes());
    pos[4..8].copy_from_slice(&position.x.to_le_bytes());
    pos[8..12].copy_from_slice(&position.y.to_le_bytes());
    let tail = &mut bytes[CTRL_HEADER_BYTES + 16..];
    tail[0..4].copy_from_slice(&resource.to_le_bytes());
    tail[4..8].copy_from_slice(&hotspot.x.to_le_bytes());
    tail[8..12].copy_from_slice(&hotspot.y.to_le_bytes());
    bytes
}

/// The wire number of a pixel format (`enum virtio_gpu_formats`, virtio
/// 1.2 §5.7.6.7).
const fn wire_format(format: PixelFormat) -> u32 {
    match format {
        PixelFormat::Bgra8888 => 1,
        PixelFormat::Bgrx8888 => 2,
        PixelFormat::Argb8888 => 3,
        PixelFormat::Xrgb8888 => 4,
        PixelFormat::Rgba8888 => 67,
        PixelFormat::Xbgr8888 => 68,
        PixelFormat::Abgr8888 => 121,
        PixelFormat::Rgbx8888 => 134,
    }
}

/// Turns a response header into either "this is the answer that was
/// asked for" or the typed refusal it carries.
fn check_response(response: &[u8], expected: u32, subject: RequestSubject) -> DisplayResult<()> {
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
        (RESP_ERR_UNSPEC, _) => DisplayError::Unspecified,
        (RESP_ERR_OUT_OF_MEMORY, _) => DisplayError::OutOfMemory,
        (RESP_ERR_INVALID_PARAMETER, _) => DisplayError::InvalidParameter,
        (
            RESP_ERR_INVALID_SCANOUT_ID,
            RequestSubject {
                scanout: Some(scanout),
                ..
            },
        ) => DisplayError::UnknownScanout(scanout),
        (
            RESP_ERR_INVALID_RESOURCE_ID,
            RequestSubject {
                framebuffer: Some(framebuffer),
                ..
            },
        ) => DisplayError::UnknownFramebuffer(framebuffer),
        (code, _) => DisplayError::UnexpectedResponse { code },
    })
}

/// Decodes `struct virtio_gpu_resp_display_info`.
///
/// Only the first `scanouts` entries are read: the reply always carries
/// sixteen, and the ones past the device's own count describe nothing.
fn decode_display_info(response: &[u8], scanouts: u32) -> DisplayResult<ScanoutList> {
    let mut list = ScanoutList::new();
    for index in 0..scanouts {
        let start = CTRL_HEADER_BYTES + DISPLAY_ONE_BYTES * index as usize;
        let entry = response
            .get(start..start + DISPLAY_ONE_BYTES)
            .ok_or(IoError::DeviceFault)?;
        let word = |offset: usize| {
            u32::from_le_bytes(
                entry[offset..offset + 4]
                    .try_into()
                    .expect("a four-byte window of a checked slice"),
            )
        };
        list.push(ScanoutInfo {
            id: ScanoutId::new(index),
            geometry: Rect::new(word(0), word(4), word(8), word(12)),
            enabled: word(16) != 0,
        });
    }
    Ok(list)
}

/// The preferred mode an EDID block names, when it carries one.
///
/// The first detailed timing descriptor is the display's preferred
/// timing (VESA E-EDID §3.10.1); a block whose first descriptor is not a
/// timing — a monitor name or a range limit — states no preference, and
/// a device that answers with no block at all states none either.
fn decode_edid_preferred_mode(response: &[u8]) -> Option<DisplayMode> {
    let size = u32::from_le_bytes(
        response
            .get(CTRL_HEADER_BYTES..CTRL_HEADER_BYTES + 4)?
            .try_into()
            .ok()?,
    ) as usize;
    let block = response.get(CTRL_HEADER_BYTES + 8..CTRL_HEADER_BYTES + 8 + size)?;
    if block.get(0..8)? != EDID_MAGIC {
        return None;
    }
    let timing = block.get(
        EDID_DETAILED_TIMING_OFFSET..EDID_DETAILED_TIMING_OFFSET + EDID_DETAILED_TIMING_BYTES,
    )?;
    // A descriptor whose pixel clock is zero is one of the display
    // descriptors, not a timing.
    if timing[0] == 0 && timing[1] == 0 {
        return None;
    }
    let width = (u32::from(timing[4] & 0xf0) << 4) | u32::from(timing[2]);
    let height = (u32::from(timing[7] & 0xf0) << 4) | u32::from(timing[5]);
    if width == 0 || height == 0 {
        return None;
    }
    Some(DisplayMode::new(width, height))
}

mod render;

#[cfg(test)]
mod tests;
