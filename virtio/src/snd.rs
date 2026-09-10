//! virtio-snd driver: PCM playback, with the jacks and channel maps the
//! device publishes.
//!
//! The device is a sound card with a command protocol. Four queues serve
//! it (virtio 1.2 §5.14.2): the control queue carries every question and
//! every state change, the event queue carries what the device reports
//! without being asked, and the transmit and receive queues carry the
//! samples themselves — transmit towards the device, receive back from
//! it. Only playback is driven here, so the receive queue is programmed
//! and left empty: a queue the driver never posts a buffer on is a queue
//! the device has nothing to complete, which is how a capture-capable
//! device is told this driver is not recording.
//!
//! A stream is a state machine and the control queue is how it is
//! driven: `PCM_SET_PARAMS` fixes the format, rate, channel count and
//! period, `PCM_PREPARE` makes the device allocate, `PCM_START` begins
//! its clock, and `PCM_STOP` and `PCM_RELEASE` undo the two. Every
//! reply carries a status word and every status word is checked against
//! the four the specification defines; a code outside them is a device
//! fault rather than a refusal, because it answers a question this
//! driver never asked.
//!
//! # Transmit
//!
//! One period is one chain: a four-byte `virtio_snd_pcm_xfer` naming the
//! stream, the caller's period, and an eight-byte
//! `virtio_snd_pcm_status` the device writes back. The status carries
//! `latency_bytes` — how much the device still held unplayed when it
//! took this period — which is the only number a device can state its
//! latency in, and the one the audio service reports.
//!
//! The driver never allocates a period. The bytes are the caller's, on
//! loan to the device between the submission and the completion, and the
//! transmit queue is deep enough that a caller with several `write`
//! futures alive at once has several periods in flight: a device that
//! runs out between two periods plays a gap, and the gap is audible.
//!
//! # Receive memory
//!
//! The event ring *is* the buffer pool. One eight-byte
//! `virtio_snd_event` is allocated per descriptor at bring-up and
//! recycled for the lifetime of the device: a used buffer is decoded
//! into a value and reposted before the queue lock is released, so
//! nothing on the receive path allocates and no reader can pin a buffer
//! the device needs back (AGENTS.md §3.1).
//!
//! # Concurrency contract
//!
//! Every entry point takes `&self` and may be called from any processor.
//! Control commands and transmits go through the shared
//! `submit_chain`/`await_completion` pair on their own queues, so a
//! queue lock is held only long enough to publish a chain and never
//! across an await: several commands and several periods are in flight
//! at once and completions are routed back by descriptor identifier.
//! `next_event` serialises on the event ring's own async mutex, parks on
//! the device interrupt when the ring is empty, and never holds the lock
//! across an await; it is the device's single reader. The configured
//! period length of each stream is a per-stream atomic rather than a
//! lock, because it is the one fact a transmit needs and a
//! `PCM_SET_PARAMS` on another processor is the only thing that changes
//! it. `handle_interrupt` runs in interrupt context: it acknowledges the
//! device and wakes waiters, and does nothing else.

use core::sync::atomic::{AtomicU32, Ordering};

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use async_lock::Mutex as AsyncMutex;

use helios_hal::audio::{
    AudioError, AudioEvent, AudioResult, ChannelMap, ChannelMapId, ChannelMapList, ChannelPosition,
    ChannelPositions, JackId, JackInfo, JackList, MAX_CHANNEL_MAPS, MAX_CHANNELS, MAX_JACKS,
    MAX_STREAMS, PcmParams, PlaybackDevice, SampleFormat, SampleFormats, SampleRate, SampleRates,
    StreamDirection, StreamId, StreamInfo, StreamList, XferStatus,
};
use helios_hal::io::{IoError, IoResult};

use crate::features::{NegotiatedFeatures, RING_FEATURES, negotiate};
use crate::inflight::{InFlight, await_completion, submit_chain};
use crate::notify::Notify;
use crate::queue::{VirtQueue, negotiated_queue_size};
use crate::transport::{DeviceStatus, DeviceType, VirtioTransport};

/// The control queue: every question and every state change.
const CONTROL_QUEUE_INDEX: u16 = 0;
/// The event queue: what the device reports without being asked.
const EVENT_QUEUE_INDEX: u16 = 1;
/// The transmit queue: periods on their way to the device.
const TX_QUEUE_INDEX: u16 = 2;
/// The receive queue: periods on their way back from it. Programmed and
/// never posted, because this driver does not capture.
const RX_QUEUE_INDEX: u16 = 3;

/// Depth the driver asks for on the control queue. A stream's whole
/// state machine is five commands, and a machine drives a handful of
/// streams, so this is more than the ring will ever hold at once.
const CONTROL_QUEUE_SIZE: u16 = 16;
/// Depth of the event ring, and therefore how many things the device may
/// have reported before the reader has taken any of them back. A period
/// elapsing is the frequent one, and a reader that has not run for
/// sixteen periods has problems this ring cannot fix.
const EVENT_QUEUE_SIZE: u16 = 16;
/// Depth of the transmit ring. A period is a few milliseconds, so this
/// is what lets a caller keep the device fed across a scheduling gap
/// without the ring becoming the thing that stalls.
const TX_QUEUE_SIZE: u16 = 64;
/// The receive ring carries nothing. It is programmed at its smallest
/// legal depth because a device presents four queues and a driver that
/// left one unprogrammed would be telling it the queue is absent.
const RX_QUEUE_SIZE: u16 = 1;

/// A control chain is the request and the writable reply.
const CONTROL_CHAIN_LIMIT: u16 = 2;
/// An event is a single writable buffer.
const EVENT_CHAIN_LIMIT: u16 = 1;
/// A transmit chain is the transfer header, the period, and the writable
/// status.
const TX_CHAIN_LIMIT: u16 = 3;
/// Nothing is ever chained on the receive queue.
const RX_CHAIN_LIMIT: u16 = 1;

/// Byte offsets in `struct virtio_snd_config` (virtio 1.2 §5.14.4).
const CONFIG_JACKS: usize = 0;
const CONFIG_STREAMS: usize = 4;
const CONFIG_CHMAPS: usize = 8;

/// Request codes (virtio 1.2 §5.14.6).
const R_JACK_INFO: u32 = 1;
const R_PCM_INFO: u32 = 0x0100;
const R_PCM_SET_PARAMS: u32 = 0x0101;
const R_PCM_PREPARE: u32 = 0x0102;
const R_PCM_RELEASE: u32 = 0x0103;
const R_PCM_START: u32 = 0x0104;
const R_PCM_STOP: u32 = 0x0105;
const R_CHMAP_INFO: u32 = 0x0200;

/// Event codes.
const EVT_JACK_CONNECTED: u32 = 0x1000;
const EVT_JACK_DISCONNECTED: u32 = 0x1001;
const EVT_PCM_PERIOD_ELAPSED: u32 = 0x1100;
const EVT_PCM_XRUN: u32 = 0x1101;

/// Status codes, shared by every control reply and every transfer
/// status.
const S_OK: u32 = 0x8000;
const S_BAD_MSG: u32 = 0x8001;
const S_NOT_SUPP: u32 = 0x8002;
const S_IO_ERR: u32 = 0x8003;

/// `struct virtio_snd_hdr`: one little-endian code.
const HEADER_BYTES: usize = 4;
/// `struct virtio_snd_query_info`: a header, a first item, a count and
/// the size of one item.
const QUERY_INFO_BYTES: usize = HEADER_BYTES + 12;
/// `struct virtio_snd_pcm_hdr`: a header and a stream identifier.
const PCM_HEADER_BYTES: usize = HEADER_BYTES + 4;
/// `struct virtio_snd_pcm_set_params`: a PCM header, the buffer and
/// period sizes, a feature word, and the channel count, format and rate.
const SET_PARAMS_BYTES: usize = PCM_HEADER_BYTES + 16;
/// `struct virtio_snd_pcm_info`.
const PCM_INFO_BYTES: usize = 32;
/// `struct virtio_snd_jack_info`.
const JACK_INFO_BYTES: usize = 24;
/// `struct virtio_snd_chmap_info`.
const CHMAP_INFO_BYTES: usize = 24;
/// `struct virtio_snd_event`: a code and the identifier it names.
const EVENT_BYTES: usize = 8;
/// `struct virtio_snd_pcm_xfer`: the stream a period belongs to.
const XFER_HEADER_BYTES: usize = 4;
/// `struct virtio_snd_pcm_status`: a status code and a latency.
const XFER_STATUS_BYTES: usize = 8;

/// The largest reply any query in this driver asks for.
const MAX_PCM_INFO_REPLY: usize = HEADER_BYTES + MAX_STREAMS * PCM_INFO_BYTES;
const MAX_JACK_INFO_REPLY: usize = HEADER_BYTES + MAX_JACKS * JACK_INFO_BYTES;
const MAX_CHMAP_INFO_REPLY: usize = HEADER_BYTES + MAX_CHANNEL_MAPS * CHMAP_INFO_BYTES;

/// `VIRTIO_SND_D_OUTPUT` and `VIRTIO_SND_D_INPUT`.
const D_OUTPUT: u8 = 0;
const D_INPUT: u8 = 1;

/// `VIRTIO_SND_JACK_F_REMAP`: the driver may point the jack at another
/// stream.
const JACK_F_REMAP: u32 = 1 << 0;

/// Every channel position the specification defines, in wire order
/// (virtio 1.2 §5.14.6.6.4.1). The table is the translation between the
/// wire's numbering and the contract's vocabulary, and it is a table
/// rather than arithmetic so that a position the specification adds
/// later is a compile error here instead of a silent mismatch.
const CHANNEL_POSITIONS: [ChannelPosition; 37] = [
    ChannelPosition::None,
    ChannelPosition::NotApplicable,
    ChannelPosition::Mono,
    ChannelPosition::FrontLeft,
    ChannelPosition::FrontRight,
    ChannelPosition::RearLeft,
    ChannelPosition::RearRight,
    ChannelPosition::FrontCenter,
    ChannelPosition::LowFrequency,
    ChannelPosition::SideLeft,
    ChannelPosition::SideRight,
    ChannelPosition::RearCenter,
    ChannelPosition::FrontLeftCenter,
    ChannelPosition::FrontRightCenter,
    ChannelPosition::RearLeftCenter,
    ChannelPosition::RearRightCenter,
    ChannelPosition::FrontLeftWide,
    ChannelPosition::FrontRightWide,
    ChannelPosition::FrontLeftHigh,
    ChannelPosition::FrontCenterHigh,
    ChannelPosition::FrontRightHigh,
    ChannelPosition::TopCenter,
    ChannelPosition::TopFrontLeft,
    ChannelPosition::TopFrontRight,
    ChannelPosition::TopFrontCenter,
    ChannelPosition::TopRearLeft,
    ChannelPosition::TopRearRight,
    ChannelPosition::TopRearCenter,
    ChannelPosition::TopFrontLeftCenter,
    ChannelPosition::TopFrontRightCenter,
    ChannelPosition::TopSideLeft,
    ChannelPosition::TopSideRight,
    ChannelPosition::LeftLowFrequency,
    ChannelPosition::RightLowFrequency,
    ChannelPosition::BottomCenter,
    ChannelPosition::BottomLeftCenter,
    ChannelPosition::BottomRightCenter,
];

/// Everything the device said about itself at bring-up.
///
/// The stream descriptions are what every later request is checked
/// against — a format the stream does not accept is refused here rather
/// than at the device — and the counts are what the boot line reports.
/// A jack's connected state is *not* read from here: it changes when
/// somebody pulls a plug, so [`PlaybackDevice::jacks`] asks the device
/// again.
#[derive(Clone, Debug)]
struct SoundTopology {
    streams: StreamList,
    jacks: JackList,
    chmaps: ChannelMapList,
}

impl SoundTopology {
    /// The topology of a device that has been programmed and not yet
    /// asked what it is. It never survives bring-up: `new` replaces it
    /// with what the device answered, and a device that answers with no
    /// playback stream is refused.
    fn empty() -> Self {
        Self {
            streams: StreamList::new(),
            jacks: JackList::new(),
            chmaps: ChannelMapList::new(),
        }
    }
}

/// The event ring together with the buffer pool that backs it.
///
/// Slot bookkeeping lives beside the queue rather than behind a lock of
/// its own because every path that touches it already holds the event
/// queue: a completion is drained, its event decoded, and the slot
/// reposted before the lock is released.
struct EventRing<T: VirtioTransport> {
    queue: VirtQueue<T>,
    /// One `EVENT_BYTES` slot per descriptor, in one allocation: the
    /// pool is fixed at bring-up and every slot has the same size, so
    /// there is nothing for a per-slot allocation to express.
    slots: Box<[u8]>,
    /// Which slot each outstanding descriptor identifier carries.
    slot_for_token: Box<[u16]>,
}

pub struct VirtioSndDevice<T: VirtioTransport> {
    transport: T,
    control: AsyncMutex<VirtQueue<T>>,
    control_inflight: InFlight<{ CONTROL_QUEUE_SIZE as usize }>,
    tx: AsyncMutex<VirtQueue<T>>,
    tx_inflight: InFlight<{ TX_QUEUE_SIZE as usize }>,
    /// Programmed so the device sees four queues, and never posted: this
    /// driver plays and does not record.
    rx: AsyncMutex<VirtQueue<T>>,
    events: AsyncMutex<EventRing<T>>,
    /// Raised by every device interrupt: a completion may be waiting on
    /// either the control queue or the transmit queue.
    completions: Notify,
    /// Raised when the device published on the event ring.
    event_arrivals: Notify,
    topology: SoundTopology,
    /// The period length each stream was last configured with, or zero
    /// for a stream nothing has configured. Written by `set_params` and
    /// read by `write`, which is the whole of its use; an atomic rather
    /// than a lock because there is nothing else in the word to keep
    /// consistent with it.
    period_bytes: [AtomicU32; MAX_STREAMS],
    features: NegotiatedFeatures,
}

/// How many of each kind of item the device's configuration space says
/// it presents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ItemCounts {
    streams: usize,
    jacks: usize,
    chmaps: usize,
}

impl<T: VirtioTransport> VirtioSndDevice<T> {
    /// Programs the device and its four queues, and reads everything it
    /// says about itself.
    ///
    /// The three information queries run here, on the bring-up path,
    /// because the answers are what every later request is validated
    /// against and what the boot line names. They are device handshakes
    /// rather than waits on software state — the device answers out of
    /// its own model without needing anything from this machine first —
    /// which is why the used ring may be polled for them the way
    /// virtio-gpu polls for its display info. `&mut` access to the
    /// control queue is the exclusion that makes the poll correct: the
    /// device has been handed nowhere else yet.
    pub fn new(transport: T) -> IoResult<Self> {
        let (mut device, counts) = Self::program(transport)?;
        let transport = &device.transport;
        device.topology =
            read_topology(transport, device.control.get_mut(), counts).map_err(topology_fault)?;
        device.check_playable()?;
        Ok(device)
    }

    /// The same device with a topology a test supplies, so the request
    /// paths can be driven without a device model to answer the
    /// bring-up queries. Everything else is what [`Self::new`] builds.
    #[cfg(test)]
    fn with_topology(transport: T, topology: SoundTopology) -> IoResult<Self> {
        let (mut device, _counts) = Self::program(transport)?;
        device.topology = topology;
        device.check_playable()?;
        Ok(device)
    }

    /// Negotiates, programs the four queues, fills the event ring and
    /// tells the device a driver is ready. No question is asked of the
    /// device here beyond its configuration space.
    fn program(transport: T) -> IoResult<(Self, ItemCounts)> {
        if transport.device_type() != DeviceType::Sound {
            return Err(IoError::Unsupported);
        }

        // VIRTIO_SND_F_CTLS is deliberately not asked for: it adds the
        // mixer control protocol, which is a different contract from
        // this one and has no consumer in the tree.
        let features = negotiate(&transport, RING_FEATURES)?;

        let control_size =
            negotiated_queue_size(&transport, CONTROL_QUEUE_INDEX, CONTROL_QUEUE_SIZE)?;
        let event_size = negotiated_queue_size(&transport, EVENT_QUEUE_INDEX, EVENT_QUEUE_SIZE)?;
        let tx_size = negotiated_queue_size(&transport, TX_QUEUE_INDEX, TX_QUEUE_SIZE)?;
        let rx_size = negotiated_queue_size(&transport, RX_QUEUE_INDEX, RX_QUEUE_SIZE)?;

        let control = VirtQueue::new(
            &transport,
            CONTROL_QUEUE_INDEX,
            control_size,
            CONTROL_CHAIN_LIMIT,
            features,
        )?;
        let mut event_queue = VirtQueue::new(
            &transport,
            EVENT_QUEUE_INDEX,
            event_size,
            EVENT_CHAIN_LIMIT,
            features,
        )?;
        let tx = VirtQueue::new(
            &transport,
            TX_QUEUE_INDEX,
            tx_size,
            TX_CHAIN_LIMIT,
            features,
        )?;
        let rx = VirtQueue::new(
            &transport,
            RX_QUEUE_INDEX,
            rx_size,
            RX_CHAIN_LIMIT,
            features,
        )?;

        let counts = ItemCounts {
            streams: counted(&transport, CONFIG_STREAMS, MAX_STREAMS, "streams")?,
            jacks: counted(&transport, CONFIG_JACKS, MAX_JACKS, "jacks")?,
            chmaps: counted(&transport, CONFIG_CHMAPS, MAX_CHANNEL_MAPS, "channel maps")?,
        };
        if counts.streams == 0 {
            return Err(IoError::InvalidDeviceConfig(
                "virtio-snd device presents no PCM streams",
            ));
        }

        let mut slots = vec![0_u8; usize::from(event_size) * EVENT_BYTES].into_boxed_slice();
        let mut slot_for_token = vec![0_u16; usize::from(event_size)].into_boxed_slice();
        for index in 0..usize::from(event_size) {
            let slot = &mut slots[index * EVENT_BYTES..(index + 1) * EVENT_BYTES];
            let token = event_queue.submit_output_deferred(&transport, slot)?;
            slot_for_token[usize::from(token)] =
                u16::try_from(index).map_err(|_| IoError::DeviceFault)?;
        }
        event_queue.publish();

        transport.set_status(
            DeviceStatus::ACKNOWLEDGE
                | DeviceStatus::DRIVER
                | DeviceStatus::FEATURES_OK
                | DeviceStatus::DRIVER_OK,
        );
        event_queue.notify(&transport);

        let device = Self {
            transport,
            control: AsyncMutex::new(control),
            control_inflight: InFlight::new(),
            tx: AsyncMutex::new(tx),
            tx_inflight: InFlight::new(),
            rx: AsyncMutex::new(rx),
            events: AsyncMutex::new(EventRing {
                queue: event_queue,
                slots,
                slot_for_token,
            }),
            completions: Notify::new(),
            event_arrivals: Notify::new(),
            topology: SoundTopology::empty(),
            period_bytes: [const { AtomicU32::new(0) }; MAX_STREAMS],
            features,
        };
        Ok((device, counts))
    }

    /// Refuses a device that describes nothing this driver can play.
    fn check_playable(&self) -> IoResult<()> {
        if self
            .topology
            .streams
            .iter()
            .any(|stream| stream.direction == StreamDirection::Playback)
        {
            return Ok(());
        }
        Err(IoError::InvalidDeviceConfig(
            "virtio-snd device presents no playback stream",
        ))
    }

    /// The feature set this device negotiated.
    pub fn features(&self) -> NegotiatedFeatures {
        self.features
    }

    /// The streams the device described at bring-up.
    ///
    /// Immutable: a stream's direction, formats and rates are properties
    /// of the silicon. What changes underneath a caller is a jack's
    /// connected state, which is why that one is asked for again.
    pub fn stream_topology(&self) -> &StreamList {
        &self.topology.streams
    }

    /// The jacks the device described at bring-up, with the connected
    /// state they had then.
    pub fn jack_topology(&self) -> &JackList {
        &self.topology.jacks
    }

    /// The channel maps the device published at bring-up.
    pub fn channel_map_topology(&self) -> &ChannelMapList {
        &self.topology.chmaps
    }

    /// Acknowledges the device's interrupt and wakes whoever it was for.
    ///
    /// Two kinds of waiter exist: tasks parked on a queue completion and
    /// the single reader parked on the event ring. Both are broadcast
    /// to, because an interrupt says only that something happened.
    pub fn handle_interrupt(&self) {
        self.transport.ack_interrupt();
        self.completions.notify_all();
        self.event_arrivals.notify_all();
    }

    /// Runs one control command and checks the status it answers with.
    async fn control_command(&self, request: &[u8], response: &mut [u8]) -> AudioResult<u32> {
        let written = {
            let token = submit_chain(
                &self.control_inflight,
                &self.control,
                &self.transport,
                &[request],
                &mut [&mut *response],
            )
            .await?;
            await_completion(&self.control_inflight, &self.control, token, || {
                self.completions.notified()
            })
            .await
        };
        if (written as usize) < HEADER_BYTES {
            return Err(IoError::DeviceFault.into());
        }
        check_status(status_of(response)?)?;
        Ok(written)
    }

    /// Runs one command whose whole body is a PCM header.
    async fn stream_command(&self, code: u32, stream: StreamId) -> AudioResult<()> {
        self.playback_stream(stream)?;
        let request = encode_pcm_header(code, stream);
        let mut response = [0_u8; HEADER_BYTES];
        self.control_command(&request, &mut response).await?;
        Ok(())
    }

    /// The description of `stream`, refusing one this device does not
    /// present and one that records rather than plays.
    fn playback_stream(&self, stream: StreamId) -> AudioResult<&StreamInfo> {
        let info = self
            .topology
            .streams
            .iter()
            .find(|info| info.id == stream)
            .ok_or(AudioError::UnknownStream(stream))?;
        if info.direction != StreamDirection::Playback {
            return Err(AudioError::NotPlayback(stream));
        }
        Ok(info)
    }

    /// The slot holding `stream`'s configured period length.
    ///
    /// The identifier is the device's own index, checked against the
    /// stream list before it reaches here, so it always names a slot.
    fn period_slot(&self, stream: StreamId) -> AudioResult<&AtomicU32> {
        self.period_bytes
            .get(stream.index() as usize)
            .ok_or(AudioError::UnknownStream(stream))
    }

    /// Takes one event out of the ring, if the device has published one,
    /// and reposts the slot it came in.
    fn take_event(state: &mut EventRing<T>, transport: &T) -> AudioResult<Option<AudioEvent>> {
        let EventRing {
            queue,
            slots,
            slot_for_token,
        } = state;
        let Some((token, used_len)) = queue.pop_used_with_len() else {
            return Ok(None);
        };
        let index = usize::from(
            *slot_for_token
                .get(usize::from(token))
                .ok_or(IoError::DeviceFault)?,
        );
        let slot = slots
            .get_mut(index * EVENT_BYTES..(index + 1) * EVENT_BYTES)
            .ok_or(IoError::DeviceFault)?;
        // The slot goes back to the device whatever the event turns out
        // to be: a fault that also leaked a buffer would shrink the ring
        // on top of dropping the report.
        let decoded = decode_event(slot, used_len);
        let token = queue.submit_output_deferred(transport, slot)?;
        slot_for_token[usize::from(token)] =
            u16::try_from(index).map_err(|_| IoError::DeviceFault)?;
        queue.publish();
        queue.notify(transport);
        decoded.map(Some)
    }
}

impl<T: VirtioTransport> PlaybackDevice for VirtioSndDevice<T> {
    async fn streams(&self) -> AudioResult<StreamList> {
        let request = encode_query_info(R_PCM_INFO, self.topology.streams.len(), PCM_INFO_BYTES);
        let mut response = [0_u8; MAX_PCM_INFO_REPLY];
        self.control_command(&request, &mut response).await?;
        decode_streams(&response, self.topology.streams.len())
    }

    async fn jacks(&self) -> AudioResult<JackList> {
        let request = encode_query_info(R_JACK_INFO, self.topology.jacks.len(), JACK_INFO_BYTES);
        let mut response = [0_u8; MAX_JACK_INFO_REPLY];
        self.control_command(&request, &mut response).await?;
        decode_jacks(&response, self.topology.jacks.len())
    }

    async fn channel_maps(&self) -> AudioResult<ChannelMapList> {
        let request = encode_query_info(R_CHMAP_INFO, self.topology.chmaps.len(), CHMAP_INFO_BYTES);
        let mut response = [0_u8; MAX_CHMAP_INFO_REPLY];
        self.control_command(&request, &mut response).await?;
        decode_channel_maps(&response, self.topology.chmaps.len())
    }

    async fn set_params(&self, stream: StreamId, params: PcmParams) -> AudioResult<()> {
        let info = *self.playback_stream(stream)?;
        check_params(&info, &params)?;
        let request = encode_set_params(stream, &params);
        let mut response = [0_u8; HEADER_BYTES];
        self.control_command(&request, &mut response).await?;
        // Only once the device has taken them: a length remembered for a
        // configuration the device refused would let the next `write`
        // hand it a period it never agreed to.
        self.period_slot(stream)?
            .store(params.period_bytes, Ordering::Release);
        Ok(())
    }

    async fn prepare(&self, stream: StreamId) -> AudioResult<()> {
        self.stream_command(R_PCM_PREPARE, stream).await
    }

    async fn start(&self, stream: StreamId) -> AudioResult<()> {
        self.stream_command(R_PCM_START, stream).await
    }

    async fn stop(&self, stream: StreamId) -> AudioResult<()> {
        self.stream_command(R_PCM_STOP, stream).await
    }

    async fn release(&self, stream: StreamId) -> AudioResult<()> {
        // The parameters survive a release — the specification puts the
        // stream back in the state `PCM_SET_PARAMS` left it in — so the
        // remembered period length stays with it.
        self.stream_command(R_PCM_RELEASE, stream).await
    }

    async fn write(&self, stream: StreamId, period: &[u8]) -> AudioResult<XferStatus> {
        self.playback_stream(stream)?;
        let period_bytes = self.period_slot(stream)?.load(Ordering::Acquire);
        if period_bytes == 0 {
            return Err(AudioError::NotConfigured(stream));
        }
        if period.len() as u64 != u64::from(period_bytes) {
            return Err(AudioError::PeriodLength {
                stream,
                period_bytes,
                actual: period.len(),
            });
        }

        let header: [u8; XFER_HEADER_BYTES] = stream.index().to_le_bytes();
        let mut status = [0_u8; XFER_STATUS_BYTES];
        let written = {
            let token = submit_chain(
                &self.tx_inflight,
                &self.tx,
                &self.transport,
                &[&header, period],
                &mut [&mut status],
            )
            .await?;
            await_completion(&self.tx_inflight, &self.tx, token, || {
                self.completions.notified()
            })
            .await
        };
        if (written as usize) < XFER_STATUS_BYTES {
            // The device returned the chain without saying what it did
            // with the period, so nothing here can say whether it was
            // played.
            return Err(IoError::DeviceFault.into());
        }
        check_status(status_of(&status)?)?;
        Ok(XferStatus {
            latency_bytes: word_at(&status, 4)?,
        })
    }

    async fn next_event(&self) -> AudioResult<AudioEvent> {
        loop {
            // Armed before the ring is drained: an event the device
            // publishes in between belongs to this wait, not to the next
            // interrupt.
            let notified = self.event_arrivals.notified();
            {
                let mut state = self.events.lock().await;
                if let Some(event) = Self::take_event(&mut state, &self.transport)? {
                    return Ok(event);
                }
                // The ring is empty, so this reader is about to park and
                // nobody is looking at the device: it hands the interrupt
                // line back to the platform before it does.
                //
                // The interrupt-status register is read-to-clear, and on
                // a transport whose interrupt line is a function of it —
                // virtio-mmio's is — a status nobody reads holds that
                // line asserted. A line that never falls never rises
                // again, so an edge-triggered controller sees no further
                // interrupt and the device goes silent for the life of
                // the machine. A sound device reports without being
                // asked, so it can raise its line before the platform has
                // routed that line anywhere, and the reader's first park
                // is what clears the raise nobody could deliver.
                self.transport.ack_interrupt();
                // The device may have published between the drain above
                // and that acknowledgement, in which case the line it
                // raised has just been cleared and the ring holds an
                // event the wait below would never be woken for.
                if let Some(event) = Self::take_event(&mut state, &self.transport)? {
                    return Ok(event);
                }
            }
            notified.await;
        }
    }
}

impl<T: VirtioTransport> Drop for VirtioSndDevice<T> {
    fn drop(&mut self) {
        self.control.get_mut().shutdown(&self.transport);
        self.events.get_mut().queue.shutdown(&self.transport);
        self.tx.get_mut().shutdown(&self.transport);
        self.rx.get_mut().shutdown(&self.transport);
    }
}

/// Announces one sound device on the line a backend's boot log carries.
///
/// It lives here rather than in each backend because the three of them
/// would otherwise each render the same topology their own way, and a
/// boot line is evidence: it has to read the same whichever machine
/// produced it (AGENTS.md §1). The formats and rates are the first
/// playback stream's, because that is the stream a machine with one
/// sound card plays down; the rest of the topology goes to the debug log
/// rather than lengthening the line.
pub(crate) fn report_snd_online<T: VirtioTransport>(device: &VirtioSndDevice<T>, transport: &str) {
    let streams = device.stream_topology();
    let playback = streams
        .iter()
        .find(|stream| stream.direction == StreamDirection::Playback)
        .expect("bring-up refuses a virtio-snd device with no playback stream");
    tracing::info!(
        "virtio-snd online transport={transport} streams={} jacks={} rates={} formats={}",
        streams.len(),
        device.jack_topology().len(),
        playback.rates,
        playback.formats
    );
    for stream in streams {
        tracing::debug!(
            stream = stream.id.index(),
            direction = ?stream.direction,
            channels_min = stream.channels_min,
            channels_max = stream.channels_max,
            "virtio-snd stream"
        );
    }
    for jack in device.jack_topology() {
        tracing::debug!(
            jack = jack.id.index(),
            connected = jack.connected,
            defconf = jack.hda_defconf,
            remappable = jack.remappable,
            "virtio-snd jack"
        );
    }
    for chmap in device.channel_map_topology() {
        tracing::debug!(
            chmap = chmap.id.index(),
            direction = ?chmap.direction,
            channels = chmap.positions.len(),
            "virtio-snd channel map"
        );
    }
}

/// One configuration count, refused when it is more than the contract
/// holds.
fn counted<T: VirtioTransport>(
    transport: &T,
    offset: usize,
    limit: usize,
    what: &'static str,
) -> IoResult<usize> {
    let count = transport.read_config_u32(offset) as usize;
    if count > limit {
        tracing::error!(count, limit, what, "virtio-snd device presents too many");
        return Err(IoError::InvalidDeviceConfig(
            "virtio-snd device presents more streams, jacks or channel maps than the audio contract holds",
        ));
    }
    Ok(count)
}

/// Turns a bring-up query failure into the transport error a
/// constructor answers with, naming what the device did on the way.
fn topology_fault(error: AudioError) -> IoError {
    tracing::error!(%error, "virtio-snd did not describe itself at bring-up");
    match error {
        AudioError::Transport(io) => io,
        _ => IoError::DeviceFault,
    }
}

/// Reads the three descriptions the device answers with, on the
/// bring-up path.
fn read_topology<T: VirtioTransport>(
    transport: &T,
    control: &mut VirtQueue<T>,
    counts: ItemCounts,
) -> AudioResult<SoundTopology> {
    let ItemCounts {
        streams,
        jacks,
        chmaps,
    } = counts;
    let reply = query_blocking(
        transport,
        control,
        &encode_query_info(R_PCM_INFO, streams, PCM_INFO_BYTES),
        HEADER_BYTES + streams * PCM_INFO_BYTES,
    )?;
    let streams = decode_streams(&reply, streams)?;

    let jacks = if jacks == 0 {
        JackList::new()
    } else {
        let reply = query_blocking(
            transport,
            control,
            &encode_query_info(R_JACK_INFO, jacks, JACK_INFO_BYTES),
            HEADER_BYTES + jacks * JACK_INFO_BYTES,
        )?;
        decode_jacks(&reply, jacks)?
    };

    let chmaps = if chmaps == 0 {
        ChannelMapList::new()
    } else {
        let reply = query_blocking(
            transport,
            control,
            &encode_query_info(R_CHMAP_INFO, chmaps, CHMAP_INFO_BYTES),
            HEADER_BYTES + chmaps * CHMAP_INFO_BYTES,
        )?;
        decode_channel_maps(&reply, chmaps)?
    };

    Ok(SoundTopology {
        streams,
        jacks,
        chmaps,
    })
}

/// One control round trip on the bring-up path, polled rather than
/// awaited.
///
/// Both buffers are kernel-heap allocations rather than locals, and that
/// is not a style choice: this runs on the boot stack, which the
/// platform maps outside the window the device's DMA pool translates, so
/// a device handed a stack address reads and writes memory that is not
/// this buffer and answers with a completion whose reply never arrives.
/// Every later command builds its buffers inside a task future, which
/// lives in the kernel's task arena and translates correctly.
fn query_blocking<T: VirtioTransport>(
    transport: &T,
    queue: &mut VirtQueue<T>,
    request: &[u8],
    response_bytes: usize,
) -> AudioResult<Vec<u8>> {
    let request = request.to_vec();
    let mut response = vec![0_u8; response_bytes];
    let token = queue.submit(
        transport,
        &[request.as_slice()],
        &mut [response.as_mut_slice()],
    )?;
    queue.notify(transport);
    let written = reap_blocking(transport, queue, token);
    if (written as usize) < HEADER_BYTES {
        return Err(IoError::DeviceFault.into());
    }
    check_status(status_of(&response)?)?;
    Ok(response)
}

/// Waits for `token`'s completion on the bring-up path and clears the
/// interrupt that completion raised.
///
/// The acknowledgement is the half that is easy to leave out and
/// impossible to notice here: nothing on this path waits on an
/// interrupt, so a status register left set costs the bring-up nothing
/// at all. It costs everything afterwards. A virtio-mmio line is
/// edge-triggered, so a line that was raised and never lowered cannot
/// rise again — and every asynchronous request the driver makes once the
/// executor is running parks on an interrupt that can no longer arrive.
fn reap_blocking<T: VirtioTransport>(transport: &T, queue: &mut VirtQueue<T>, token: u16) -> u32 {
    let written = loop {
        match queue.pop_used_with_len() {
            Some((completed, written)) => {
                assert_eq!(
                    completed, token,
                    "virtio-snd answered a bring-up query that was never issued"
                );
                break written;
            }
            None => core::hint::spin_loop(),
        }
    };
    transport.ack_interrupt();
    written
}

/// Whether the parameters describe something this stream can play.
///
/// Every one of these is checked here rather than left to the device
/// because the device's own refusal is a single `BAD_MSG` that says
/// nothing about which field was wrong.
fn check_params(info: &StreamInfo, params: &PcmParams) -> AudioResult<()> {
    if params.periods().is_none() {
        return Err(AudioError::InvalidParams(info.id));
    }
    if !info.formats.contains(params.format) || !info.rates.contains(params.rate) {
        return Err(AudioError::InvalidParams(info.id));
    }
    if params.channels < info.channels_min || params.channels > info.channels_max {
        return Err(AudioError::InvalidParams(info.id));
    }
    let frame_bytes = params.frame_bytes();
    if frame_bytes == 0 || !(params.period_bytes as usize).is_multiple_of(frame_bytes) {
        // A period that ends mid-frame leaves the device to decide what
        // the missing channels play.
        return Err(AudioError::InvalidParams(info.id));
    }
    Ok(())
}

/// Turns a status word into either "this is the answer that was asked
/// for" or the typed refusal it carries.
fn check_status(code: u32) -> AudioResult<()> {
    match code {
        S_OK => Ok(()),
        S_BAD_MSG => Err(AudioError::BadMessage),
        S_NOT_SUPP => Err(AudioError::NotSupported),
        S_IO_ERR => Err(AudioError::DeviceIo),
        code => Err(AudioError::UnexpectedResponse { code }),
    }
}

/// The status word a reply opens with.
fn status_of(response: &[u8]) -> AudioResult<u32> {
    word_at(response, 0)
}

fn word_at(bytes: &[u8], offset: usize) -> AudioResult<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .and_then(|slice| slice.try_into().ok())
            .ok_or(IoError::DeviceFault)?,
    ))
}

fn long_at(bytes: &[u8], offset: usize) -> AudioResult<u64> {
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..offset + 8)
            .and_then(|slice| slice.try_into().ok())
            .ok_or(IoError::DeviceFault)?,
    ))
}

/// `struct virtio_snd_query_info` asking for every item of one kind.
fn encode_query_info(code: u32, count: usize, item_bytes: usize) -> [u8; QUERY_INFO_BYTES] {
    let mut bytes = [0_u8; QUERY_INFO_BYTES];
    bytes[0..4].copy_from_slice(&code.to_le_bytes());
    // Always from the first item: the driver holds every one of them, so
    // there is no window to ask for.
    bytes[8..12].copy_from_slice(
        &u32::try_from(count)
            .expect("the item count is bounded by the audio contract")
            .to_le_bytes(),
    );
    bytes[12..16].copy_from_slice(
        &u32::try_from(item_bytes)
            .expect("an item size is a small constant")
            .to_le_bytes(),
    );
    bytes
}

/// `struct virtio_snd_pcm_hdr`: a command naming one stream.
fn encode_pcm_header(code: u32, stream: StreamId) -> [u8; PCM_HEADER_BYTES] {
    let mut bytes = [0_u8; PCM_HEADER_BYTES];
    bytes[0..4].copy_from_slice(&code.to_le_bytes());
    bytes[4..8].copy_from_slice(&stream.index().to_le_bytes());
    bytes
}

/// `struct virtio_snd_pcm_set_params`.
fn encode_set_params(stream: StreamId, params: &PcmParams) -> [u8; SET_PARAMS_BYTES] {
    let mut bytes = [0_u8; SET_PARAMS_BYTES];
    bytes[0..4].copy_from_slice(&R_PCM_SET_PARAMS.to_le_bytes());
    bytes[4..8].copy_from_slice(&stream.index().to_le_bytes());
    bytes[8..12].copy_from_slice(&params.buffer_bytes.to_le_bytes());
    bytes[12..16].copy_from_slice(&params.period_bytes.to_le_bytes());
    // No PCM feature is asked for: message polling and shared-memory
    // periods are alternatives to the event ring this driver reads.
    bytes[16..20].copy_from_slice(&0_u32.to_le_bytes());
    bytes[20] = params.channels;
    bytes[21] = wire_format(params.format);
    bytes[22] = wire_rate(params.rate);
    bytes
}

/// Decodes one used event slot.
fn decode_event(slot: &[u8], used_len: u32) -> AudioResult<AudioEvent> {
    // The device writes whole events and nothing else; a short or long
    // one means the ring and the device disagree about the buffer, and
    // guessing which half is real would report something that never
    // happened.
    if usize::try_from(used_len).map_err(|_| IoError::DeviceFault)? != EVENT_BYTES {
        return Err(IoError::DeviceFault.into());
    }
    let code = word_at(slot, 0)?;
    let data = word_at(slot, 4)?;
    match code {
        EVT_JACK_CONNECTED => Ok(AudioEvent::JackConnected(JackId::new(data))),
        EVT_JACK_DISCONNECTED => Ok(AudioEvent::JackDisconnected(JackId::new(data))),
        EVT_PCM_PERIOD_ELAPSED => Ok(AudioEvent::PeriodElapsed(StreamId::new(data))),
        EVT_PCM_XRUN => Ok(AudioEvent::Underrun(StreamId::new(data))),
        code => Err(AudioError::UnexpectedResponse { code }),
    }
}

/// Decodes the `struct virtio_snd_pcm_info` array of a `PCM_INFO` reply.
fn decode_streams(response: &[u8], count: usize) -> AudioResult<StreamList> {
    let mut streams = StreamList::new();
    for index in 0..count {
        let entry = item(response, index, PCM_INFO_BYTES)?;
        let direction = direction_from_wire(entry[24])?;
        streams.push(StreamInfo {
            id: StreamId::new(u32::try_from(index).map_err(|_| IoError::DeviceFault)?),
            direction,
            formats: decode_formats(long_at(entry, 8)?),
            rates: decode_rates(long_at(entry, 16)?),
            channels_min: entry[25],
            channels_max: entry[26],
        });
    }
    Ok(streams)
}

/// Decodes the `struct virtio_snd_jack_info` array of a `JACK_INFO`
/// reply.
fn decode_jacks(response: &[u8], count: usize) -> AudioResult<JackList> {
    let mut jacks = JackList::new();
    for index in 0..count {
        let entry = item(response, index, JACK_INFO_BYTES)?;
        jacks.push(JackInfo {
            id: JackId::new(u32::try_from(index).map_err(|_| IoError::DeviceFault)?),
            connected: entry[16] != 0,
            hda_fn_nid: word_at(entry, 0)?,
            hda_defconf: word_at(entry, 8)?,
            hda_caps: word_at(entry, 12)?,
            remappable: word_at(entry, 4)? & JACK_F_REMAP != 0,
        });
    }
    Ok(jacks)
}

/// Decodes the `struct virtio_snd_chmap_info` array of a `CHMAP_INFO`
/// reply.
fn decode_channel_maps(response: &[u8], count: usize) -> AudioResult<ChannelMapList> {
    let mut maps = ChannelMapList::new();
    for index in 0..count {
        let entry = item(response, index, CHMAP_INFO_BYTES)?;
        let channels = usize::from(entry[5]);
        if channels > MAX_CHANNELS {
            return Err(IoError::InvalidDeviceConfig(
                "virtio-snd channel map names more channels than one map may carry",
            )
            .into());
        }
        let mut positions = ChannelPositions::new();
        for position in &entry[6..6 + channels] {
            positions.push(*CHANNEL_POSITIONS.get(usize::from(*position)).ok_or(
                IoError::InvalidDeviceConfig(
                    "virtio-snd channel map names a position the specification does not define",
                ),
            )?);
        }
        maps.push(ChannelMap {
            id: ChannelMapId::new(u32::try_from(index).map_err(|_| IoError::DeviceFault)?),
            direction: direction_from_wire(entry[4])?,
            positions,
        });
    }
    Ok(maps)
}

/// The `index`th item of an information reply, past the status header.
fn item(response: &[u8], index: usize, item_bytes: usize) -> AudioResult<&[u8]> {
    let start = HEADER_BYTES + index * item_bytes;
    Ok(response
        .get(start..start + item_bytes)
        .ok_or(IoError::DeviceFault)?)
}

/// The formats a `PCM_INFO` bitmap names, restricted to the ones the
/// audio contract carries.
///
/// A format the contract does not name is dropped rather than refused:
/// a device that also offers mu-law is not a broken device, it is a
/// device offering something nothing here produces.
fn decode_formats(bitmap: u64) -> SampleFormats {
    let mut formats = SampleFormats::new();
    for bit in 0..u64::BITS {
        if bitmap & (1 << bit) == 0 {
            continue;
        }
        if let Some(format) = format_from_wire(bit) {
            formats.insert(format);
        }
    }
    formats
}

/// The rates a `PCM_INFO` bitmap names.
fn decode_rates(bitmap: u64) -> SampleRates {
    let mut rates = SampleRates::new();
    for bit in 0..u64::BITS {
        if bitmap & (1 << bit) == 0 {
            continue;
        }
        if let Some(rate) = rate_from_wire(bit) {
            rates.insert(rate);
        }
    }
    rates
}

fn direction_from_wire(direction: u8) -> AudioResult<StreamDirection> {
    match direction {
        D_OUTPUT => Ok(StreamDirection::Playback),
        D_INPUT => Ok(StreamDirection::Capture),
        _ => Err(IoError::InvalidDeviceConfig(
            "virtio-snd device reports a stream direction that is neither output nor input",
        )
        .into()),
    }
}

/// The wire number of a sample format (`enum virtio_snd_pcm_fmt`, virtio
/// 1.2 §5.14.6.6.3.1).
const fn wire_format(format: SampleFormat) -> u8 {
    match format {
        SampleFormat::S8 => 3,
        SampleFormat::U8 => 4,
        SampleFormat::S16 => 5,
        SampleFormat::U16 => 6,
        SampleFormat::S32 => 17,
        SampleFormat::U32 => 18,
        SampleFormat::Float => 19,
        SampleFormat::Float64 => 20,
    }
}

const fn format_from_wire(bit: u32) -> Option<SampleFormat> {
    match bit {
        3 => Some(SampleFormat::S8),
        4 => Some(SampleFormat::U8),
        5 => Some(SampleFormat::S16),
        6 => Some(SampleFormat::U16),
        17 => Some(SampleFormat::S32),
        18 => Some(SampleFormat::U32),
        19 => Some(SampleFormat::Float),
        20 => Some(SampleFormat::Float64),
        _ => None,
    }
}

/// The wire number of a rate (`enum virtio_snd_pcm_rate`).
const fn wire_rate(rate: SampleRate) -> u8 {
    match rate {
        SampleRate::Hz5512 => 0,
        SampleRate::Hz8000 => 1,
        SampleRate::Hz11025 => 2,
        SampleRate::Hz16000 => 3,
        SampleRate::Hz22050 => 4,
        SampleRate::Hz32000 => 5,
        SampleRate::Hz44100 => 6,
        SampleRate::Hz48000 => 7,
        SampleRate::Hz64000 => 8,
        SampleRate::Hz88200 => 9,
        SampleRate::Hz96000 => 10,
        SampleRate::Hz176400 => 11,
        SampleRate::Hz192000 => 12,
        SampleRate::Hz384000 => 13,
    }
}

const fn rate_from_wire(bit: u32) -> Option<SampleRate> {
    match bit {
        0 => Some(SampleRate::Hz5512),
        1 => Some(SampleRate::Hz8000),
        2 => Some(SampleRate::Hz11025),
        3 => Some(SampleRate::Hz16000),
        4 => Some(SampleRate::Hz22050),
        5 => Some(SampleRate::Hz32000),
        6 => Some(SampleRate::Hz44100),
        7 => Some(SampleRate::Hz48000),
        8 => Some(SampleRate::Hz64000),
        9 => Some(SampleRate::Hz88200),
        10 => Some(SampleRate::Hz96000),
        11 => Some(SampleRate::Hz176400),
        12 => Some(SampleRate::Hz192000),
        13 => Some(SampleRate::Hz384000),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
