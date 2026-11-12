//! virtio-gpu driver tests.
//!
//! The device side is played by hand: a request is read out of the chain
//! the driver published, a canned reply is written into its writable
//! buffer, and the completion is raised. That is what lets a test assert
//! on the exact wire bytes the driver emits and on how it reads the
//! bytes a real device would answer with.

use core::future::Future;
use core::pin::{Pin, pin};

use alloc::vec::Vec;
use futures_lite::future::{block_on, poll_once};

use helios_hal::display::{
    CursorImage, DisplayDevice, DisplayError, DisplayMode, FramebufferId, MAX_BACKING_RANGES,
    PixelFormat, Point, Rect, ScanoutId,
};
use helios_hal::io::IoError;
use helios_hal::pmm::{PhysFrame, PhysFrameRange};

use core::sync::atomic::{AtomicBool, Ordering};

use helios_hal::device::DeviceRegionAttributes;
use helios_hal::display::{
    BlobId, BlobMemory, BlobRequest, BlobScanout, BlobUsage, CapsetId, CommandStream, ContextId,
    ContextName, FenceId, Gpu3d, Gpu3dError, Gpu3dResult, Gpu3dTable, RingCount, RingIndex,
    Submission,
};
use helios_hal::iommu::PhysicalRange;

use super::render::{
    APERTURE_ALIGN, BLOB_MEM_GUEST, BLOB_MEM_HOST3D, CMD_CTX_ATTACH_RESOURCE, CMD_CTX_CREATE,
    CMD_CTX_DESTROY, CMD_CTX_DETACH_RESOURCE, CMD_GET_CAPSET, CMD_GET_CAPSET_INFO,
    CMD_RESOURCE_CREATE_BLOB, CMD_RESOURCE_MAP_BLOB, CMD_RESOURCE_UNMAP_BLOB, CMD_SET_SCANOUT_BLOB,
    CMD_SUBMIT_3D, CTRL_FLAG_FENCE, CTRL_FLAG_INFO_RING_IDX, CTRL_RING_IDX_OFFSET,
    GPU_FEATURE_BLOB_ALIGNMENT, GPU_FEATURE_CONTEXT_INIT, GPU_FEATURE_RESOURCE_BLOB,
    GPU_FEATURE_RESOURCE_UUID, GPU_FEATURE_VIRGL, MAP_CACHE_CACHED, MAP_CACHE_UNCACHED,
    MAP_CACHE_WC, MAX_CONTEXTS, RESP_CAPSET_INFO_BYTES, RESP_ERR_INVALID_CONTEXT_ID,
    RESP_MAP_INFO_BYTES, RESP_OK_CAPSET, RESP_OK_CAPSET_INFO, RESP_OK_MAP_INFO,
    SET_SCANOUT_BLOB_BYTES, SHM_ID_HOST_VISIBLE, SHM_ID_UNDEFINED,
};
use super::{
    CMD_GET_DISPLAY_INFO, CMD_MOVE_CURSOR, CMD_RESOURCE_ATTACH_BACKING, CMD_RESOURCE_CREATE_2D,
    CMD_RESOURCE_DETACH_BACKING, CMD_RESOURCE_FLUSH, CMD_RESOURCE_UNREF, CMD_SET_SCANOUT,
    CMD_TRANSFER_TO_HOST_2D, CMD_UPDATE_CURSOR, CTRL_HEADER_BYTES, DISPLAY_ONE_BYTES,
    DisplayTopology, RESP_ERR_INVALID_RESOURCE_ID, RESP_ERR_INVALID_SCANOUT_ID,
    RESP_ERR_OUT_OF_MEMORY, RESP_OK_DISPLAY_INFO, RESP_OK_NODATA, VirtioGpuDevice,
    decode_display_info, decode_edid_preferred_mode, online_line,
};
use crate::testing::{FakeTransport, FakeTransportConfig};
use crate::transport::{DeviceType, VirtioFeatures, VirtioTransport};

/// `VIRTIO_GPU_F_EDID`, as a device would offer it.
const OFFERED_EDID: u64 = 1 << 1;

const CONFIG_NUM_SCANOUTS: usize = 8;
const CONFIG_NUM_CAPSETS: usize = 12;

/// A device with `scanouts` outputs and no EDID support.
fn device_with(scanouts: u32, offered_features: u64) -> VirtioGpuDevice<FakeTransport> {
    let transport = FakeTransport::new(FakeTransportConfig {
        device_type: DeviceType::Gpu,
        offered_features: VirtioFeatures::VERSION_1.bits() | offered_features,
        queue_size: 8,
        supports_queue_reset: false,
        absent_queues: &[],
    });
    transport.set_config_u32(CONFIG_NUM_SCANOUTS, scanouts);
    transport.set_config_u32(CONFIG_NUM_CAPSETS, 0);
    VirtioGpuDevice::new(transport).expect("the display device should initialize")
}

fn device() -> VirtioGpuDevice<FakeTransport> {
    device_with(1, 0)
}

/// Polls `future` once, expecting it to park on a control command, and
/// hands back the descriptor that command was published under.
///
/// The descriptor is read before the poll rather than assumed: a chain
/// takes as many descriptors as it has buffers and gives them back when
/// its completion is drained, so which identifier a command lands on is
/// the ring's business and not a number a test may spell out.
fn pending_control<Output>(
    device: &VirtioGpuDevice<FakeTransport>,
    future: Pin<&mut impl Future<Output = Output>>,
) -> u16 {
    let token = device
        .control
        .try_lock()
        .expect("a parked driver does not hold the control queue lock")
        .next_free_descriptor();
    assert!(
        block_on(poll_once(future)).is_none(),
        "the command is still with the device"
    );
    token
}

/// The cursor-queue counterpart of [`pending_control`].
fn pending_cursor<Output>(
    device: &VirtioGpuDevice<FakeTransport>,
    future: Pin<&mut impl Future<Output = Output>>,
) -> u16 {
    let token = device
        .cursor
        .try_lock()
        .expect("a parked driver does not hold the cursor queue lock")
        .next_free_descriptor();
    assert!(
        block_on(poll_once(future)).is_none(),
        "the command is still with the device"
    );
    token
}

/// The bytes the driver made readable in control chain `token`.
fn control_request(device: &VirtioGpuDevice<FakeTransport>, token: u16) -> Vec<u8> {
    device
        .control
        .try_lock()
        .expect("a parked driver does not hold the control queue lock")
        .device_request(token)
}

/// The bytes the driver made readable in cursor chain `token`.
fn cursor_request(device: &VirtioGpuDevice<FakeTransport>, token: u16) -> Vec<u8> {
    device
        .cursor
        .try_lock()
        .expect("a parked driver does not hold the cursor queue lock")
        .device_request(token)
}

/// Plays the device: writes `response` into control chain `token`'s
/// writable buffer and raises the interrupt.
fn answer_control(device: &VirtioGpuDevice<FakeTransport>, token: u16, response: &[u8]) {
    let written = device
        .control
        .try_lock()
        .expect("a parked driver does not hold the control queue lock")
        .device_respond(token, response);
    device
        .control
        .try_lock()
        .expect("a parked driver does not hold the control queue lock")
        .device_complete(token, written);
    device.handle_interrupt();
}

/// Plays the device on the cursor queue, which carries no reply: the
/// descriptor simply comes back.
fn answer_cursor(device: &VirtioGpuDevice<FakeTransport>, token: u16) {
    device
        .cursor
        .try_lock()
        .expect("a parked driver does not hold the cursor queue lock")
        .device_complete(token, 0);
    device.handle_interrupt();
}

/// A bare `virtio_gpu_ctrl_hdr` reply.
fn header_response(code: u32) -> [u8; CTRL_HEADER_BYTES] {
    let mut bytes = [0_u8; CTRL_HEADER_BYTES];
    bytes[0..4].copy_from_slice(&code.to_le_bytes());
    bytes
}

/// A `virtio_gpu_resp_display_info` naming `modes` outputs.
fn display_info_response(modes: &[(Rect, bool)]) -> Vec<u8> {
    let mut bytes = alloc::vec![0_u8; CTRL_HEADER_BYTES + DISPLAY_ONE_BYTES * 16];
    bytes[0..4].copy_from_slice(&RESP_OK_DISPLAY_INFO.to_le_bytes());
    for (index, (rect, enabled)) in modes.iter().enumerate() {
        let entry = CTRL_HEADER_BYTES + DISPLAY_ONE_BYTES * index;
        bytes[entry..entry + 4].copy_from_slice(&rect.x.to_le_bytes());
        bytes[entry + 4..entry + 8].copy_from_slice(&rect.y.to_le_bytes());
        bytes[entry + 8..entry + 12].copy_from_slice(&rect.width.to_le_bytes());
        bytes[entry + 12..entry + 16].copy_from_slice(&rect.height.to_le_bytes());
        bytes[entry + 16..entry + 20].copy_from_slice(&u32::from(*enabled).to_le_bytes());
    }
    bytes
}

fn command_of(request: &[u8]) -> u32 {
    u32::from_le_bytes(request[0..4].try_into().expect("a control header"))
}

fn word_at(request: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        request[offset..offset + 4]
            .try_into()
            .expect("a four-byte field"),
    )
}

/// One page of physical memory to back a small frame buffer with.
fn backing(frames: usize) -> [PhysFrameRange; 1] {
    [PhysFrameRange {
        start: PhysFrame::from_phys_addr(0x4000_0000),
        frame_count: frames,
    }]
}

#[test]
fn a_wrong_device_type_is_rejected() {
    let rejected = VirtioGpuDevice::new(FakeTransport::new(FakeTransportConfig {
        device_type: DeviceType::Block,
        ..FakeTransportConfig::default()
    }))
    .err();
    assert_eq!(rejected, Some(IoError::Unsupported));
}

/// A device that claims no output, or more outputs than the reply can
/// describe, is a device the driver cannot drive.
#[test]
fn an_impossible_scanout_count_is_refused_at_bring_up() {
    for count in [0, 17, u32::MAX] {
        let transport = FakeTransport::new(FakeTransportConfig {
            device_type: DeviceType::Gpu,
            offered_features: VirtioFeatures::VERSION_1.bits(),
            queue_size: 8,
            supports_queue_reset: false,
            absent_queues: &[],
        });
        transport.set_config_u32(CONFIG_NUM_SCANOUTS, count);
        assert!(
            matches!(
                VirtioGpuDevice::new(transport),
                Err(IoError::InvalidDeviceConfig(_))
            ),
            "a scanout count of {count} must be refused"
        );
    }
}

/// A device with no renderer negotiates exactly what a 2D-only driver
/// did, however loudly it offers the resource features around it: blob
/// resources and context types are the renderer's vocabulary, and a
/// driver that took them from a device with no renderer would have told
/// it to expect commands nothing will ever send.
#[test]
fn a_device_without_a_renderer_negotiates_no_three_dimensional_feature() {
    let device = device_with(
        1,
        OFFERED_EDID | GPU_FEATURE_RESOURCE_BLOB | GPU_FEATURE_CONTEXT_INIT,
    );

    assert!(device.edid_supported(), "EDID is the one class bit wanted");
    let features = device.features();
    for unwanted in [
        GPU_FEATURE_VIRGL,
        GPU_FEATURE_RESOURCE_BLOB,
        GPU_FEATURE_CONTEXT_INIT,
    ] {
        assert!(
            !features.device(unwanted),
            "feature {unwanted:#x} must not be negotiated against a device with no renderer"
        );
    }
    assert!(
        !Gpu3d::renders(&device),
        "a device with no VIRGL renders nothing, which is what the boot line's 3d=none says"
    );
}

/// A device that carries a renderer gets the whole 3D set it offers.
#[test]
fn a_device_with_a_renderer_negotiates_the_blob_and_context_features_it_offers() {
    let device = device_with(
        1,
        OFFERED_EDID
            | GPU_FEATURE_VIRGL
            | GPU_FEATURE_RESOURCE_UUID
            | GPU_FEATURE_RESOURCE_BLOB
            | GPU_FEATURE_CONTEXT_INIT,
    );

    let features = device.features();
    for wanted in [
        GPU_FEATURE_VIRGL,
        GPU_FEATURE_RESOURCE_UUID,
        GPU_FEATURE_RESOURCE_BLOB,
        GPU_FEATURE_CONTEXT_INIT,
    ] {
        assert!(
            features.device(wanted),
            "feature {wanted:#x} is offered by a rendering device and has to be taken"
        );
    }
    assert!(Gpu3d::renders(&device));
}

/// A renderer that offers no blob resources still gets its contexts:
/// the bits are negotiated one at a time out of what was offered, not
/// as a set that is taken whole or not at all.
#[test]
fn the_three_dimensional_features_are_taken_one_at_a_time() {
    let device = device_with(
        1,
        OFFERED_EDID | GPU_FEATURE_VIRGL | GPU_FEATURE_CONTEXT_INIT,
    );

    let features = device.features();
    assert!(features.device(GPU_FEATURE_VIRGL));
    assert!(features.device(GPU_FEATURE_CONTEXT_INIT));
    assert!(!features.device(GPU_FEATURE_RESOURCE_BLOB));
}

/// The `virtio-gpu online` line is what a lane greps: it names the
/// transport and the topology and then reports each negotiated renderer
/// feature in a fixed order, `uuid=` included.
#[test]
fn the_online_line_reports_what_was_negotiated() {
    let topology = DisplayTopology {
        scanouts: decode_display_info(
            &display_info_response(&[(Rect::new(0, 0, 1280, 800), true)]),
            1,
        )
        .expect("the reply describes one scanout"),
        preferred: DisplayMode::new(1280, 800),
    };

    let rendered = render_device(1);
    assert_eq!(
        online_line(&rendered, "pci", &topology),
        "virtio-gpu online transport=pci scanouts=1 preferred=1280x800 \
         edid=on 3d=virgl blob=on context-init=on uuid=on"
    );

    let plain = device();
    assert_eq!(
        online_line(&plain, "mmio", &topology),
        "virtio-gpu online transport=mmio scanouts=1 preferred=1280x800 \
         edid=off 3d=none blob=off context-init=off uuid=off"
    );
}

#[test]
fn display_info_is_parsed_into_one_entry_per_scanout() {
    let response = display_info_response(&[
        (Rect::new(0, 0, 1280, 800), true),
        (Rect::new(1280, 0, 1920, 1080), false),
    ]);

    let list = decode_display_info(&response, 2).expect("the reply describes two scanouts");

    assert_eq!(list.len(), 2);
    assert_eq!(list[0].id, ScanoutId::new(0));
    assert_eq!(list[0].geometry, Rect::new(0, 0, 1280, 800));
    assert!(list[0].enabled);
    assert_eq!(list[1].id, ScanoutId::new(1));
    assert_eq!(list[1].geometry, Rect::new(1280, 0, 1920, 1080));
    assert!(!list[1].enabled, "the host is not presenting the second");
}

/// The reply always carries sixteen entries; the ones past the device's
/// own count describe nothing and must not become scanouts.
#[test]
fn display_info_entries_past_the_device_count_are_not_read() {
    let response = display_info_response(&[
        (Rect::new(0, 0, 800, 600), true),
        (Rect::new(0, 0, 640, 480), true),
    ]);

    let list = decode_display_info(&response, 1).expect("one scanout");

    assert_eq!(list.len(), 1);
    assert_eq!(list[0].geometry, Rect::new(0, 0, 800, 600));
}

#[test]
fn a_truncated_display_info_reply_is_a_device_fault() {
    let short = alloc::vec![0_u8; CTRL_HEADER_BYTES + DISPLAY_ONE_BYTES];
    assert_eq!(
        decode_display_info(&short, 2),
        Err(DisplayError::Transport(IoError::DeviceFault))
    );
}

#[test]
fn scanouts_asks_the_device_and_returns_what_it_answered() {
    let device = device();
    let mut scanouts = pin!(DisplayDevice::scanouts(&device));

    let token = pending_control(&device, scanouts.as_mut());
    assert_eq!(
        command_of(&control_request(&device, token)),
        CMD_GET_DISPLAY_INFO
    );

    answer_control(
        &device,
        token,
        &display_info_response(&[(Rect::new(0, 0, 1024, 768), true)]),
    );

    let list = block_on(poll_once(scanouts.as_mut()))
        .expect("the reply is in")
        .expect("the device answered with display info");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].geometry, Rect::new(0, 0, 1024, 768));
}

/// Without EDID the host's published geometry is the display's answer.
#[test]
fn the_preferred_mode_falls_back_to_the_published_geometry() {
    let device = device();
    let mut preferred = pin!(device.preferred_mode(ScanoutId::new(0)));

    let token = pending_control(&device, preferred.as_mut());
    assert_eq!(
        command_of(&control_request(&device, token)),
        CMD_GET_DISPLAY_INFO
    );
    answer_control(
        &device,
        token,
        &display_info_response(&[(Rect::new(0, 0, 1440, 900), true)]),
    );

    assert_eq!(
        block_on(poll_once(preferred.as_mut())),
        Some(Ok(DisplayMode::new(1440, 900)))
    );
}

/// The whole create/attach/scanout/transfer/flush sequence, checked
/// command by command against the wire layout of virtio 1.2 §5.7.6.
#[test]
fn a_frame_reaches_a_scanout_through_the_documented_command_sequence() {
    let device = device();
    let pages = backing(16);
    let mode = DisplayMode::new(64, 32);

    let mut create = pin!(device.create_framebuffer(mode, PixelFormat::Bgrx8888, &pages));
    let token = pending_control(&device, create.as_mut());

    let request = control_request(&device, token);
    assert_eq!(command_of(&request), CMD_RESOURCE_CREATE_2D);
    let resource = word_at(&request, CTRL_HEADER_BYTES);
    assert_eq!(resource, 1, "resource ids start at one; zero means none");
    assert_eq!(
        word_at(&request, CTRL_HEADER_BYTES + 4),
        2,
        "VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM"
    );
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES + 8), 64);
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES + 12), 32);
    answer_control(&device, token, &header_response(RESP_OK_NODATA));

    let token = pending_control(&device, create.as_mut());
    let request = control_request(&device, token);
    assert_eq!(command_of(&request), CMD_RESOURCE_ATTACH_BACKING);
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES), resource);
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES + 4), 1, "one range");
    // The memory-entry table rides in the same chain as its request.
    let entry = CTRL_HEADER_BYTES + 8;
    assert_eq!(
        u64::from_le_bytes(request[entry..entry + 8].try_into().expect("an address")),
        0x4000_0000
    );
    assert_eq!(word_at(&request, entry + 8), 16 * 4096, "sixteen pages");
    answer_control(&device, token, &header_response(RESP_OK_NODATA));

    let framebuffer = block_on(poll_once(create.as_mut()))
        .expect("the attach is answered")
        .expect("the frame buffer is created");
    assert_eq!(framebuffer, FramebufferId::new(resource));

    let mut scanout = pin!(device.set_scanout(ScanoutId::new(0), framebuffer, Rect::of(mode)));
    let token = pending_control(&device, scanout.as_mut());
    let request = control_request(&device, token);
    assert_eq!(command_of(&request), CMD_SET_SCANOUT);
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES + 8), 64, "rect width");
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES + 16), 0, "scanout id");
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES + 20), resource);
    answer_control(&device, token, &header_response(RESP_OK_NODATA));
    assert_eq!(block_on(poll_once(scanout.as_mut())), Some(Ok(())));

    // A damaged region halfway down the frame buffer: the transfer's
    // offset is where that row starts in the caller's pages.
    let damage = Rect::new(8, 4, 16, 8);
    let mut flush = pin!(device.flush(framebuffer, damage));
    let token = pending_control(&device, flush.as_mut());
    let request = control_request(&device, token);
    assert_eq!(command_of(&request), CMD_TRANSFER_TO_HOST_2D);
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES), 8, "rect x");
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES + 4), 4, "rect y");
    let offset_at = CTRL_HEADER_BYTES + 16;
    assert_eq!(
        u64::from_le_bytes(
            request[offset_at..offset_at + 8]
                .try_into()
                .expect("an offset")
        ),
        (4 * 64 + 8) * 4,
        "row four, column eight, four bytes a pixel"
    );
    assert_eq!(word_at(&request, offset_at + 8), resource);
    answer_control(&device, token, &header_response(RESP_OK_NODATA));

    let token = pending_control(&device, flush.as_mut());
    let request = control_request(&device, token);
    assert_eq!(command_of(&request), CMD_RESOURCE_FLUSH);
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES + 8), 16, "rect width");
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES + 16), resource);
    answer_control(&device, token, &header_response(RESP_OK_NODATA));
    assert_eq!(block_on(poll_once(flush.as_mut())), Some(Ok(())));
}

#[test]
fn destroying_a_frame_buffer_detaches_the_backing_before_dropping_it() {
    let device = device();
    let framebuffer = create_framebuffer(&device, DisplayMode::new(64, 32));

    let mut destroy = pin!(device.destroy_framebuffer(framebuffer));
    let token = pending_control(&device, destroy.as_mut());
    assert_eq!(
        command_of(&control_request(&device, token)),
        CMD_RESOURCE_DETACH_BACKING,
        "the caller's pages are handed back before the resource goes"
    );
    answer_control(&device, token, &header_response(RESP_OK_NODATA));

    let token = pending_control(&device, destroy.as_mut());
    assert_eq!(
        command_of(&control_request(&device, token)),
        CMD_RESOURCE_UNREF
    );
    answer_control(&device, token, &header_response(RESP_OK_NODATA));
    assert_eq!(block_on(poll_once(destroy.as_mut())), Some(Ok(())));

    // The frame buffer is gone from the device's table too.
    assert_eq!(
        block_on(device.flush(framebuffer, Rect::new(0, 0, 1, 1))),
        Err(DisplayError::UnknownFramebuffer(framebuffer))
    );
}

/// Drives the create sequence to completion and hands back the frame
/// buffer, for the tests whose subject is what comes after it.
fn create_framebuffer(device: &VirtioGpuDevice<FakeTransport>, mode: DisplayMode) -> FramebufferId {
    let pages = backing(64);
    let mut create = pin!(device.create_framebuffer(mode, PixelFormat::Bgra8888, &pages));
    let token = pending_control(device, create.as_mut());
    answer_control(device, token, &header_response(RESP_OK_NODATA));
    let token = pending_control(device, create.as_mut());
    answer_control(device, token, &header_response(RESP_OK_NODATA));
    block_on(poll_once(create.as_mut()))
        .expect("the attach is answered")
        .expect("the frame buffer is created")
}

#[test]
fn the_cursor_image_goes_up_the_control_queue_and_the_placement_up_the_cursor_queue() {
    let device = device();
    let cursor = create_framebuffer(&device, CursorImage::MODE);

    let image = CursorImage {
        framebuffer: cursor,
        hotspot: Point::new(3, 5),
    };
    let mut set = pin!(device.set_cursor(ScanoutId::new(0), Point::new(400, 300), image));

    // The pointer's pixels reach the device's copy of the resource
    // first, on the control queue.
    let token = pending_control(&device, set.as_mut());
    let request = control_request(&device, token);
    assert_eq!(command_of(&request), CMD_TRANSFER_TO_HOST_2D);
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES + 8), CursorImage::WIDTH);
    assert_eq!(
        word_at(&request, CTRL_HEADER_BYTES + 12),
        CursorImage::HEIGHT
    );
    answer_control(&device, token, &header_response(RESP_OK_NODATA));

    // The placement goes up the cursor queue, which carries no reply.
    let token = pending_cursor(&device, set.as_mut());
    let request = cursor_request(&device, token);
    assert_eq!(command_of(&request), CMD_UPDATE_CURSOR);
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES), 0, "scanout id");
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES + 4), 400);
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES + 8), 300);
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES + 16), cursor.raw());
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES + 20), 3, "hotspot x");
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES + 24), 5, "hotspot y");
    answer_cursor(&device, token);
    assert_eq!(block_on(poll_once(set.as_mut())), Some(Ok(())));
}

/// Pointer motion is one command on the cursor queue and touches no
/// pixels at all: that is the whole point of a cursor plane.
#[test]
fn moving_the_pointer_costs_one_cursor_command_and_no_frame_traffic() {
    let device = device();
    let mut moved = pin!(device.move_cursor(ScanoutId::new(0), Point::new(12, 34)));

    let token = pending_cursor(&device, moved.as_mut());
    let request = cursor_request(&device, token);
    assert_eq!(command_of(&request), CMD_MOVE_CURSOR);
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES + 4), 12);
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES + 8), 34);
    answer_cursor(&device, token);
    assert_eq!(block_on(poll_once(moved.as_mut())), Some(Ok(())));

    assert!(
        device
            .control
            .try_lock()
            .expect("the control queue is idle")
            .available_descriptors()
            > 0,
        "no control chain was published for a pointer move"
    );
}

/// Releasing a frame buffer is not enough: a scanout still pointed at
/// the resource would keep the display engine reading pages that have
/// gone back to their owner, so blanking has to reach the device as its
/// own command before anything is destroyed.
#[test]
fn blanking_a_scanout_names_the_reserved_resource_and_an_empty_rectangle() {
    let device = device();
    let mut blanked = pin!(device.blank_scanout(ScanoutId::new(0)));

    let token = pending_control(&device, blanked.as_mut());
    let request = control_request(&device, token);
    assert_eq!(command_of(&request), CMD_SET_SCANOUT);
    for (index, field) in ["x", "y", "width", "height"].iter().enumerate() {
        assert_eq!(
            word_at(&request, CTRL_HEADER_BYTES + index * 4),
            0,
            "a blanked scanout shows an empty rectangle, but {field} was not zero"
        );
    }
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES + 16), 0, "scanout zero");
    assert_eq!(
        word_at(&request, CTRL_HEADER_BYTES + 20),
        0,
        "resource zero is how a scanout is switched off"
    );
    answer_control(&device, token, &header_response(RESP_OK_NODATA));
    assert_eq!(block_on(poll_once(blanked.as_mut())), Some(Ok(())));
}

#[test]
fn hiding_the_pointer_names_the_reserved_resource() {
    let device = device();
    let mut hidden = pin!(device.hide_cursor(ScanoutId::new(0)));

    let token = pending_cursor(&device, hidden.as_mut());
    let request = cursor_request(&device, token);
    assert_eq!(command_of(&request), CMD_UPDATE_CURSOR);
    assert_eq!(
        word_at(&request, CTRL_HEADER_BYTES + 16),
        0,
        "resource zero is how the plane is switched off"
    );
    answer_cursor(&device, token);
    assert_eq!(block_on(poll_once(hidden.as_mut())), Some(Ok(())));
}

/// A cursor plane is a fixed size, and a frame buffer that is not it
/// never reaches the device.
#[test]
fn a_cursor_image_of_the_wrong_size_is_refused_before_the_device_sees_it() {
    let device = device();
    let framebuffer = create_framebuffer(&device, DisplayMode::new(32, 32));

    let refused = block_on(device.set_cursor(
        ScanoutId::new(0),
        Point::new(0, 0),
        CursorImage {
            framebuffer,
            hotspot: Point::new(0, 0),
        },
    ));

    assert_eq!(
        refused,
        Err(DisplayError::CursorSize {
            width: 32,
            height: 32
        })
    );
}

#[test]
fn every_error_response_becomes_the_refusal_it_names() {
    let device = device();
    let framebuffer = create_framebuffer(&device, DisplayMode::new(64, 32));

    // An unknown scanout: the request named one, so the refusal can say
    // which.
    let mut scanout =
        pin!(device.set_scanout(ScanoutId::new(0), framebuffer, Rect::new(0, 0, 64, 32)));
    let token = pending_control(&device, scanout.as_mut());
    answer_control(
        &device,
        token,
        &header_response(RESP_ERR_INVALID_SCANOUT_ID),
    );
    assert_eq!(
        block_on(poll_once(scanout.as_mut())),
        Some(Err(DisplayError::UnknownScanout(ScanoutId::new(0))))
    );

    // An unknown resource, on a request that named one.
    let mut flush = pin!(device.flush(framebuffer, Rect::new(0, 0, 64, 32)));
    let token = pending_control(&device, flush.as_mut());
    answer_control(
        &device,
        token,
        &header_response(RESP_ERR_INVALID_RESOURCE_ID),
    );
    assert_eq!(
        block_on(poll_once(flush.as_mut())),
        Some(Err(DisplayError::UnknownFramebuffer(framebuffer)))
    );

    // Out of memory is a resource condition, not a caller bug, and stays
    // distinct from both.
    let mut flush = pin!(device.flush(framebuffer, Rect::new(0, 0, 64, 32)));
    let token = pending_control(&device, flush.as_mut());
    answer_control(&device, token, &header_response(RESP_ERR_OUT_OF_MEMORY));
    assert_eq!(
        block_on(poll_once(flush.as_mut())),
        Some(Err(DisplayError::OutOfMemory))
    );
}

/// A code that belongs to no request this driver issues is a fault
/// rather than a refusal, and carries the code so the fault can be
/// diagnosed.
#[test]
fn an_unknown_response_code_is_reported_as_the_fault_it_is() {
    let device = device();
    let mut scanouts = pin!(DisplayDevice::scanouts(&device));

    let token = pending_control(&device, scanouts.as_mut());
    answer_control(&device, token, &header_response(0x1204));

    assert_eq!(
        block_on(poll_once(scanouts.as_mut())),
        Some(Err(DisplayError::UnexpectedResponse { code: 0x1204 })),
        "an invalid-context refusal answers a question a 2D driver never asked"
    );
}

/// A backing store that does not cover the mode would have the device
/// read past the caller's pages.
#[test]
fn a_backing_store_too_small_for_the_mode_is_refused() {
    let device = device();
    let pages = backing(1);

    let refused = block_on(device.create_framebuffer(
        DisplayMode::new(1024, 768),
        PixelFormat::Bgrx8888,
        &pages,
    ));

    assert_eq!(refused, Err(DisplayError::InvalidParameter));
    assert_eq!(
        device.transport.kick_count(),
        0,
        "nothing reached the device"
    );
}

#[test]
fn a_backing_store_of_too_many_ranges_is_refused() {
    let device = device();
    let ranges: alloc::vec::Vec<PhysFrameRange> = (0..=MAX_BACKING_RANGES)
        .map(|index| PhysFrameRange {
            start: PhysFrame::from_index(0x4_0000 + index),
            frame_count: 1,
        })
        .collect();

    let refused = block_on(device.create_framebuffer(
        DisplayMode::new(64, 32),
        PixelFormat::Bgrx8888,
        &ranges,
    ));

    assert_eq!(
        refused,
        Err(DisplayError::TooManyBackingRanges {
            ranges: MAX_BACKING_RANGES + 1,
            limit: MAX_BACKING_RANGES,
        })
    );
}

/// A damaged region outside the frame buffer would have the device read
/// somebody else's memory.
#[test]
fn a_region_outside_the_frame_buffer_never_reaches_the_device() {
    let device = device();
    let framebuffer = create_framebuffer(&device, DisplayMode::new(64, 32));
    let kicks = device.transport.kick_count();

    let refused = block_on(device.flush(framebuffer, Rect::new(60, 30, 8, 8)));

    assert_eq!(
        refused,
        Err(DisplayError::RegionOutOfBounds {
            x: 60,
            y: 30,
            width: 8,
            height: 8,
            buffer_width: 64,
            buffer_height: 32,
        })
    );
    assert_eq!(device.transport.kick_count(), kicks);
}

/// A device configuration change carrying the display event wakes the
/// topology watcher, and the event word is cleared so the next change
/// raises a fresh interrupt.
#[test]
fn a_display_event_wakes_the_topology_watcher_and_is_cleared() {
    const CONFIG_EVENTS_READ: usize = 0;
    const CONFIG_EVENTS_CLEAR: usize = 4;
    const ISR_CONFIG_CHANGE: u32 = 1 << 1;

    let device = device();
    let mut changed = pin!(DisplayDevice::display_changed(&device));
    assert!(block_on(poll_once(changed.as_mut())).is_none());

    device.transport.set_config_u32(CONFIG_EVENTS_READ, 1);
    device.transport.raise_interrupt(ISR_CONFIG_CHANGE);
    device.handle_interrupt();

    assert_eq!(block_on(poll_once(changed.as_mut())), Some(()));
    assert_eq!(
        device.transport.read_config_u32(CONFIG_EVENTS_CLEAR),
        1,
        "the driver writes the bit back or the device keeps it set"
    );
}

/// An interrupt that carries no display event must not wake the
/// topology watcher: the device only says the set of scanouts changed
/// through that one bit.
#[test]
fn a_completion_interrupt_does_not_look_like_a_display_change() {
    const ISR_USED_BUFFER: u32 = 1 << 0;

    let device = device();
    let mut changed = pin!(DisplayDevice::display_changed(&device));
    assert!(block_on(poll_once(changed.as_mut())).is_none());

    device.transport.raise_interrupt(ISR_USED_BUFFER);
    device.handle_interrupt();

    assert!(block_on(poll_once(changed.as_mut())).is_none());
}

/// The preferred timing of an EDID block is its first detailed timing
/// descriptor.
#[test]
fn an_edid_block_yields_its_first_detailed_timing() {
    let mut response = alloc::vec![0_u8; CTRL_HEADER_BYTES + 8 + 128];
    response[CTRL_HEADER_BYTES..CTRL_HEADER_BYTES + 4].copy_from_slice(&128_u32.to_le_bytes());
    let block = CTRL_HEADER_BYTES + 8;
    response[block..block + 8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
    let timing = block + 54;
    // A 1920x1080 detailed timing: a nonzero pixel clock, the low bytes
    // of each active count, and their high nibbles in the shared bytes.
    response[timing] = 0x02;
    response[timing + 1] = 0x3a;
    response[timing + 2] = 0x80; // 1920 & 0xff
    response[timing + 4] = 0x70; // (1920 >> 8) << 4
    response[timing + 5] = 0x38; // 1080 & 0xff
    response[timing + 7] = 0x40; // (1080 >> 8) << 4

    assert_eq!(
        decode_edid_preferred_mode(&response),
        Some(DisplayMode::new(1920, 1080))
    );
}

/// A block whose first descriptor is a monitor name rather than a
/// timing states no preference, and a reply that is not an EDID block at
/// all states none either.
#[test]
fn an_edid_block_without_a_detailed_timing_states_no_preference() {
    let mut response = alloc::vec![0_u8; CTRL_HEADER_BYTES + 8 + 128];
    response[CTRL_HEADER_BYTES..CTRL_HEADER_BYTES + 4].copy_from_slice(&128_u32.to_le_bytes());
    let block = CTRL_HEADER_BYTES + 8;
    response[block..block + 8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
    assert_eq!(decode_edid_preferred_mode(&response), None);

    let empty = alloc::vec![0_u8; CTRL_HEADER_BYTES + 8];
    assert_eq!(decode_edid_preferred_mode(&empty), None);
}

/// Two tasks may have control commands in flight at once, and each
/// collects its own answer whichever order the device replies in.
#[test]
fn concurrent_commands_are_routed_by_descriptor_not_by_arrival_order() {
    let device = device();
    let mut first = pin!(DisplayDevice::scanouts(&device));
    let mut second = pin!(DisplayDevice::scanouts(&device));

    let first_token = pending_control(&device, first.as_mut());
    let second_token = pending_control(&device, second.as_mut());

    // The device answers the second request first.
    answer_control(
        &device,
        second_token,
        &display_info_response(&[(Rect::new(0, 0, 640, 480), true)]),
    );
    assert!(
        block_on(poll_once(first.as_mut())).is_none(),
        "the first request is still outstanding"
    );
    let list = block_on(poll_once(second.as_mut()))
        .expect("the second reply is in")
        .expect("display info");
    assert_eq!(list[0].geometry, Rect::new(0, 0, 640, 480));

    answer_control(
        &device,
        first_token,
        &display_info_response(&[(Rect::new(0, 0, 800, 600), true)]),
    );
    let list = block_on(poll_once(first.as_mut()))
        .expect("the first reply is in")
        .expect("display info");
    assert_eq!(list[0].geometry, Rect::new(0, 0, 800, 600));
}

#[test]
fn the_bring_up_round_trip_clears_the_interrupt_it_raised() {
    let device = device();
    let request = [0_u8; CTRL_HEADER_BYTES];
    let mut response = [0_u8; CTRL_HEADER_BYTES];

    let token = {
        let mut queue = device
            .control
            .try_lock()
            .expect("nothing else holds the control queue at bring-up");
        let token = queue
            .submit(
                &device.transport,
                &[request.as_slice()],
                &mut [response.as_mut_slice()],
            )
            .expect("the bring-up chain fits in an empty ring");
        queue.notify(&device.transport);
        // The device answers and raises its line, which is the whole of
        // what a real one does for a used buffer.
        queue.device_complete(token, CTRL_HEADER_BYTES as u32);
        device.transport.raise_interrupt(1);
        token
    };

    let mut queue = device
        .control
        .try_lock()
        .expect("nothing else holds the control queue at bring-up");
    let written = queue
        .reap_blocking(&device.transport, token)
        .expect("the bring-up request completes");
    drop(queue);

    assert_eq!(written, CTRL_HEADER_BYTES as u32);
    assert_eq!(
        device.transport.acknowledged_interrupts(),
        1,
        "a used buffer nobody acknowledges leaves an edge-triggered line \
         asserted, and a line that never falls cannot rise for the next \
         completion"
    );
}

// The rendering half. The device side is played exactly as it is for
// the 2D commands: a request is read out of the chain the driver
// published, a canned reply is written into its writable buffer, and
// the completion is raised.

/// Where a rendering device publishes its host-visible aperture, and
/// how long it is. The base is arbitrary; what matters is that a mapped
/// blob's region is reported relative to it.
const APERTURE_BASE: u64 = 0x8000_0000;
const APERTURE_BYTES: u64 = 16 << 20;

/// A device that carries a renderer with `capsets` capability sets and
/// a host-visible aperture.
fn render_device(capsets: u32) -> VirtioGpuDevice<FakeTransport> {
    VirtioGpuDevice::new(render_transport(capsets, 0))
        .expect("the rendering device should initialize")
}

/// `blob_alignment` in `struct virtio_gpu_config`, as a device publishes
/// it (Linux `include/uapi/linux/virtio_gpu.h`).
const CONFIG_BLOB_ALIGNMENT: usize = 16;

/// A rendering device that offers `VIRTIO_GPU_F_BLOB_ALIGNMENT` and
/// publishes `alignment`.
fn aligned_transport(alignment: u32) -> FakeTransport {
    let transport = render_transport(1, GPU_FEATURE_BLOB_ALIGNMENT);
    transport.set_config_u32(CONFIG_BLOB_ALIGNMENT, alignment);
    transport
}

fn aligned_render_device(alignment: u32) -> VirtioGpuDevice<FakeTransport> {
    VirtioGpuDevice::new(aligned_transport(alignment))
        .expect("the rendering device should initialize")
}

/// The transport of a rendering device, offering `extra` features on
/// top of the whole 3D set.
fn render_transport(capsets: u32, extra: u64) -> FakeTransport {
    let transport = FakeTransport::new(FakeTransportConfig {
        device_type: DeviceType::Gpu,
        offered_features: VirtioFeatures::VERSION_1.bits()
            | OFFERED_EDID
            | GPU_FEATURE_VIRGL
            | GPU_FEATURE_RESOURCE_UUID
            | GPU_FEATURE_RESOURCE_BLOB
            | GPU_FEATURE_CONTEXT_INIT
            | extra,
        queue_size: 8,
        supports_queue_reset: false,
        absent_queues: &[],
    });
    transport.set_config_u32(CONFIG_NUM_SCANOUTS, 1);
    transport.set_config_u32(CONFIG_NUM_CAPSETS, capsets);
    transport.set_shared_memory_region(
        // `VIRTIO_GPU_SHM_ID_HOST_VISIBLE` is the spec's literal 1
        // (virtio 1.2 §5.7.4), spelled out rather than quoted from the
        // driver's constant: a driver that asked for the wrong id is
        // answered `None`, which is what makes the constant's value
        // observable to a test at all.
        1,
        PhysicalRange::new(APERTURE_BASE, APERTURE_BYTES),
    );
    transport
}

/// A `virtio_gpu_resp_capset_info` for one capability set.
fn capset_info_response(id: CapsetId, max_version: u32, max_size: u32) -> Vec<u8> {
    let mut bytes = alloc::vec![0_u8; RESP_CAPSET_INFO_BYTES];
    bytes[0..4].copy_from_slice(&RESP_OK_CAPSET_INFO.to_le_bytes());
    bytes[CTRL_HEADER_BYTES..CTRL_HEADER_BYTES + 4].copy_from_slice(&id.raw().to_le_bytes());
    bytes[CTRL_HEADER_BYTES + 4..CTRL_HEADER_BYTES + 8].copy_from_slice(&max_version.to_le_bytes());
    bytes[CTRL_HEADER_BYTES + 8..CTRL_HEADER_BYTES + 12].copy_from_slice(&max_size.to_le_bytes());
    bytes
}

/// A `virtio_gpu_resp_map_info` carrying the caching the host requires.
fn map_info_response(map_info: u32) -> Vec<u8> {
    let mut bytes = alloc::vec![0_u8; RESP_MAP_INFO_BYTES];
    bytes[0..4].copy_from_slice(&RESP_OK_MAP_INFO.to_le_bytes());
    bytes[CTRL_HEADER_BYTES..CTRL_HEADER_BYTES + 4].copy_from_slice(&map_info.to_le_bytes());
    bytes
}

/// The physical run a slice occupies, which is how a guest's own pinned
/// command buffer reaches the driver.
///
/// The fake transport's pool is the identity, so a test's own bytes are
/// their own device address and the descriptor the driver publishes
/// reads them back exactly as a real device would read a guest's pages.
fn command_buffer(bytes: &[u8]) -> PhysicalRange {
    PhysicalRange::new(bytes.as_ptr() as u64, bytes.len() as u64)
}

fn long_word_at(request: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        request[offset..offset + 8]
            .try_into()
            .expect("an eight-byte field"),
    )
}

/// Drives one control command to completion: polls the future until it
/// parks, hands back the request bytes the driver published, answers
/// with `response`, and resolves the future.
fn exchange<Output>(
    device: &VirtioGpuDevice<FakeTransport>,
    mut future: Pin<&mut impl Future<Output = Output>>,
    response: &[u8],
) -> (Vec<u8>, Output) {
    let token = pending_control(device, future.as_mut());
    let request = control_request(device, token);
    answer_control(device, token, response);
    let outcome = block_on(future);
    (request, outcome)
}

/// A context the device accepted, for the tests that need one before
/// they can test anything else.
fn open_context(device: &VirtioGpuDevice<FakeTransport>) -> ContextId {
    open_context_with(device, RingCount::ONE)
}

/// A context of `rings` rings.
fn open_context_with(device: &VirtioGpuDevice<FakeTransport>, rings: RingCount) -> ContextId {
    let name = ContextName::from("venus").expect("a five-character name fits");
    let future = pin!(device.create_context(CapsetId::VENUS, rings, name));
    let (_, context) = exchange(device, future, &header_response(RESP_OK_NODATA));
    context.expect("the device accepted the context")
}

/// A blob the device accepted, mappable and host-allocated.
fn open_blob(device: &VirtioGpuDevice<FakeTransport>, context: ContextId, size: u64) -> BlobId {
    let request = BlobRequest {
        context,
        memory: BlobMemory::Host3d,
        usage: BlobUsage::MAPPABLE,
        size,
        blob_id: 0x1234,
        backing: &[],
    };
    let future = pin!(device.create_blob(request));
    let (_, blob) = exchange(device, future, &header_response(RESP_OK_NODATA));
    blob.expect("the device accepted the blob")
}

fn ring(index: u8) -> RingIndex {
    RingIndex::new(index).expect("a ring index below the table's bound")
}

fn rings(count: u8) -> RingCount {
    RingCount::new(count).expect("a ring count of one to sixty-four")
}

/// A submission of the guest's own pages.
fn pages_submission(
    context: ContextId,
    commands: &[u8],
    ring: RingIndex,
    fence: Option<u64>,
) -> Submission<'static> {
    Submission {
        context,
        commands: CommandStream::Pages(command_buffer(commands)),
        ring,
        fence: fence.map(FenceId::new),
    }
}

/// Runs one submission to completion against `response`, and reports
/// the wire bytes, the outcome, and whether the driver said the command
/// was posted.
fn run_submit(
    device: &VirtioGpuDevice<FakeTransport>,
    submission: Submission<'_>,
    response: &[u8],
) -> (Vec<u8>, Gpu3dResult<()>, bool) {
    let posted = AtomicBool::new(false);
    let mut mark = || posted.store(true, Ordering::Relaxed);
    let (wire, outcome) = {
        let future = pin!(device.submit(submission, &mut mark));
        exchange(device, future, response)
    };
    (wire, outcome, posted.load(Ordering::Relaxed))
}

/// Submits `commands` fenced at `fence` on `ring` and expects the device
/// to take it.
fn fenced(
    device: &VirtioGpuDevice<FakeTransport>,
    context: ContextId,
    ring: RingIndex,
    fence: u64,
) {
    let commands = [1_u8, 2, 3, 4];
    let (_, outcome, _) = run_submit(
        device,
        pages_submission(context, &commands, ring, Some(fence)),
        &header_response(RESP_OK_NODATA),
    );
    outcome.expect("the device took the command buffer");
}

/// Every capability set is read with its own `GET_CAPSET_INFO`, indexed
/// rather than named: the driver does not know what it will find.
#[test]
fn capability_sets_are_read_one_index_at_a_time() {
    let device = render_device(2);
    let mut future = pin!(Gpu3d::capsets(&device));

    let first = pending_control(&device, future.as_mut());
    let request = control_request(&device, first);
    assert_eq!(command_of(&request), CMD_GET_CAPSET_INFO);
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES), 0);
    answer_control(
        &device,
        first,
        &capset_info_response(CapsetId::VIRGL2, 2, 512),
    );

    let second = pending_control(&device, future.as_mut());
    let request = control_request(&device, second);
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES), 1);
    answer_control(
        &device,
        second,
        &capset_info_response(CapsetId::VENUS, 1, 4096),
    );

    let capsets = block_on(future).expect("the device described both capability sets");
    assert_eq!(capsets.len(), 2);
    assert_eq!(capsets[0].id, CapsetId::VIRGL2);
    assert_eq!(capsets[0].max_version, 2);
    assert_eq!(capsets[0].max_size, 512);
    assert_eq!(capsets[1].id, CapsetId::VENUS);
    assert_eq!(capsets[1].max_size, 4096);
}

/// A device with no renderer carries no capability set, whatever its
/// configuration space claims, and says so without touching the queue.
#[test]
fn a_device_with_no_renderer_carries_no_capability_set() {
    let device = device_with(1, OFFERED_EDID);
    device.transport.set_config_u32(CONFIG_NUM_CAPSETS, 4);

    let capsets = block_on(Gpu3d::capsets(&device)).expect("an empty list is not a refusal");

    assert!(capsets.is_empty());
    assert_eq!(
        device.transport.kick_count(),
        0,
        "an engine with no renderer answers without asking the device"
    );
}

/// The bytes of a capability set are the renderer's. The driver names
/// the set and the version, hands the reply to the caller's own buffer,
/// and decodes nothing.
#[test]
fn a_capability_set_reaches_the_caller_undecoded() {
    let device = render_device(1);
    let mut out = [0_u8; 8];
    let written = {
        let mut future = pin!(device.capset(CapsetId::VENUS, 1, &mut out));

        let token = pending_control(&device, future.as_mut());
        let request = control_request(&device, token);
        assert_eq!(command_of(&request), CMD_GET_CAPSET);
        assert_eq!(word_at(&request, CTRL_HEADER_BYTES), CapsetId::VENUS.raw());
        assert_eq!(word_at(&request, CTRL_HEADER_BYTES + 4), 1);

        let mut response = alloc::vec![0_u8; CTRL_HEADER_BYTES];
        response[0..4].copy_from_slice(&RESP_OK_CAPSET.to_le_bytes());
        response.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03, 0x04]);
        answer_control(&device, token, &response);

        block_on(future).expect("the device answered with the capability set")
    };
    assert_eq!(written, 8);
    assert_eq!(out, [0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03, 0x04]);
}

/// `CTX_CREATE` names the renderer in `context_init` and the context in
/// the header, and carries the debug name a host prints when the
/// context faults.
#[test]
fn a_context_names_its_renderer_and_carries_its_debug_name() {
    let device = render_device(1);
    let name = ContextName::from("venus").expect("a five-character name fits");
    let future = pin!(device.create_context(CapsetId::VENUS, rings(2), name));

    let (request, context) = exchange(&device, future, &header_response(RESP_OK_NODATA));
    let context = context.expect("the device accepted the context");

    assert_eq!(command_of(&request), CMD_CTX_CREATE);
    assert_eq!(
        word_at(&request, 16),
        context.raw(),
        "the header names the context every later command will"
    );
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES), 5, "the name's length");
    assert_eq!(
        word_at(&request, CTRL_HEADER_BYTES + 4),
        CapsetId::VENUS.raw(),
        "context_init selects the renderer"
    );
    assert_eq!(
        &request[CTRL_HEADER_BYTES + 8..CTRL_HEADER_BYTES + 13],
        b"venus"
    );
}

/// Without `VIRTIO_GPU_F_CONTEXT_INIT` the field is reserved and every
/// context is virgl's, so the driver leaves it zero rather than writing
/// a number into a field the device does not read.
#[test]
fn a_context_leaves_context_init_alone_when_the_device_did_not_offer_it() {
    let device = device_with(1, OFFERED_EDID | GPU_FEATURE_VIRGL);
    let name = ContextName::from("virgl").expect("a five-character name fits");
    let future = pin!(device.create_context(CapsetId::VIRGL2, RingCount::ONE, name));

    let (request, context) = exchange(&device, future, &header_response(RESP_OK_NODATA));

    context.expect("the device accepted the context");
    assert_eq!(word_at(&request, CTRL_HEADER_BYTES + 4), 0);
}

/// Without context-init the device reads no ring index, so a context of
/// more than the CPU ring would have every fence land on one timeline;
/// it is refused before the device is asked.
#[test]
fn a_context_of_several_rings_needs_context_init() {
    let device = device_with(1, OFFERED_EDID | GPU_FEATURE_VIRGL);
    let name = ContextName::from("virgl").expect("a five-character name fits");

    let refused = block_on(device.create_context(CapsetId::VIRGL2, rings(2), name));

    assert_eq!(refused.err(), Some(Gpu3dError::InvalidParameter));
    assert_eq!(device.transport.kick_count(), 0);
}

/// The context table is the device's whole bound across every instance,
/// and filling it says which table is full.
#[test]
fn the_context_table_says_when_it_is_full() {
    let device = render_device(1);
    for _ in 0..MAX_CONTEXTS {
        open_context(&device);
    }
    let name = ContextName::from("venus").expect("a five-character name fits");

    let refused = block_on(device.create_context(CapsetId::VENUS, RingCount::ONE, name));

    assert_eq!(
        refused.err(),
        Some(Gpu3dError::TooMany {
            table: Gpu3dTable::Contexts,
            limit: MAX_CONTEXTS,
        })
    );
}

/// Every 3D call on a device with no renderer is refused, and none of
/// them reaches the queue.
#[test]
fn a_device_with_no_renderer_refuses_every_rendering_call() {
    let device = device_with(1, OFFERED_EDID);
    let name = ContextName::from("venus").expect("a five-character name fits");

    let created = block_on(device.create_context(CapsetId::VENUS, RingCount::ONE, name));
    assert_eq!(created.err(), Some(Gpu3dError::Unsupported));
    let commands = [0_u8; 4];
    let submitted = block_on(device.submit(
        pages_submission(ContextId::new(1), &commands, RingIndex::CPU, Some(1)),
        &mut || {},
    ));
    assert_eq!(submitted.err(), Some(Gpu3dError::Unsupported));
    let mapped = block_on(device.map_blob(BlobId::new(1)));
    assert_eq!(mapped.err(), Some(Gpu3dError::Unsupported));
    assert_eq!(device.transport.kick_count(), 0);
}

/// A host-3D blob's storage is the renderer's, so the request carries
/// no memory-entry table and the host's own handle for the allocation.
#[test]
fn a_host_three_d_blob_carries_the_renderers_handle_and_no_pages() {
    let device = render_device(1);
    let context = open_context(&device);
    let request = BlobRequest {
        context,
        memory: BlobMemory::Host3d,
        usage: BlobUsage::MAPPABLE | BlobUsage::SHAREABLE,
        size: 0x2000,
        blob_id: 0xfeed_face,
        backing: &[],
    };
    let future = pin!(device.create_blob(request));

    let (wire, blob) = exchange(&device, future, &header_response(RESP_OK_NODATA));
    let blob = blob.expect("the device accepted the blob");

    assert_eq!(command_of(&wire), CMD_RESOURCE_CREATE_BLOB);
    assert_eq!(word_at(&wire, 16), context.raw(), "the owning context");
    assert_eq!(word_at(&wire, CTRL_HEADER_BYTES), blob.raw());
    assert_eq!(word_at(&wire, CTRL_HEADER_BYTES + 4), BLOB_MEM_HOST3D);
    assert_eq!(
        word_at(&wire, CTRL_HEADER_BYTES + 8),
        (BlobUsage::MAPPABLE | BlobUsage::SHAREABLE).bits()
    );
    assert_eq!(word_at(&wire, CTRL_HEADER_BYTES + 12), 0, "no entries");
    assert_eq!(long_word_at(&wire, CTRL_HEADER_BYTES + 16), 0xfeed_face);
    assert_eq!(long_word_at(&wire, CTRL_HEADER_BYTES + 24), 0x2000);
    assert_eq!(
        wire.len(),
        CTRL_HEADER_BYTES + 32,
        "a host-3D blob publishes no memory entries"
    );
}

/// A guest blob's pages *are* the resource, so they follow the request
/// as a memory-entry table.
#[test]
fn a_guest_blob_publishes_the_pages_it_is_backed_by() {
    let device = render_device(1);
    let context = open_context(&device);
    let pages = backing(2);
    let request = BlobRequest {
        context,
        memory: BlobMemory::Guest,
        usage: BlobUsage::MAPPABLE,
        size: 0x2000,
        blob_id: 0,
        backing: &pages,
    };
    let future = pin!(device.create_blob(request));

    let (wire, blob) = exchange(&device, future, &header_response(RESP_OK_NODATA));

    blob.expect("the device accepted the blob");
    assert_eq!(word_at(&wire, CTRL_HEADER_BYTES + 4), BLOB_MEM_GUEST);
    assert_eq!(word_at(&wire, CTRL_HEADER_BYTES + 12), 1, "one range");
    assert_eq!(
        long_word_at(&wire, CTRL_HEADER_BYTES + 32),
        pages[0].start.phys_addr() as u64,
        "the memory entry names the caller's pages"
    );
}

/// A blob whose parameters contradict its kind never reaches the wire.
#[test]
fn a_blob_that_names_the_wrong_memory_is_refused_before_the_device_sees_it() {
    let device = render_device(1);
    let context = open_context(&device);
    let kicks = device.transport.kick_count();
    let pages = backing(1);
    let request = BlobRequest {
        context,
        memory: BlobMemory::Host3d,
        usage: BlobUsage::MAPPABLE,
        size: 0x1000,
        blob_id: 1,
        backing: &pages,
    };

    let refused = block_on(device.create_blob(request));

    assert_eq!(refused.err(), Some(Gpu3dError::InvalidBlob));
    assert_eq!(device.transport.kick_count(), kicks);
}

/// A pure guest blob has no host object to name, so a non-zero blob id
/// on one is refused before the wire, as Linux's `verify_blob` refuses
/// it.
#[test]
fn a_guest_blob_that_names_a_host_object_is_refused() {
    let device = render_device(1);
    let context = open_context(&device);
    let kicks = device.transport.kick_count();
    let pages = backing(1);
    let request = BlobRequest {
        context,
        memory: BlobMemory::Guest,
        usage: BlobUsage::MAPPABLE,
        size: 0x1000,
        blob_id: 7,
        backing: &pages,
    };

    let refused = block_on(device.create_blob(request));

    assert_eq!(refused.err(), Some(Gpu3dError::InvalidBlob));
    assert_eq!(device.transport.kick_count(), kicks);
}

/// A device that states no blob alignment imposes none: the size goes
/// on the wire as the caller gave it.
#[test]
fn a_blob_size_goes_unrounded_to_a_device_that_states_no_alignment() {
    let device = render_device(1);
    let context = open_context(&device);
    let request = BlobRequest {
        context,
        memory: BlobMemory::Host3d,
        usage: BlobUsage::MAPPABLE,
        size: 0x1801,
        blob_id: 9,
        backing: &[],
    };
    let future = pin!(device.create_blob(request));

    let (wire, blob) = exchange(&device, future, &header_response(RESP_OK_NODATA));

    blob.expect("the device accepted the blob");
    assert_eq!(long_word_at(&wire, CTRL_HEADER_BYTES + 24), 0x1801);
}

/// A device that offers `VIRTIO_GPU_F_BLOB_ALIGNMENT` is asked for its
/// alignment, and every blob size is rounded up to it.
#[test]
fn a_blob_size_is_rounded_to_the_alignment_the_device_publishes() {
    let device = aligned_render_device(0x1_0000);
    assert!(device.features().device(GPU_FEATURE_BLOB_ALIGNMENT));
    let context = open_context(&device);
    let request = BlobRequest {
        context,
        memory: BlobMemory::Host3d,
        usage: BlobUsage::MAPPABLE,
        size: 0x1801,
        blob_id: 9,
        backing: &[],
    };
    let future = pin!(device.create_blob(request));

    let (wire, blob) = exchange(&device, future, &header_response(RESP_OK_NODATA));

    blob.expect("the device accepted the blob");
    assert_eq!(long_word_at(&wire, CTRL_HEADER_BYTES + 24), 0x1_0000);
}

/// A guest blob's rounded size may not reach past the pages it is
/// backed by: the device would read memory the caller did not give it.
#[test]
fn a_guest_blob_too_small_for_the_device_alignment_is_refused() {
    let device = aligned_render_device(0x1_0000);
    let context = open_context(&device);
    let kicks = device.transport.kick_count();
    let pages = backing(1);
    let request = BlobRequest {
        context,
        memory: BlobMemory::Guest,
        usage: BlobUsage::MAPPABLE,
        size: 0x1000,
        blob_id: 0,
        backing: &pages,
    };

    let refused = block_on(device.create_blob(request));

    assert_eq!(refused.err(), Some(Gpu3dError::InvalidBlob));
    assert_eq!(device.transport.kick_count(), kicks);
}

/// An alignment that is not a power of two describes no address
/// arithmetic, so the device is refused at bring-up.
#[test]
fn a_blob_alignment_that_is_not_a_power_of_two_is_refused_at_bring_up() {
    let transport = aligned_transport(0x3000);
    assert!(matches!(
        VirtioGpuDevice::new(transport),
        Err(IoError::InvalidDeviceConfig(_))
    ));
}

/// The aperture lives under `VIRTIO_GPU_SHM_ID_HOST_VISIBLE` — the
/// spec's id 1, not `VIRTIO_GPU_SHM_ID_UNDEFINED`'s 0 (virtio 1.2
/// §5.7.4). Both values are pinned against literals because quoting the
/// constants would be vouching for the driver's own numbers; the fake
/// publishes the region under the literal id, so a driver asking for
/// the wrong one would find nothing.
#[test]
fn the_host_visible_aperture_is_the_specification_s_region() {
    assert_eq!(SHM_ID_UNDEFINED, 0, "the spec's 'no such region'");
    assert_eq!(SHM_ID_HOST_VISIBLE, 1, "the spec's aperture id");

    let device = render_device(1);
    assert_eq!(
        device.host_visible_aperture(),
        Some(PhysicalRange::new(APERTURE_BASE, APERTURE_BYTES)),
        "the driver asked the transport for the host-visible id"
    );
    assert!(
        device
            .transport
            .shared_memory_region(SHM_ID_UNDEFINED)
            .is_none(),
        "nothing is published under the undefined id"
    );
}

/// Mapping places the blob at an offset in the engine's aperture and
/// reports the physical span it now answers on.
#[test]
fn mapping_a_blob_places_it_in_the_aperture_and_reports_where() {
    let device = render_device(1);
    let context = open_context(&device);
    let blob = open_blob(&device, context, 0x1000);
    let future = pin!(device.map_blob(blob));

    let (wire, region) = exchange(&device, future, &map_info_response(MAP_CACHE_CACHED));
    let region = region.expect("the device mapped the blob");

    assert_eq!(command_of(&wire), CMD_RESOURCE_MAP_BLOB);
    assert_eq!(word_at(&wire, CTRL_HEADER_BYTES), blob.raw());
    assert_eq!(long_word_at(&wire, CTRL_HEADER_BYTES + 8), 0, "first blob");
    assert_eq!(region.physical.start, APERTURE_BASE);
    assert_eq!(
        region.physical.bytes, APERTURE_ALIGN,
        "a blob takes a whole aligned span, because that is the unit an \
         address space maps at"
    );
    assert_eq!(
        region.attributes,
        helios_hal::device::DeviceRegionAttributes::PREFETCHABLE_MEMORY,
        "a mapped blob is a renderer's buffer, not a register file"
    );
}

/// Two mapped blobs never overlap: the second is placed past the first,
/// aligned to what an address space can map at.
#[test]
fn a_second_mapped_blob_is_placed_past_the_first() {
    let device = render_device(1);
    let context = open_context(&device);
    let first = open_blob(&device, context, 0x1000);
    let second = open_blob(&device, context, 0x1000);

    let future = pin!(device.map_blob(first));
    let (_, region) = exchange(&device, future, &map_info_response(MAP_CACHE_CACHED));
    let first_region = region.expect("the device mapped the first blob");

    let future = pin!(device.map_blob(second));
    let (wire, region) = exchange(&device, future, &map_info_response(MAP_CACHE_CACHED));
    let second_region = region.expect("the device mapped the second blob");

    assert_eq!(long_word_at(&wire, CTRL_HEADER_BYTES + 8), APERTURE_ALIGN);
    assert_eq!(
        second_region.physical.start,
        first_region.physical.start + first_region.physical.bytes
    );
}

/// Two `map_blob`s whose device round trips overlap still get two
/// different spans: the reservation is recorded before this call's
/// first await, so a call that starts while another is in flight
/// already sees its span as taken.
#[test]
fn two_maps_in_flight_at_once_get_different_spans() {
    let device = render_device(1);
    let context = open_context(&device);
    let first = open_blob(&device, context, 0x1000);
    let second = open_blob(&device, context, 0x1000);

    let mut first_map = pin!(device.map_blob(first));
    let mut second_map = pin!(device.map_blob(second));
    let first_token = pending_control(&device, first_map.as_mut());
    let second_token = pending_control(&device, second_map.as_mut());
    let first_wire = control_request(&device, first_token);
    let second_wire = control_request(&device, second_token);
    answer_control(&device, first_token, &map_info_response(MAP_CACHE_CACHED));
    answer_control(&device, second_token, &map_info_response(MAP_CACHE_CACHED));
    let first_region = block_on(first_map).expect("the device mapped the first");
    let second_region = block_on(second_map).expect("the device mapped the second");

    assert_eq!(long_word_at(&first_wire, CTRL_HEADER_BYTES + 8), 0);
    assert_eq!(
        long_word_at(&second_wire, CTRL_HEADER_BYTES + 8),
        APERTURE_ALIGN,
        "the second call saw the first's span as taken while it was in flight"
    );
    assert_eq!(first_region.physical.start, APERTURE_BASE);
    assert_eq!(second_region.physical.start, APERTURE_BASE + APERTURE_ALIGN);
}

/// The host's `map_info` answer is carried to the mapping: a blob it
/// wants write-combined is reported as write-combining memory, and one
/// it wants uncached as memory every access must reach uncached.
#[test]
fn a_mapping_carries_the_caching_the_host_requires() {
    for (map_info, expected) in [
        (MAP_CACHE_WC, DeviceRegionAttributes::WRITE_COMBINING_MEMORY),
        (MAP_CACHE_UNCACHED, DeviceRegionAttributes::UNCACHED_MEMORY),
        (
            MAP_CACHE_CACHED,
            DeviceRegionAttributes::PREFETCHABLE_MEMORY,
        ),
    ] {
        let device = render_device(1);
        let context = open_context(&device);
        let blob = open_blob(&device, context, 0x1000);
        let future = pin!(device.map_blob(blob));

        let (_, region) = exchange(&device, future, &map_info_response(map_info));

        assert_eq!(
            region.expect("the device mapped the blob").attributes,
            expected,
            "map_info {map_info:#x}"
        );
    }
}

/// A caching mode the specification gives no meaning to is a rule
/// nothing here can keep, so the mapping is taken back rather than
/// handed over under a guess.
#[test]
fn a_mapping_under_an_unknown_caching_mode_is_given_back() {
    const UNDEFINED_CACHING: u32 = 0x0007;

    let device = render_device(1);
    let context = open_context(&device);
    let blob = open_blob(&device, context, 0x1000);
    let mut future = pin!(device.map_blob(blob));

    let map = pending_control(&device, future.as_mut());
    answer_control(&device, map, &map_info_response(UNDEFINED_CACHING));
    let unmap = pending_control(&device, future.as_mut());
    let wire = control_request(&device, unmap);
    answer_control(&device, unmap, &header_response(RESP_OK_NODATA));
    let refused = block_on(future);

    assert_eq!(command_of(&wire), CMD_RESOURCE_UNMAP_BLOB);
    assert_eq!(
        refused.err(),
        Some(Gpu3dError::UnknownCaching {
            map_info: UNDEFINED_CACHING
        })
    );
}

/// A resource a guest still has a path to is never taken back: the
/// aperture would then decode to whatever the renderer put there next.
#[test]
fn a_mapped_blob_cannot_be_destroyed_until_it_is_unmapped() {
    let device = render_device(1);
    let context = open_context(&device);
    let blob = open_blob(&device, context, 0x1000);
    let future = pin!(device.map_blob(blob));
    let (_, mapped) = exchange(&device, future, &map_info_response(MAP_CACHE_CACHED));
    mapped.expect("the device mapped the blob");

    let refused = block_on(device.destroy_blob(blob));
    assert_eq!(refused.err(), Some(Gpu3dError::NotMappable(blob)));

    let future = pin!(device.unmap_blob(blob));
    let (wire, unmapped) = exchange(&device, future, &header_response(RESP_OK_NODATA));
    unmapped.expect("the device took the blob back out of the aperture");
    assert_eq!(command_of(&wire), CMD_RESOURCE_UNMAP_BLOB);

    let future = pin!(device.destroy_blob(blob));
    let (wire, destroyed) = exchange(&device, future, &header_response(RESP_OK_NODATA));
    destroyed.expect("an unmapped blob goes back");
    assert_eq!(command_of(&wire), CMD_RESOURCE_UNREF);
}

/// Attaching and detaching name the context in the header and the
/// resource in the body, which is the whole of `virtio_gpu_ctx_resource`.
#[test]
fn attaching_a_resource_names_the_context_and_the_resource() {
    let device = render_device(1);
    let context = open_context(&device);
    let blob = open_blob(&device, context, 0x1000);

    let future = pin!(device.attach_resource(context, blob));
    let (wire, attached) = exchange(&device, future, &header_response(RESP_OK_NODATA));
    attached.expect("the device attached the resource");
    assert_eq!(command_of(&wire), CMD_CTX_ATTACH_RESOURCE);
    assert_eq!(word_at(&wire, 16), context.raw());
    assert_eq!(word_at(&wire, CTRL_HEADER_BYTES), blob.raw());

    let future = pin!(device.detach_resource(context, blob));
    let (wire, detached) = exchange(&device, future, &header_response(RESP_OK_NODATA));
    detached.expect("the device detached the resource");
    assert_eq!(command_of(&wire), CMD_CTX_DETACH_RESOURCE);
}

/// A fenced submission asks for a context fence on its ring: the fence
/// flag and the ring flag, the fence the caller chose, its context, the
/// ring index, and the command buffer as a descriptor of its own — the
/// driver never copies the guest's bytes.
#[test]
fn a_fenced_submission_names_its_ring_on_the_wire() {
    let device = render_device(1);
    let context = open_context_with(&device, rings(4));
    let commands = [0x11_u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];

    let (wire, submitted, posted) = run_submit(
        &device,
        pages_submission(context, &commands, ring(3), Some(7)),
        &header_response(RESP_OK_NODATA),
    );
    submitted.expect("the device took the command buffer");

    assert!(posted, "the driver said the command reached the queue");
    assert_eq!(command_of(&wire), CMD_SUBMIT_3D);
    assert_eq!(word_at(&wire, 4), CTRL_FLAG_FENCE | CTRL_FLAG_INFO_RING_IDX);
    assert_eq!(long_word_at(&wire, 8), 7, "the fence the caller chose");
    assert_eq!(word_at(&wire, 16), context.raw());
    assert_eq!(wire[CTRL_RING_IDX_OFFSET], 3, "the ring the fence is on");
    assert_eq!(word_at(&wire, CTRL_HEADER_BYTES), commands.len() as u32);
    assert_eq!(
        &wire[CTRL_HEADER_BYTES + 8..],
        &commands,
        "the command buffer travels as it was written"
    );
}

/// An unfenced submission carries neither flag, no fence id and no ring:
/// the device answers it once it has processed it, and creates no fence.
#[test]
fn an_unfenced_submission_carries_no_fence_flag() {
    let device = render_device(1);
    let context = open_context_with(&device, rings(2));
    let commands = [1_u8, 2, 3, 4];

    let (wire, submitted, _) = run_submit(
        &device,
        pages_submission(context, &commands, ring(1), None),
        &header_response(RESP_OK_NODATA),
    );
    submitted.expect("the device took the command buffer");

    assert_eq!(word_at(&wire, 4), 0, "no flag at all");
    assert_eq!(long_word_at(&wire, 8), 0, "no fence id");
    assert_eq!(wire[CTRL_RING_IDX_OFFSET], 0, "no ring index");
}

/// A short command stream the caller holds goes on the wire from where
/// it is, after the request, exactly as a page run would.
#[test]
fn an_inline_command_stream_travels_after_the_request() {
    let device = render_device(1);
    let context = open_context(&device);
    let commands = [0xa1_u8, 0xa2, 0xa3, 0xa4];

    let (wire, submitted, _) = run_submit(
        &device,
        Submission {
            context,
            commands: CommandStream::Inline(&commands),
            ring: RingIndex::CPU,
            fence: Some(FenceId::new(1)),
        },
        &header_response(RESP_OK_NODATA),
    );
    submitted.expect("the device took the command stream");

    assert_eq!(word_at(&wire, CTRL_HEADER_BYTES), commands.len() as u32);
    assert_eq!(&wire[CTRL_HEADER_BYTES + 8..], &commands);
}

/// The driver says a command is posted the moment its chain is in the
/// ring — while the device still holds it — so the caller can let the
/// next command follow it without waiting for this one's answer.
#[test]
fn a_submission_is_posted_before_the_device_answers() {
    let device = render_device(1);
    let context = open_context(&device);
    let commands = [1_u8, 2, 3, 4];
    let posted = AtomicBool::new(false);
    let mut mark = || posted.store(true, Ordering::Relaxed);
    let mut future = pin!(device.submit(
        pages_submission(context, &commands, RingIndex::CPU, Some(1)),
        &mut mark,
    ));

    let token = pending_control(&device, future.as_mut());
    assert!(
        posted.load(Ordering::Relaxed),
        "posted while the device still holds the command"
    );
    answer_control(&device, token, &header_response(RESP_OK_NODATA));
    block_on(future).expect("the device took the command buffer");
}

/// A ring the context was not created with is refused before the wire,
/// and the caller's command is never posted.
#[test]
fn a_ring_past_the_context_s_count_is_refused() {
    let device = render_device(1);
    let context = open_context_with(&device, rings(2));
    let kicks = device.transport.kick_count();
    let commands = [1_u8, 2, 3, 4];
    let posted = AtomicBool::new(false);

    let refused = block_on(device.submit(
        pages_submission(context, &commands, ring(2), Some(1)),
        &mut || posted.store(true, Ordering::Relaxed),
    ));

    assert_eq!(
        refused.err(),
        Some(Gpu3dError::UnknownRing {
            context,
            ring: ring(2)
        })
    );
    assert!(!posted.load(Ordering::Relaxed));
    assert_eq!(device.transport.kick_count(), kicks);
    assert_eq!(
        block_on(poll_once(pin!(device.fences(
            context,
            ring(2),
            FenceId::START
        )))),
        Some(Err(Gpu3dError::UnknownRing {
            context,
            ring: ring(2)
        })),
        "the ring has no fence stream either"
    );
}

/// A command buffer of no bytes is not a submission, and neither is one
/// longer than a submission carries.
#[test]
fn an_empty_command_buffer_never_reaches_the_device() {
    let device = render_device(1);
    let context = open_context(&device);
    let kicks = device.transport.kick_count();

    let refused = block_on(device.submit(
        Submission {
            context,
            commands: CommandStream::Pages(PhysicalRange::new(0, 0)),
            ring: RingIndex::CPU,
            fence: Some(FenceId::new(1)),
        },
        &mut || {},
    ));

    assert_eq!(
        refused.err(),
        Some(Gpu3dError::CommandBufferLength { bytes: 0 })
    );
    assert_eq!(device.transport.kick_count(), kicks);
}

/// The fence stream is a cursor: a reader passes back what it last saw
/// and is handed the next point, in order, and never the same one
/// twice.
#[test]
fn the_fence_stream_is_read_in_order_from_where_the_reader_left_off() {
    let device = render_device(1);
    let context = open_context(&device);

    let mut waiting = pin!(device.fences(context, RingIndex::CPU, FenceId::START));
    assert!(
        block_on(poll_once(waiting.as_mut())).is_none(),
        "no fence has retired yet"
    );

    fenced(&device, context, RingIndex::CPU, 4);
    let fence = block_on(waiting).expect("the fence the submission carried");
    assert_eq!(fence, FenceId::new(4));

    let mut waiting = pin!(device.fences(context, RingIndex::CPU, fence));
    assert!(
        block_on(poll_once(waiting.as_mut())).is_none(),
        "a reader that has seen fence four is not handed it again"
    );
    fenced(&device, context, RingIndex::CPU, 9);
    assert_eq!(
        block_on(waiting).expect("the second fence"),
        FenceId::new(9)
    );
}

/// A reader that was away while several fences retired is handed the
/// newest of them rather than the next in sequence: the ring is
/// monotone, so the newest answers for every one before it.
#[test]
fn a_lagging_fence_reader_is_handed_the_newest_fence() {
    let device = render_device(1);
    let context = open_context(&device);

    fenced(&device, context, RingIndex::CPU, 4);
    fenced(&device, context, RingIndex::CPU, 9);

    let fence = block_on(device.fences(context, RingIndex::CPU, FenceId::START))
        .expect("two fences have retired since the reader last looked");
    assert_eq!(
        fence,
        FenceId::new(9),
        "the newest fence answers for every one before it"
    );
}

/// One context's fences are not another's: a reader following one
/// context's ring is not woken through by work on another context's
/// ring of the same index.
#[test]
fn a_fence_belongs_to_the_context_that_submitted_it() {
    let device = render_device(1);
    let first = open_context(&device);
    let second = open_context(&device);

    let mut waiting = pin!(device.fences(second, RingIndex::CPU, FenceId::START));
    assert!(block_on(poll_once(waiting.as_mut())).is_none());

    fenced(&device, first, RingIndex::CPU, 3);
    assert!(
        block_on(poll_once(waiting.as_mut())).is_none(),
        "the other context's ring has not moved"
    );
    fenced(&device, second, RingIndex::CPU, 1);
    assert_eq!(
        block_on(waiting).expect("this context's fence"),
        FenceId::new(1)
    );
}

/// One ring's fences are not another's on the same context: each ring
/// is a timeline of its own, and retiring a fence on one leaves every
/// other where it was.
#[test]
fn a_fence_belongs_to_the_ring_it_was_submitted_on() {
    let device = render_device(1);
    let context = open_context_with(&device, rings(3));

    let mut cpu = pin!(device.fences(context, RingIndex::CPU, FenceId::START));
    let mut second = pin!(device.fences(context, ring(2), FenceId::START));
    assert!(block_on(poll_once(cpu.as_mut())).is_none());
    assert!(block_on(poll_once(second.as_mut())).is_none());

    fenced(&device, context, ring(1), 40);
    assert!(
        block_on(poll_once(cpu.as_mut())).is_none(),
        "ring zero has not moved"
    );
    assert!(
        block_on(poll_once(second.as_mut())).is_none(),
        "ring two has not moved"
    );
    assert_eq!(
        block_on(device.fences(context, ring(1), FenceId::START)),
        Ok(FenceId::new(40))
    );

    fenced(&device, context, ring(2), 2);
    assert_eq!(block_on(second), Ok(FenceId::new(2)));
    assert!(
        block_on(poll_once(cpu.as_mut())).is_none(),
        "ring zero still has not moved"
    );
}

/// A command the device refuses still retires its fence: a reader of
/// the ring must not be left waiting for a point the device has already
/// decided will not come.
#[test]
fn a_rejected_submission_still_retires_its_fence() {
    let device = render_device(1);
    let context = open_context(&device);

    let mut waiting = pin!(device.fences(context, RingIndex::CPU, FenceId::START));
    assert!(block_on(poll_once(waiting.as_mut())).is_none());

    let commands = [1_u8, 2, 3, 4];
    let (_, submitted, posted) = run_submit(
        &device,
        pages_submission(context, &commands, RingIndex::CPU, Some(2)),
        &header_response(super::RESP_ERR_INVALID_PARAMETER),
    );

    assert!(
        posted,
        "the command reached the queue before it was refused"
    );
    assert_eq!(submitted.err(), Some(Gpu3dError::InvalidParameter));
    assert_eq!(
        block_on(waiting).expect("the fence of a refused command"),
        FenceId::new(2)
    );
}

/// An `ERR_INVALID_CONTEXT_ID` answer names the context the request
/// carried, rather than being reported as an unspecified fault.
#[test]
fn an_invalid_context_answer_names_the_context_that_was_asked_for() {
    let device = render_device(1);
    let context = open_context(&device);
    let commands = [1_u8, 2, 3, 4];

    let (_, submitted, _) = run_submit(
        &device,
        pages_submission(context, &commands, RingIndex::CPU, Some(1)),
        &header_response(RESP_ERR_INVALID_CONTEXT_ID),
    );

    assert_eq!(submitted.err(), Some(Gpu3dError::UnknownContext(context)));
}

/// A context the device let go names nothing afterwards.
#[test]
fn a_destroyed_context_is_not_this_devices_any_more() {
    let device = render_device(1);
    let context = open_context(&device);

    let future = pin!(device.destroy_context(context));
    let (wire, destroyed) = exchange(&device, future, &header_response(RESP_OK_NODATA));
    destroyed.expect("the device released the context");
    assert_eq!(command_of(&wire), CMD_CTX_DESTROY);
    assert_eq!(word_at(&wire, 16), context.raw());

    let commands = [1_u8, 2, 3, 4];
    let refused = block_on(device.submit(
        pages_submission(context, &commands, RingIndex::CPU, Some(1)),
        &mut || {},
    ));
    assert_eq!(refused.err(), Some(Gpu3dError::UnknownContext(context)));
}

/// `SET_SCANOUT_BLOB` names the scanout, the blob, the frame's geometry
/// and format, and one plane's stride and offset, with the whole frame
/// as the source rectangle.
#[test]
fn a_blob_scanout_is_encoded_as_the_specification_lays_it_out() {
    let device = render_device(1);
    let context = open_context(&device);
    let blob = open_blob(&device, context, 0x20_0000);
    let layout = BlobScanout {
        blob,
        mode: DisplayMode::new(640, 480),
        format: PixelFormat::Bgrx8888,
        offset: 0x100,
        stride: 2560,
    };
    let future = pin!(device.set_scanout_blob(ScanoutId::new(0), layout));

    let (wire, shown) = exchange(&device, future, &header_response(RESP_OK_NODATA));
    shown.expect("the device showed the blob");

    assert_eq!(wire.len(), SET_SCANOUT_BLOB_BYTES);
    assert_eq!(command_of(&wire), CMD_SET_SCANOUT_BLOB);
    let rect = CTRL_HEADER_BYTES;
    assert_eq!(
        [
            word_at(&wire, rect),
            word_at(&wire, rect + 4),
            word_at(&wire, rect + 8),
            word_at(&wire, rect + 12)
        ],
        [0, 0, 640, 480],
        "the source rectangle is the whole frame"
    );
    let body = CTRL_HEADER_BYTES + 16;
    assert_eq!(word_at(&wire, body), 0, "scanout id");
    assert_eq!(word_at(&wire, body + 4), blob.raw(), "resource id");
    assert_eq!(word_at(&wire, body + 8), 640, "width");
    assert_eq!(word_at(&wire, body + 12), 480, "height");
    assert_eq!(
        word_at(&wire, body + 16),
        2,
        "VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM"
    );
    assert_eq!(word_at(&wire, body + 24), 2560, "strides[0]");
    assert_eq!(word_at(&wire, body + 28), 0, "strides[1]");
    assert_eq!(word_at(&wire, body + 40), 0x100, "offsets[0]");
    assert_eq!(word_at(&wire, body + 44), 0, "offsets[1]");
}

/// A frame that ends past the blob is refused with the numbers that say
/// why, before the device sees it.
#[test]
fn a_blob_scanout_that_does_not_fit_never_reaches_the_device() {
    let device = render_device(1);
    let context = open_context(&device);
    let blob = open_blob(&device, context, 0x1000);
    let kicks = device.transport.kick_count();
    let layout = BlobScanout {
        blob,
        mode: DisplayMode::new(64, 64),
        format: PixelFormat::Bgrx8888,
        offset: 0,
        stride: 256,
    };

    let refused = block_on(device.set_scanout_blob(ScanoutId::new(0), layout));

    assert_eq!(
        refused.err(),
        Some(DisplayError::BlobLayout {
            width: 64,
            height: 64,
            offset: 0,
            stride: 256,
            blob_bytes: 0x1000,
        })
    );
    assert_eq!(device.transport.kick_count(), kicks);
}

/// A device that negotiated no blob resources scans no blob out.
#[test]
fn a_device_without_blob_resources_scans_no_blob_out() {
    let device = device_with(1, OFFERED_EDID | GPU_FEATURE_VIRGL);
    let layout = BlobScanout {
        blob: BlobId::new(1),
        mode: DisplayMode::new(64, 64),
        format: PixelFormat::Bgrx8888,
        offset: 0,
        stride: 256,
    };

    let refused = block_on(device.set_scanout_blob(ScanoutId::new(0), layout));

    assert_eq!(refused.err(), Some(DisplayError::BlobScanoutUnsupported));
    assert_eq!(device.transport.kick_count(), 0);
}

/// Presenting a blob is a `RESOURCE_FLUSH` naming it, with no transfer
/// first: the bytes are already the device's.
#[test]
fn flushing_a_blob_is_one_resource_flush() {
    let device = render_device(1);
    let context = open_context(&device);
    let blob = open_blob(&device, context, 0x10_0000);
    let future = pin!(device.flush_blob(blob, Rect::new(0, 0, 640, 480)));

    let (wire, flushed) = exchange(&device, future, &header_response(RESP_OK_NODATA));
    flushed.expect("the device flushed the blob");

    assert_eq!(command_of(&wire), CMD_RESOURCE_FLUSH);
    assert_eq!(word_at(&wire, CTRL_HEADER_BYTES + 8), 640);
    assert_eq!(word_at(&wire, CTRL_HEADER_BYTES + 16), blob.raw());
}

/// A device that publishes no host-visible aperture maps nothing, and
/// says so rather than being handed an address it does not answer on.
#[test]
fn a_device_with_no_aperture_maps_nothing() {
    let transport = FakeTransport::new(FakeTransportConfig {
        device_type: DeviceType::Gpu,
        offered_features: VirtioFeatures::VERSION_1.bits()
            | GPU_FEATURE_VIRGL
            | GPU_FEATURE_RESOURCE_BLOB
            | GPU_FEATURE_CONTEXT_INIT,
        queue_size: 8,
        supports_queue_reset: false,
        absent_queues: &[],
    });
    transport.set_config_u32(CONFIG_NUM_SCANOUTS, 1);
    transport.set_config_u32(CONFIG_NUM_CAPSETS, 1);
    let device = VirtioGpuDevice::new(transport).expect("the device should initialize");
    assert!(device.host_visible_aperture().is_none());

    let context = open_context(&device);
    let blob = open_blob(&device, context, 0x1000);

    let refused = block_on(device.map_blob(blob));

    assert_eq!(refused.err(), Some(Gpu3dError::ApertureExhausted));
}

/// A blob larger than the whole aperture is refused rather than placed
/// past its end.
#[test]
fn a_blob_larger_than_the_aperture_is_refused() {
    let device = render_device(1);
    let context = open_context(&device);
    let blob = open_blob(&device, context, APERTURE_BYTES + 1);

    let refused = block_on(device.map_blob(blob));

    assert_eq!(refused.err(), Some(Gpu3dError::ApertureExhausted));
}
