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

use super::{
    CMD_GET_DISPLAY_INFO, CMD_MOVE_CURSOR, CMD_RESOURCE_ATTACH_BACKING, CMD_RESOURCE_CREATE_2D,
    CMD_RESOURCE_DETACH_BACKING, CMD_RESOURCE_FLUSH, CMD_RESOURCE_UNREF, CMD_SET_SCANOUT,
    CMD_TRANSFER_TO_HOST_2D, CMD_UPDATE_CURSOR, CTRL_HEADER_BYTES, DISPLAY_ONE_BYTES,
    RESP_ERR_INVALID_RESOURCE_ID, RESP_ERR_INVALID_SCANOUT_ID, RESP_ERR_OUT_OF_MEMORY,
    RESP_OK_DISPLAY_INFO, RESP_OK_NODATA, VirtioGpuDevice, decode_display_info,
    decode_edid_preferred_mode,
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

/// The 3D features are never asked for, however loudly a device offers
/// them.
#[test]
fn no_three_dimensional_feature_is_negotiated() {
    const VIRGL: u64 = 1 << 0;
    const RESOURCE_UUID: u64 = 1 << 2;
    const RESOURCE_BLOB: u64 = 1 << 3;
    const CONTEXT_INIT: u64 = 1 << 4;

    let device = device_with(
        1,
        VIRGL | OFFERED_EDID | RESOURCE_UUID | RESOURCE_BLOB | CONTEXT_INIT,
    );

    assert!(device.edid_supported(), "EDID is the one class bit wanted");
    let features = device.features();
    for unwanted in [VIRGL, RESOURCE_UUID, RESOURCE_BLOB, CONTEXT_INIT] {
        assert!(
            !features.device(unwanted),
            "feature {unwanted:#x} must not be negotiated by a 2D driver"
        );
    }
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
    let written = device.reap_blocking(&mut queue, token);
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
