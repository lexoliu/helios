//! virtio-snd driver tests.
//!
//! The device side is played by hand: a request is read out of the chain
//! the driver published, a canned reply is written into its writable
//! buffer, and the completion is raised. That is what lets a test assert
//! on the exact wire bytes the driver emits and on how it reads the
//! bytes a real device would answer with.
//!
//! The topology a device answers its three bring-up queries with is
//! built here from those same wire bytes and handed to the driver
//! through [`VirtioSndDevice::with_topology`], because the bring-up
//! queries are polled: a single-threaded test cannot both be inside the
//! poll and be the device answering it. The decoders those queries run
//! are exercised directly, on the bytes QEMU's device answers with, and
//! the poll's own obligation — clearing the interrupt its completion
//! raised — is exercised through [`super::reap_blocking`] on the
//! driver's real control queue.

use core::future::Future;
use core::pin::{Pin, pin};

use alloc::vec::Vec;
use futures_lite::future::{block_on, poll_once};

use helios_hal::audio::{
    AudioError, AudioEvent, ChannelPosition, JackId, PcmParams, PlaybackDevice, SampleFormat,
    SampleFormats, SampleRate, SampleRates, StreamDirection, StreamId,
};
use helios_hal::io::IoError;

use super::{
    CHMAP_INFO_BYTES, EVENT_BYTES, EVT_PCM_PERIOD_ELAPSED, EVT_PCM_XRUN, HEADER_BYTES,
    JACK_INFO_BYTES, PCM_HEADER_BYTES, PCM_INFO_BYTES, R_PCM_PREPARE, R_PCM_RELEASE,
    R_PCM_SET_PARAMS, R_PCM_START, R_PCM_STOP, S_IO_ERR, S_NOT_SUPP, S_OK, SoundTopology,
    VirtioSndDevice, XFER_STATUS_BYTES, decode_channel_maps, decode_event, decode_jacks,
    decode_streams,
};
use crate::testing::{FakeTransport, FakeTransportConfig};
use crate::transport::{DeviceType, VirtioFeatures, VirtioTransport};

/// `VIRTIO_SND_PCM_FMT_*` wire numbers QEMU's device offers.
const FMT_S16: u32 = 5;
const FMT_S32: u32 = 17;
const FMT_FLOAT: u32 = 19;
/// `VIRTIO_SND_PCM_RATE_*` wire numbers.
const RATE_8000: u32 = 1;
const RATE_44100: u32 = 6;
const RATE_48000: u32 = 7;
const RATE_192000: u32 = 12;

/// `VIRTIO_SND_CHMAP_FL` and `VIRTIO_SND_CHMAP_FR`.
const CHMAP_FL: u8 = 3;
const CHMAP_FR: u8 = 4;

fn transport_with(streams: u32, jacks: u32, chmaps: u32) -> FakeTransport {
    let transport = FakeTransport::new(FakeTransportConfig {
        device_type: DeviceType::Sound,
        offered_features: VirtioFeatures::VERSION_1.bits(),
        queue_size: 8,
        supports_queue_reset: false,
        absent_queues: &[],
    });
    transport.set_config_u32(super::CONFIG_JACKS, jacks);
    transport.set_config_u32(super::CONFIG_STREAMS, streams);
    transport.set_config_u32(super::CONFIG_CHMAPS, chmaps);
    transport
}

/// `struct virtio_snd_pcm_info` for one stream, as a device answers it.
fn pcm_info(direction: u8, formats: &[u32], rates: &[u32], channels: (u8, u8)) -> Vec<u8> {
    let mut bytes = alloc::vec![0_u8; PCM_INFO_BYTES];
    let formats: u64 = formats.iter().map(|bit| 1_u64 << bit).sum();
    let rates: u64 = rates.iter().map(|bit| 1_u64 << bit).sum();
    bytes[8..16].copy_from_slice(&formats.to_le_bytes());
    bytes[16..24].copy_from_slice(&rates.to_le_bytes());
    bytes[24] = direction;
    bytes[25] = channels.0;
    bytes[26] = channels.1;
    bytes
}

/// `struct virtio_snd_jack_info`.
fn jack_info(connected: bool, defconf: u32, remappable: bool) -> Vec<u8> {
    let mut bytes = alloc::vec![0_u8; JACK_INFO_BYTES];
    bytes[4..8].copy_from_slice(&u32::from(remappable).to_le_bytes());
    bytes[8..12].copy_from_slice(&defconf.to_le_bytes());
    bytes[16] = u8::from(connected);
    bytes
}

/// `struct virtio_snd_chmap_info`.
fn chmap_info(direction: u8, positions: &[u8]) -> Vec<u8> {
    let mut bytes = alloc::vec![0_u8; CHMAP_INFO_BYTES];
    bytes[4] = direction;
    bytes[5] = u8::try_from(positions.len()).expect("a test map is short");
    bytes[6..6 + positions.len()].copy_from_slice(positions);
    bytes
}

/// An information reply: the status word, then the items.
fn info_reply(status: u32, items: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = status.to_le_bytes().to_vec();
    for item in items {
        bytes.extend_from_slice(item);
    }
    bytes
}

/// The topology QEMU's `virtio-sound-pci` answers with: one playback
/// stream, one jack, one stereo channel map. Decoded from the wire bytes
/// rather than written out as values, so the driver's own decoders are
/// what the rest of the tests are built on.
fn qemu_topology() -> SoundTopology {
    let streams = decode_streams(
        &info_reply(
            S_OK,
            &[pcm_info(
                0,
                &[FMT_S16, FMT_S32, FMT_FLOAT],
                &[RATE_8000, RATE_44100, RATE_48000, RATE_192000],
                (1, 2),
            )],
        ),
        1,
    )
    .expect("the QEMU stream description decodes");
    let jacks = decode_jacks(&info_reply(S_OK, &[jack_info(true, 0x0141_0110, false)]), 1)
        .expect("the QEMU jack description decodes");
    let chmaps = decode_channel_maps(
        &info_reply(S_OK, &[chmap_info(0, &[CHMAP_FL, CHMAP_FR])]),
        1,
    )
    .expect("the QEMU channel map decodes");
    SoundTopology {
        streams,
        jacks,
        chmaps,
    }
}

/// A device with one playback stream, one jack and one channel map.
fn device() -> VirtioSndDevice<FakeTransport> {
    VirtioSndDevice::with_topology(transport_with(1, 1, 1), qemu_topology())
        .expect("the sound device should initialize")
}

/// The parameters QEMU's stream accepts: stereo 16-bit at 48 kHz, four
/// periods of 2048 bytes each.
fn params() -> PcmParams {
    PcmParams {
        rate: SampleRate::Hz48000,
        channels: 2,
        format: SampleFormat::S16,
        buffer_bytes: 8192,
        period_bytes: 2048,
    }
}

const STREAM: StreamId = StreamId::new(0);

/// Polls `future` once, expecting it to park on a control command, and
/// hands back the descriptor that command was published under.
///
/// The descriptor is read before the poll rather than assumed: a chain
/// takes as many descriptors as it has buffers and gives them back when
/// its completion is drained, so which identifier a command lands on is
/// the ring's business and not a number a test may spell out.
fn pending_control<Output>(
    device: &VirtioSndDevice<FakeTransport>,
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

/// The transmit-queue counterpart of [`pending_control`].
fn pending_tx<Output>(
    device: &VirtioSndDevice<FakeTransport>,
    future: Pin<&mut impl Future<Output = Output>>,
) -> u16 {
    let token = device
        .tx
        .try_lock()
        .expect("a parked driver does not hold the transmit queue lock")
        .next_free_descriptor();
    assert!(
        block_on(poll_once(future)).is_none(),
        "the period is still with the device"
    );
    token
}

/// The bytes the driver made readable in control chain `token`.
fn control_request(device: &VirtioSndDevice<FakeTransport>, token: u16) -> Vec<u8> {
    device
        .control
        .try_lock()
        .expect("a parked driver does not hold the control queue lock")
        .device_request(token)
}

/// The bytes the driver made readable in transmit chain `token`: the
/// transfer header followed by the period itself.
fn tx_request(device: &VirtioSndDevice<FakeTransport>, token: u16) -> Vec<u8> {
    device
        .tx
        .try_lock()
        .expect("a parked driver does not hold the transmit queue lock")
        .device_request(token)
}

/// Plays the device: writes `response` into control chain `token`'s
/// writable buffer and raises the interrupt.
fn answer_control(device: &VirtioSndDevice<FakeTransport>, token: u16, response: &[u8]) {
    let queue = device
        .control
        .try_lock()
        .expect("a parked driver does not hold the control queue lock");
    let written = queue.device_respond(token, response);
    queue.device_complete(token, written);
    drop(queue);
    device.handle_interrupt();
}

/// The transmit-queue counterpart of [`answer_control`].
fn answer_tx(device: &VirtioSndDevice<FakeTransport>, token: u16, status: u32, latency: u32) {
    let mut response = [0_u8; XFER_STATUS_BYTES];
    response[0..4].copy_from_slice(&status.to_le_bytes());
    response[4..8].copy_from_slice(&latency.to_le_bytes());
    let queue = device
        .tx
        .try_lock()
        .expect("a parked driver does not hold the transmit queue lock");
    let written = queue.device_respond(token, &response);
    queue.device_complete(token, written);
    drop(queue);
    device.handle_interrupt();
}

/// Runs one control command to completion, answering it with `status`,
/// and hands back the request bytes the driver emitted.
fn round_trip<Output>(
    device: &VirtioSndDevice<FakeTransport>,
    mut future: Pin<&mut impl Future<Output = Output>>,
    status: u32,
) -> (Vec<u8>, Output) {
    let token = pending_control(device, future.as_mut());
    let request = control_request(device, token);
    answer_control(device, token, &status.to_le_bytes());
    let outcome =
        block_on(poll_once(future)).expect("an answered command resolves on the next poll");
    (request, outcome)
}

/// Plays the device on the event ring: writes `event` into the slot
/// descriptor `token` carries, publishes the completion, and raises the
/// interrupt.
fn report(device: &VirtioSndDevice<FakeTransport>, token: u16, code: u32, data: u32) {
    let mut state = device
        .events
        .try_lock()
        .expect("a parked driver does not hold the event queue lock");
    let index = usize::from(state.slot_for_token[usize::from(token)]);
    let slot = &mut state.slots[index * EVENT_BYTES..(index + 1) * EVENT_BYTES];
    slot[0..4].copy_from_slice(&code.to_le_bytes());
    slot[4..8].copy_from_slice(&data.to_le_bytes());
    state.queue.device_complete(token, EVENT_BYTES as u32);
    drop(state);
    device.handle_interrupt();
}

fn command_of(request: &[u8]) -> u32 {
    u32::from_le_bytes(request[0..4].try_into().expect("a request header"))
}

fn word_at(request: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        request[offset..offset + 4]
            .try_into()
            .expect("a four-byte field"),
    )
}

#[test]
fn a_wrong_device_type_is_rejected() {
    let rejected = VirtioSndDevice::with_topology(
        FakeTransport::new(FakeTransportConfig {
            device_type: DeviceType::Gpu,
            ..FakeTransportConfig::default()
        }),
        qemu_topology(),
    )
    .err();
    assert_eq!(rejected, Some(IoError::Unsupported));
}

/// A device with no stream at all describes nothing a driver could
/// program, so bring-up says so instead of leaving a sound card that
/// answers every request with `BAD_MSG`.
#[test]
fn a_device_with_no_streams_is_refused() {
    assert_eq!(
        VirtioSndDevice::with_topology(transport_with(0, 1, 1), qemu_topology()).err(),
        Some(IoError::InvalidDeviceConfig(
            "virtio-snd device presents no PCM streams"
        ))
    );
}

/// A device that only records is not a playback device, and a machine
/// that believed it was would open a stream nothing ever comes out of.
#[test]
fn a_device_with_only_capture_streams_is_refused() {
    let capture_only = SoundTopology {
        streams: decode_streams(
            &info_reply(S_OK, &[pcm_info(1, &[FMT_S16], &[RATE_48000], (1, 2))]),
            1,
        )
        .expect("a capture stream decodes"),
        ..qemu_topology()
    };

    assert_eq!(
        VirtioSndDevice::with_topology(transport_with(1, 1, 1), capture_only).err(),
        Some(IoError::InvalidDeviceConfig(
            "virtio-snd device presents no playback stream"
        ))
    );
}

/// The `PCM_INFO` reply is the only thing that says what a stream can
/// play. QEMU's answers with three formats and a dense rate span, and
/// both have to survive into the values a boot line is rendered from.
#[test]
fn a_pcm_info_reply_describes_what_the_stream_plays() {
    let streams = decode_streams(
        &info_reply(
            S_OK,
            &[
                pcm_info(
                    0,
                    &[FMT_S16, FMT_S32, FMT_FLOAT],
                    &[RATE_8000, RATE_44100, RATE_48000, RATE_192000],
                    (1, 2),
                ),
                pcm_info(1, &[FMT_S16], &[RATE_48000], (1, 1)),
            ],
        ),
        2,
    )
    .expect("two stream descriptions decode");

    assert_eq!(streams.len(), 2);
    assert_eq!(streams[0].id, StreamId::new(0));
    assert_eq!(streams[0].direction, StreamDirection::Playback);
    assert_eq!(streams[0].channels_min, 1);
    assert_eq!(streams[0].channels_max, 2);
    assert_eq!(
        streams[0].formats,
        SampleFormats::new()
            .with(SampleFormat::S16)
            .with(SampleFormat::S32)
            .with(SampleFormat::Float)
    );
    assert_eq!(
        streams[0].rates,
        SampleRates::new()
            .with(SampleRate::Hz8000)
            .with(SampleRate::Hz44100)
            .with(SampleRate::Hz48000)
            .with(SampleRate::Hz192000)
    );
    assert_eq!(streams[1].direction, StreamDirection::Capture);
}

/// A format the audio contract does not name is dropped rather than
/// refused: a device that also offers mu-law is a device offering
/// something nothing here produces, not a broken one.
#[test]
fn a_format_outside_the_contract_is_not_reported() {
    /// `VIRTIO_SND_PCM_FMT_MU_LAW`.
    const FMT_MU_LAW: u32 = 1;

    let streams = decode_streams(
        &info_reply(
            S_OK,
            &[pcm_info(0, &[FMT_MU_LAW, FMT_S16], &[RATE_48000], (2, 2))],
        ),
        1,
    )
    .expect("the stream description decodes");

    assert_eq!(
        streams[0].formats,
        SampleFormats::new().with(SampleFormat::S16)
    );
}

/// A stream whose direction byte is neither output nor input is a
/// device this driver cannot reason about, and guessing which it meant
/// would open a recording stream as a playback one.
#[test]
fn a_stream_with_an_unknown_direction_is_a_device_fault() {
    let refused = decode_streams(
        &info_reply(S_OK, &[pcm_info(7, &[FMT_S16], &[RATE_48000], (2, 2))]),
        1,
    )
    .expect_err("an unknown direction is refused");

    assert_eq!(
        refused,
        AudioError::Transport(IoError::InvalidDeviceConfig(
            "virtio-snd device reports a stream direction that is neither output nor input"
        ))
    );
}

#[test]
fn a_jack_info_reply_describes_the_connector() {
    let jacks = decode_jacks(
        &info_reply(
            S_OK,
            &[
                jack_info(true, 0x0141_0110, false),
                jack_info(false, 0x0122_1210, true),
            ],
        ),
        2,
    )
    .expect("two jack descriptions decode");

    assert_eq!(jacks[0].id, JackId::new(0));
    assert!(jacks[0].connected);
    assert_eq!(jacks[0].hda_defconf, 0x0141_0110);
    assert!(!jacks[0].remappable);
    assert!(!jacks[1].connected);
    assert!(jacks[1].remappable);
}

#[test]
fn a_chmap_info_reply_describes_the_speaker_layout() {
    let maps = decode_channel_maps(
        &info_reply(S_OK, &[chmap_info(0, &[CHMAP_FL, CHMAP_FR])]),
        1,
    )
    .expect("a stereo map decodes");

    assert_eq!(maps[0].direction, StreamDirection::Playback);
    assert_eq!(
        maps[0].positions.as_slice(),
        [ChannelPosition::FrontLeft, ChannelPosition::FrontRight]
    );
}

/// A position outside the specification's list is a device this driver
/// cannot place in a room, and a silent `none` would put a channel
/// nowhere without saying so.
#[test]
fn a_channel_position_the_specification_does_not_define_is_refused() {
    let refused = decode_channel_maps(&info_reply(S_OK, &[chmap_info(0, &[200])]), 1)
        .expect_err("an undefined position is refused");

    assert_eq!(
        refused,
        AudioError::Transport(IoError::InvalidDeviceConfig(
            "virtio-snd channel map names a position the specification does not define"
        ))
    );
}

/// The whole state machine, in the order the hardware defines it, with
/// the wire bytes of every step asserted.
#[test]
fn a_stream_runs_from_parameters_to_release() {
    let device = device();
    let period = alloc::vec![0x5a_u8; params().period_bytes as usize];

    let (request, outcome) = round_trip(&device, pin!(device.set_params(STREAM, params())), S_OK);
    assert_eq!(outcome, Ok(()));
    assert_eq!(command_of(&request), R_PCM_SET_PARAMS);
    assert_eq!(word_at(&request, 4), STREAM.index());
    assert_eq!(word_at(&request, 8), params().buffer_bytes);
    assert_eq!(word_at(&request, 12), params().period_bytes);
    assert_eq!(
        word_at(&request, 16),
        0,
        "no PCM feature is asked for: the event ring is what this driver reads"
    );
    assert_eq!(request[20], 2, "channels");
    assert_eq!(request[21], 5, "VIRTIO_SND_PCM_FMT_S16");
    assert_eq!(request[22], 7, "VIRTIO_SND_PCM_RATE_48000");

    let (request, outcome) = round_trip(&device, pin!(device.prepare(STREAM)), S_OK);
    assert_eq!(outcome, Ok(()));
    assert_eq!(request.len(), PCM_HEADER_BYTES);
    assert_eq!(command_of(&request), R_PCM_PREPARE);
    assert_eq!(word_at(&request, 4), STREAM.index());

    let (request, outcome) = round_trip(&device, pin!(device.start(STREAM)), S_OK);
    assert_eq!(outcome, Ok(()));
    assert_eq!(command_of(&request), R_PCM_START);
    assert_eq!(word_at(&request, 4), STREAM.index());

    // One period: the transfer header names the stream, the period
    // follows it unchanged, and the status carries the latency back.
    let mut transfer = pin!(device.write(STREAM, &period));
    let token = pending_tx(&device, transfer.as_mut());
    let request = tx_request(&device, token);
    assert_eq!(word_at(&request, 0), STREAM.index());
    assert_eq!(&request[4..], period.as_slice());
    answer_tx(&device, token, S_OK, 4096);
    assert_eq!(
        block_on(poll_once(transfer)),
        Some(Ok(helios_hal::audio::XferStatus {
            latency_bytes: 4096
        })),
        "the completion carries what the device still held unplayed"
    );

    let (request, outcome) = round_trip(&device, pin!(device.stop(STREAM)), S_OK);
    assert_eq!(outcome, Ok(()));
    assert_eq!(command_of(&request), R_PCM_STOP);

    let (request, outcome) = round_trip(&device, pin!(device.release(STREAM)), S_OK);
    assert_eq!(outcome, Ok(()));
    assert_eq!(command_of(&request), R_PCM_RELEASE);
}

/// Every status the specification defines has a name of its own, and a
/// code outside them is the device answering a question this driver
/// never asked.
#[test]
fn every_status_the_device_can_answer_with_is_typed() {
    let device = device();

    let (_, outcome) = round_trip(&device, pin!(device.prepare(STREAM)), S_NOT_SUPP);
    assert_eq!(outcome, Err(AudioError::NotSupported));

    let (_, outcome) = round_trip(&device, pin!(device.start(STREAM)), S_IO_ERR);
    assert_eq!(outcome, Err(AudioError::DeviceIo));

    let (_, outcome) = round_trip(&device, pin!(device.stop(STREAM)), 0x8001);
    assert_eq!(outcome, Err(AudioError::BadMessage));

    let (_, outcome) = round_trip(&device, pin!(device.release(STREAM)), 0x1234);
    assert_eq!(
        outcome,
        Err(AudioError::UnexpectedResponse { code: 0x1234 })
    );
}

/// A period the device refuses to describe is a period nothing can say
/// was played, so the transfer fails rather than reporting a latency the
/// device never stated.
#[test]
fn a_transfer_the_device_refuses_is_typed() {
    let device = device();
    let period = alloc::vec![0_u8; params().period_bytes as usize];
    let _ = round_trip(&device, pin!(device.set_params(STREAM, params())), S_OK);

    let mut transfer = pin!(device.write(STREAM, &period));
    let token = pending_tx(&device, transfer.as_mut());
    answer_tx(&device, token, S_IO_ERR, 0);

    assert_eq!(
        block_on(poll_once(transfer)),
        Some(Err(AudioError::DeviceIo))
    );
}

/// A stream nothing has configured has no period length, so nothing can
/// say what a caller's buffer is a period of.
#[test]
fn a_period_before_the_parameters_is_refused() {
    let device = device();

    assert_eq!(
        block_on(poll_once(pin!(device.write(STREAM, &[0_u8; 2048])))),
        Some(Err(AudioError::NotConfigured(STREAM)))
    );
}

/// A short period would leave the device playing whatever the rest of
/// its buffer held, which is audible, so it is refused rather than
/// padded.
#[test]
fn a_period_of_the_wrong_length_is_refused() {
    let device = device();
    let _ = round_trip(&device, pin!(device.set_params(STREAM, params())), S_OK);

    assert_eq!(
        block_on(poll_once(pin!(device.write(STREAM, &[0_u8; 1024])))),
        Some(Err(AudioError::PeriodLength {
            stream: STREAM,
            period_bytes: 2048,
            actual: 1024,
        }))
    );
}

/// Parameters the device would refuse are refused here, where the field
/// that is wrong can still be named: the device's own answer is a single
/// `BAD_MSG`.
#[test]
fn parameters_the_stream_cannot_take_are_refused_before_the_device_sees_them() {
    let device = device();
    let refusals = [
        PcmParams {
            format: SampleFormat::U8,
            ..params()
        },
        PcmParams {
            rate: SampleRate::Hz384000,
            ..params()
        },
        PcmParams {
            channels: 6,
            ..params()
        },
        PcmParams {
            period_bytes: 3000,
            ..params()
        },
        PcmParams {
            period_bytes: 2049,
            buffer_bytes: 2049,
            ..params()
        },
    ];

    // Bring-up kicked the event ring; nothing else may reach the device.
    let kicks = device.transport.kick_count();
    for params in refusals {
        assert_eq!(
            block_on(poll_once(pin!(device.set_params(STREAM, params)))),
            Some(Err(AudioError::InvalidParams(STREAM))),
            "{params:?} names something the stream cannot play"
        );
    }
    assert_eq!(
        device.transport.kick_count(),
        kicks,
        "nothing reached the device"
    );
}

/// A stream this device does not present, and one that records rather
/// than plays, are both refused before a request is built.
#[test]
fn a_stream_that_cannot_be_played_to_is_refused() {
    let mut topology = qemu_topology();
    topology.streams = decode_streams(
        &info_reply(
            S_OK,
            &[
                pcm_info(0, &[FMT_S16], &[RATE_48000], (1, 2)),
                pcm_info(1, &[FMT_S16], &[RATE_48000], (1, 1)),
            ],
        ),
        2,
    )
    .expect("the stream descriptions decode");
    let device = VirtioSndDevice::with_topology(transport_with(2, 1, 1), topology)
        .expect("a device with a playback stream initializes");

    assert_eq!(
        block_on(poll_once(pin!(device.prepare(StreamId::new(1))))),
        Some(Err(AudioError::NotPlayback(StreamId::new(1))))
    );
    assert_eq!(
        block_on(poll_once(pin!(device.prepare(StreamId::new(7))))),
        Some(Err(AudioError::UnknownStream(StreamId::new(7))))
    );
}

/// An underrun is what the device says when the caller did not keep up,
/// and it is the one event a player has to act on.
#[test]
fn an_underrun_reaches_the_reader() {
    let device = device();
    report(&device, 0, EVT_PCM_XRUN, STREAM.index());

    assert_eq!(
        block_on(poll_once(pin!(device.next_event()))),
        Some(Ok(AudioEvent::Underrun(STREAM)))
    );
}

/// The events arrive in the order the device published them, and the
/// ring is the whole buffer pool: a slot that was read has to be back
/// with the device before the reader returns, or a device that reports
/// more than the ring is deep stalls on a guest that is keeping up.
#[test]
fn events_arrive_in_order_and_their_slots_go_straight_back() {
    let device = device();
    report(&device, 0, EVT_PCM_PERIOD_ELAPSED, 0);
    report(&device, 1, super::EVT_JACK_DISCONNECTED, 0);

    assert_eq!(
        block_on(poll_once(pin!(device.next_event()))),
        Some(Ok(AudioEvent::PeriodElapsed(STREAM)))
    );
    assert_eq!(
        block_on(poll_once(pin!(device.next_event()))),
        Some(Ok(AudioEvent::JackDisconnected(JackId::new(0))))
    );
    // Descriptor 0 is available again, which is only true if the driver
    // reposted it: the ring was full of receive buffers.
    report(&device, 0, super::EVT_JACK_CONNECTED, 0);
    assert_eq!(
        block_on(poll_once(pin!(device.next_event()))),
        Some(Ok(AudioEvent::JackConnected(JackId::new(0))))
    );
    assert!(
        block_on(poll_once(pin!(device.next_event()))).is_none(),
        "a drained ring parks instead of inventing an event"
    );
}

/// A reader that finds nothing left never parks on an interrupt the
/// device has already raised.
///
/// virtio-mmio derives its interrupt line from a read-to-clear status
/// register, so a status nobody reads holds the line asserted, and a
/// line that never falls never rises again.
#[test]
fn a_park_leaves_no_interrupt_outstanding() {
    let device = device();
    // The device reported before anyone could be listening, which is
    // what a device with an event ring does.
    device.transport.raise_interrupt(1);

    assert!(
        block_on(poll_once(pin!(device.next_event()))).is_none(),
        "an empty ring parks the reader"
    );
    assert!(
        !device.transport.ack_interrupt().used_buffer,
        "the park cleared the interrupt the device had raised"
    );
}

/// The same obligation on the bring-up path, where nothing waits on an
/// interrupt and a status register left set therefore costs the bring-up
/// nothing at all — and costs every asynchronous request afterwards
/// everything.
#[test]
fn a_bring_up_query_clears_the_interrupt_its_answer_raised() {
    let device = device();
    let request = alloc::vec![0_u8; HEADER_BYTES];
    let mut response = alloc::vec![0_u8; HEADER_BYTES];

    let mut queue = device
        .control
        .try_lock()
        .expect("bring-up owns the control queue");
    let token = queue
        .submit(
            &device.transport,
            &[request.as_slice()],
            &mut [response.as_mut_slice()],
        )
        .expect("the control queue has room for one query");
    // The device answers and raises its line, exactly as it does while
    // the bring-up path is inside its poll.
    let written = queue.device_respond(token, &S_OK.to_le_bytes());
    queue.device_complete(token, written);
    device.transport.raise_interrupt(1);

    assert_eq!(
        super::reap_blocking(&device.transport, &mut queue, token),
        written
    );
    assert!(
        !device.transport.ack_interrupt().used_buffer,
        "the bring-up query cleared the interrupt its own answer raised"
    );
}

#[test]
fn an_event_round_trips_through_its_wire_bytes() {
    let mut bytes = [0_u8; EVENT_BYTES];
    bytes[0..4].copy_from_slice(&EVT_PCM_PERIOD_ELAPSED.to_le_bytes());
    bytes[4..8].copy_from_slice(&3_u32.to_le_bytes());

    assert_eq!(
        decode_event(&bytes, EVENT_BYTES as u32),
        Ok(AudioEvent::PeriodElapsed(StreamId::new(3)))
    );
    assert_eq!(
        decode_event(&bytes, 4),
        Err(AudioError::Transport(IoError::DeviceFault)),
        "a partial event is a device that disagrees with its own ring"
    );
    bytes[0..4].copy_from_slice(&0x4321_u32.to_le_bytes());
    assert_eq!(
        decode_event(&bytes, EVENT_BYTES as u32),
        Err(AudioError::UnexpectedResponse { code: 0x4321 })
    );
}
