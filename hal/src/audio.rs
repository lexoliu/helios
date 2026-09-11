//! The PCM playback contract.
//!
//! A playback device owns *streams* — the pipes a machine plays samples
//! down — together with the *jacks* those streams come out of and the
//! *channel maps* that say which speaker each channel of a frame is
//! meant for. All three are hardware facts rather than a consumer's
//! vocabulary: a stream is a thing that latches frames from memory at a
//! fixed rate, a jack is a physical connector whose plug can be pulled,
//! and a channel map is the geometry the silicon expects the frames to
//! be laid out in. virtio-snd implements them, and so do HD Audio and
//! I2S codecs, so the value types and the device trait live here and the
//! concrete driver encodes them onto its own wire format.
//!
//! A stream is a small state machine and the trait is that machine:
//! [`PlaybackDevice::set_params`] fixes the format, the rate and the
//! period, [`PlaybackDevice::prepare`] makes the device allocate
//! whatever it needs, [`PlaybackDevice::start`] begins consuming, and
//! [`PlaybackDevice::write`] hands over one period at a time.
//! [`PlaybackDevice::stop`] and [`PlaybackDevice::release`] undo the
//! last two. The order is the hardware's, not a convention: a device
//! asked to start a stream whose parameters it has never been given has
//! nothing to play.
//!
//! Sample buffers are never allocated here. A period is a slice the
//! caller owns and keeps owning; the device reads it between the call
//! and the future's completion and holds no reference afterwards.
//!
//! # SMP contract
//!
//! Every method takes `&self` and may be called from any processor and
//! from several tasks at once. The asynchronous ones park on the
//! device's completion notification rather than spinning, and an
//! implementation serialises access to its own rings. Two `write`
//! futures alive at once are two periods in flight, which is what keeps
//! a device from starving between periods; the order the device plays
//! them in is the order they were submitted in, so a caller that needs a
//! strict sequence submits in that order rather than awaiting each one.
//! [`PlaybackDevice::next_event`] is a single consumer: the device's
//! event ring is drained by whoever calls it, and a machine runs one
//! such reader.

use core::fmt;
use core::future::Future;

use arrayvec::ArrayVec;
use thiserror::Error;

use crate::io::IoError;

/// Largest number of streams one playback device may present.
///
/// The bound is what makes [`StreamList`] a value rather than an
/// allocation. A sound device presents one stream per direction per
/// function, and no device Helios targets carries more functions than
/// this.
pub const MAX_STREAMS: usize = 8;

/// Largest number of jacks one playback device may present.
pub const MAX_JACKS: usize = 8;

/// Largest number of channel maps one playback device may present.
pub const MAX_CHANNEL_MAPS: usize = 8;

/// Largest number of channels one map describes.
///
/// Eighteen is what a 22.2 layout needs and what every wire format
/// Helios speaks reserves room for.
pub const MAX_CHANNELS: usize = 18;

/// One pipe the device plays samples down.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamId(u32);

impl StreamId {
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    pub const fn index(self) -> u32 {
        self.0
    }
}

/// One physical connector on the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JackId(u32);

impl JackId {
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    pub const fn index(self) -> u32 {
        self.0
    }
}

/// One channel map the device publishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChannelMapId(u32);

impl ChannelMapId {
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    pub const fn index(self) -> u32 {
        self.0
    }
}

/// Which way samples travel on a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamDirection {
    /// The machine plays; the device consumes what it is handed.
    Playback,
    /// The machine records; the device produces.
    Capture,
}

/// How one sample is laid out in a period buffer.
///
/// These are the linear formats the contract carries: the ones a mixer
/// can write directly into a buffer with no codec and no packing rule.
/// Companded formats (mu-law, A-law), the 3-byte packed widths, DSD and
/// IEC958 subframes are deliberately absent — each needs a conversion
/// step that belongs to whatever produces the audio, not to a device
/// driver — so a device that offers only those offers this contract
/// nothing it can play.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SampleFormat {
    /// Signed 8-bit.
    S8,
    /// Unsigned 8-bit.
    U8,
    /// Signed 16-bit, little-endian.
    S16,
    /// Unsigned 16-bit, little-endian.
    U16,
    /// Signed 32-bit, little-endian.
    S32,
    /// Unsigned 32-bit, little-endian.
    U32,
    /// 32-bit IEEE 754 binary32.
    Float,
    /// 64-bit IEEE 754 binary64.
    Float64,
}

impl SampleFormat {
    /// Every format this contract names, in the order their bits sit in
    /// a [`SampleFormats`] set.
    pub const ALL: [Self; 8] = [
        Self::S8,
        Self::U8,
        Self::S16,
        Self::U16,
        Self::S32,
        Self::U32,
        Self::Float,
        Self::Float64,
    ];

    /// How many bytes one sample of this format occupies.
    ///
    /// A frame is this times the channel count, and a period is a whole
    /// number of frames.
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::S8 | Self::U8 => 1,
            Self::S16 | Self::U16 => 2,
            Self::S32 | Self::U32 | Self::Float => 4,
            Self::Float64 => 8,
        }
    }

    /// The name a boot line and a diagnostic spell this format with.
    pub const fn name(self) -> &'static str {
        match self {
            Self::S8 => "S8",
            Self::U8 => "U8",
            Self::S16 => "S16",
            Self::U16 => "U16",
            Self::S32 => "S32",
            Self::U32 => "U32",
            Self::Float => "FLOAT",
            Self::Float64 => "FLOAT64",
        }
    }

    const fn bit(self) -> u16 {
        1 << (self as u16)
    }
}

impl fmt::Display for SampleFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// The set of formats a stream accepts.
///
/// A set rather than a list because a device answers with a bitmap and
/// a caller asks "does it take this one?"; the bits are this contract's
/// own, so no wire numbering reaches here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SampleFormats(u16);

impl SampleFormats {
    /// The empty set: a stream that accepts nothing this contract names.
    pub const fn new() -> Self {
        Self(0)
    }

    pub const fn with(self, format: SampleFormat) -> Self {
        Self(self.0 | format.bit())
    }

    pub fn insert(&mut self, format: SampleFormat) {
        self.0 |= format.bit();
    }

    pub const fn contains(self, format: SampleFormat) -> bool {
        self.0 & format.bit() != 0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn len(self) -> u32 {
        self.0.count_ones()
    }

    /// The formats in the set, lowest bit first.
    pub fn iter(self) -> impl Iterator<Item = SampleFormat> {
        SampleFormat::ALL
            .into_iter()
            .filter(move |format| self.contains(*format))
    }
}

/// Renders a format set the way a boot line carries it: the names in
/// order, comma-separated, or `none`.
impl fmt::Display for SampleFormats {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return formatter.write_str("none");
        }
        for (index, format) in self.iter().enumerate() {
            if index != 0 {
                formatter.write_str(",")?;
            }
            formatter.write_str(format.name())?;
        }
        Ok(())
    }
}

/// A rate a stream can be clocked at.
///
/// The set is the one every PCM device and every wire format Helios
/// speaks enumerates; a rate outside it is not a rate a device offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SampleRate {
    Hz5512,
    Hz8000,
    Hz11025,
    Hz16000,
    Hz22050,
    Hz32000,
    Hz44100,
    Hz48000,
    Hz64000,
    Hz88200,
    Hz96000,
    Hz176400,
    Hz192000,
    Hz384000,
}

impl SampleRate {
    /// Every rate this contract names, ascending, in the order their
    /// bits sit in a [`SampleRates`] set.
    pub const ALL: [Self; 14] = [
        Self::Hz5512,
        Self::Hz8000,
        Self::Hz11025,
        Self::Hz16000,
        Self::Hz22050,
        Self::Hz32000,
        Self::Hz44100,
        Self::Hz48000,
        Self::Hz64000,
        Self::Hz88200,
        Self::Hz96000,
        Self::Hz176400,
        Self::Hz192000,
        Self::Hz384000,
    ];

    /// The rate in frames per second.
    pub const fn hz(self) -> u32 {
        match self {
            Self::Hz5512 => 5512,
            Self::Hz8000 => 8000,
            Self::Hz11025 => 11025,
            Self::Hz16000 => 16000,
            Self::Hz22050 => 22050,
            Self::Hz32000 => 32000,
            Self::Hz44100 => 44100,
            Self::Hz48000 => 48000,
            Self::Hz64000 => 64000,
            Self::Hz88200 => 88200,
            Self::Hz96000 => 96000,
            Self::Hz176400 => 176400,
            Self::Hz192000 => 192000,
            Self::Hz384000 => 384000,
        }
    }

    const fn bit(self) -> u16 {
        1 << (self as u16)
    }
}

impl fmt::Display for SampleRate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.hz())
    }
}

/// The set of rates a stream can be clocked at.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SampleRates(u16);

impl SampleRates {
    pub const fn new() -> Self {
        Self(0)
    }

    pub const fn with(self, rate: SampleRate) -> Self {
        Self(self.0 | rate.bit())
    }

    pub fn insert(&mut self, rate: SampleRate) {
        self.0 |= rate.bit();
    }

    pub const fn contains(self, rate: SampleRate) -> bool {
        self.0 & rate.bit() != 0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn len(self) -> u32 {
        self.0.count_ones()
    }

    /// The rates in the set, ascending.
    pub fn iter(self) -> impl DoubleEndedIterator<Item = SampleRate> {
        SampleRate::ALL
            .into_iter()
            .filter(move |rate| self.contains(*rate))
    }

    /// The slowest rate in the set.
    pub fn lowest(self) -> Option<SampleRate> {
        self.iter().next()
    }

    /// The fastest rate in the set.
    pub fn highest(self) -> Option<SampleRate> {
        self.iter().next_back()
    }
}

/// Renders a rate set the way a boot line carries it: the span it
/// covers, or `none`. The span rather than the list because the sets
/// devices publish are dense and a reader wants to know what the device
/// reaches, not to read fourteen numbers.
impl fmt::Display for SampleRates {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.lowest(), self.highest()) {
            (Some(lowest), Some(highest)) => write!(formatter, "{lowest}..{highest}"),
            _ => formatter.write_str("none"),
        }
    }
}

/// The parameters one stream is configured with.
///
/// `buffer_bytes` is the device-side ring the stream plays out of and
/// `period_bytes` is the unit it is filled in: the device reports a
/// period elapsed every `period_bytes` it consumes, so the two together
/// are what fix the latency a caller sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcmParams {
    /// Frames per second.
    pub rate: SampleRate,
    /// Channels per frame. Must lie between the stream's
    /// `channels_min` and `channels_max`.
    pub channels: u8,
    /// How one sample is laid out.
    pub format: SampleFormat,
    /// The whole device-side buffer, in bytes.
    pub buffer_bytes: u32,
    /// One period, in bytes. Must divide `buffer_bytes`.
    pub period_bytes: u32,
}

impl PcmParams {
    /// One frame, in bytes.
    pub const fn frame_bytes(&self) -> usize {
        self.format.bytes_per_sample() * self.channels as usize
    }

    /// How many periods the device-side buffer holds.
    ///
    /// `None` when the period does not divide the buffer, which is the
    /// one arithmetic relation a device requires of these two numbers.
    pub const fn periods(&self) -> Option<u32> {
        if self.period_bytes == 0 || !self.buffer_bytes.is_multiple_of(self.period_bytes) {
            return None;
        }
        Some(self.buffer_bytes / self.period_bytes)
    }
}

/// What a device says about one of its streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    pub id: StreamId,
    /// Which way its samples travel.
    pub direction: StreamDirection,
    /// The formats it accepts, restricted to the ones this contract
    /// names.
    pub formats: SampleFormats,
    /// The rates it can be clocked at.
    pub rates: SampleRates,
    /// Fewest channels per frame it accepts.
    pub channels_min: u8,
    /// Most channels per frame it accepts.
    pub channels_max: u8,
}

/// What a device says about one of its jacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JackInfo {
    pub id: JackId,
    /// Whether something is plugged into it right now. The device
    /// announces every change through [`AudioEvent::JackConnected`] and
    /// [`AudioEvent::JackDisconnected`], so a value read once goes
    /// stale.
    pub connected: bool,
    /// The function node the jack belongs to, in the codec's own
    /// numbering.
    pub hda_fn_nid: u32,
    /// The HD Audio pin default-configuration word: what the connector
    /// is, where on the chassis it is, and what colour it is.
    pub hda_defconf: u32,
    /// The HD Audio pin capabilities word.
    pub hda_caps: u32,
    /// Whether the device lets the driver point the jack at another
    /// stream.
    pub remappable: bool,
}

/// Where in the room one channel of a frame is meant to come out.
///
/// The vocabulary is the one every channel map shares — it is the same
/// list HD Audio, ALSA and the wire formats built on them enumerate — so
/// it belongs to the hardware contract rather than to a driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChannelPosition {
    /// Unset.
    None,
    /// Not applicable: the channel is present and goes nowhere.
    NotApplicable,
    Mono,
    FrontLeft,
    FrontRight,
    RearLeft,
    RearRight,
    FrontCenter,
    LowFrequency,
    SideLeft,
    SideRight,
    RearCenter,
    FrontLeftCenter,
    FrontRightCenter,
    RearLeftCenter,
    RearRightCenter,
    FrontLeftWide,
    FrontRightWide,
    FrontLeftHigh,
    FrontCenterHigh,
    FrontRightHigh,
    TopCenter,
    TopFrontLeft,
    TopFrontRight,
    TopFrontCenter,
    TopRearLeft,
    TopRearRight,
    TopRearCenter,
    TopFrontLeftCenter,
    TopFrontRightCenter,
    TopSideLeft,
    TopSideRight,
    LeftLowFrequency,
    RightLowFrequency,
    BottomCenter,
    BottomLeftCenter,
    BottomRightCenter,
}

impl ChannelPosition {
    /// The name a diagnostic spells this position with.
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::NotApplicable => "na",
            Self::Mono => "mono",
            Self::FrontLeft => "fl",
            Self::FrontRight => "fr",
            Self::RearLeft => "rl",
            Self::RearRight => "rr",
            Self::FrontCenter => "fc",
            Self::LowFrequency => "lfe",
            Self::SideLeft => "sl",
            Self::SideRight => "sr",
            Self::RearCenter => "rc",
            Self::FrontLeftCenter => "flc",
            Self::FrontRightCenter => "frc",
            Self::RearLeftCenter => "rlc",
            Self::RearRightCenter => "rrc",
            Self::FrontLeftWide => "flw",
            Self::FrontRightWide => "frw",
            Self::FrontLeftHigh => "flh",
            Self::FrontCenterHigh => "fch",
            Self::FrontRightHigh => "frh",
            Self::TopCenter => "tc",
            Self::TopFrontLeft => "tfl",
            Self::TopFrontRight => "tfr",
            Self::TopFrontCenter => "tfc",
            Self::TopRearLeft => "trl",
            Self::TopRearRight => "trr",
            Self::TopRearCenter => "trc",
            Self::TopFrontLeftCenter => "tflc",
            Self::TopFrontRightCenter => "tfrc",
            Self::TopSideLeft => "tsl",
            Self::TopSideRight => "tsr",
            Self::LeftLowFrequency => "llfe",
            Self::RightLowFrequency => "rlfe",
            Self::BottomCenter => "bc",
            Self::BottomLeftCenter => "blc",
            Self::BottomRightCenter => "brc",
        }
    }
}

impl fmt::Display for ChannelPosition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// The positions of one map, in channel order.
pub type ChannelPositions = ArrayVec<ChannelPosition, MAX_CHANNELS>;

/// One channel map the device publishes: the layout it expects frames of
/// a given channel count to be laid out in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelMap {
    pub id: ChannelMapId,
    /// Which direction the map describes.
    pub direction: StreamDirection,
    /// One position per channel, in the order the channels appear in a
    /// frame.
    pub positions: ChannelPositions,
}

/// Every stream one device presents.
pub type StreamList = ArrayVec<StreamInfo, MAX_STREAMS>;
/// Every jack one device presents.
pub type JackList = ArrayVec<JackInfo, MAX_JACKS>;
/// Every channel map one device publishes.
pub type ChannelMapList = ArrayVec<ChannelMap, MAX_CHANNEL_MAPS>;

/// What the device reports about a period it has taken.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct XferStatus {
    /// How many bytes the device still holds unplayed, at the moment it
    /// finished with this period.
    ///
    /// This is the latency a caller feels, in the only unit the device
    /// can state it in; divided by the frame size and the rate it is a
    /// time.
    pub latency_bytes: u32,
}

/// Something the device reported without being asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioEvent {
    /// The stream consumed one whole period.
    PeriodElapsed(StreamId),
    /// The stream ran out of samples before the caller supplied the
    /// next period, and what came out of the jack is a gap.
    Underrun(StreamId),
    /// Something was plugged into the jack.
    JackConnected(JackId),
    /// Something was unplugged from the jack.
    JackDisconnected(JackId),
}

/// Why a playback request did not do what it said.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum AudioError {
    /// The device could not parse the request. A driver that builds its
    /// own requests reports this as a fault in itself rather than a
    /// refusal of the caller's.
    #[error("the audio device could not parse the request")]
    BadMessage,
    /// The device does not implement the request.
    #[error("the audio device does not support the request")]
    NotSupported,
    /// The device failed while carrying the request out.
    #[error("the audio device failed while carrying out the request")]
    DeviceIo,
    /// No such stream on this device.
    #[error("stream {} does not exist on this device", .0.index())]
    UnknownStream(StreamId),
    /// The stream exists and records rather than plays.
    #[error("stream {} captures; it cannot be played to", .0.index())]
    NotPlayback(StreamId),
    /// A period was handed to a stream that has never been configured.
    #[error("stream {} has no parameters; nothing says how long a period is", .0.index())]
    NotConfigured(StreamId),
    /// The period is not the length the stream was configured with. A
    /// short period leaves the device playing whatever the rest of its
    /// buffer held, which is why it is refused rather than padded.
    #[error("stream {} takes {period_bytes}-byte periods, not {actual}", .stream.index())]
    PeriodLength {
        stream: StreamId,
        period_bytes: u32,
        actual: usize,
    },
    /// The parameters do not describe a buffer the device can play: a
    /// period that does not divide the buffer, a zero period, a channel
    /// count outside the stream's range, or a format or rate the stream
    /// does not accept.
    #[error("stream {} cannot be configured that way", .0.index())]
    InvalidParams(StreamId),
    /// The device answered with a code that belongs to no request this
    /// driver issues, which is a device fault rather than a refusal.
    #[error("the audio device answered with the unexpected code {code:#x}")]
    UnexpectedResponse { code: u32 },
    /// The transport underneath the device failed.
    #[error("audio transport: {0}")]
    Transport(#[from] IoError),
}

pub type AudioResult<T> = Result<T, AudioError>;

/// A device that plays PCM audio.
///
/// The two asynchronous queries are asked of the device rather than
/// read from a snapshot because a jack's connected state changes
/// underneath them: a plug pulled is an event, and the answer after it
/// is different from the answer before. The stream topology is the
/// exception and is read straight back, because it is silicon.
pub trait PlaybackDevice: Send + Sync + 'static {
    /// Every stream the device presents, capture streams included, so a
    /// caller can see the whole device rather than the half it may use.
    ///
    /// The one query that is not a round trip. A stream's direction,
    /// its formats, its rates and its channel range are properties of
    /// the silicon: the device answered them once, while it was brought
    /// up, and no later answer can differ. What does change underneath
    /// a caller is a jack's connected state, which is why that one is
    /// asked for again every time.
    fn stream_topology(&self) -> &StreamList;

    /// Every jack the device presents, with its connected state as of
    /// this call.
    fn jacks(&self) -> impl Future<Output = AudioResult<JackList>> + Send + '_;

    /// Every channel map the device publishes.
    fn channel_maps(&self) -> impl Future<Output = AudioResult<ChannelMapList>> + Send + '_;

    /// Fixes the format, rate, channel count and period of `stream`.
    ///
    /// Everything the device needs to know before it can allocate is in
    /// `params`; nothing plays until [`PlaybackDevice::prepare`] and
    /// [`PlaybackDevice::start`] have followed.
    fn set_params(
        &self,
        stream: StreamId,
        params: PcmParams,
    ) -> impl Future<Output = AudioResult<()>> + Send + '_;

    /// Makes the device allocate whatever the configured parameters
    /// need.
    fn prepare(&self, stream: StreamId) -> impl Future<Output = AudioResult<()>> + Send + '_;

    /// Starts the stream's clock. From here the device consumes
    /// whatever [`PlaybackDevice::write`] has handed it and reports an
    /// underrun when it runs dry.
    fn start(&self, stream: StreamId) -> impl Future<Output = AudioResult<()>> + Send + '_;

    /// Stops the stream's clock, keeping everything
    /// [`PlaybackDevice::prepare`] allocated.
    ///
    /// Stopping is the clock alone: writes the device still holds stay
    /// outstanding until [`PlaybackDevice::release`] completes them.
    fn stop(&self, stream: StreamId) -> impl Future<Output = AudioResult<()>> + Send + '_;

    /// Hands back everything [`PlaybackDevice::prepare`] allocated, and
    /// completes every [`PlaybackDevice::write`] it still holds first.
    ///
    /// The ordering is the load-bearing half of the contract: the
    /// device may not answer `release` while a write it took is still
    /// outstanding, so when the future resolves every one of them has
    /// already resolved, and a caller settling its own in-flight writes
    /// afterwards cannot park on a completion that is never coming.
    /// The stream keeps its parameters and can be prepared again.
    ///
    /// A device that cannot promise this cannot implement the trait; a
    /// `release` that failed made the promise to no one, and what the
    /// device still holds is then the device's to account for.
    fn release(&self, stream: StreamId) -> impl Future<Output = AudioResult<()>> + Send + '_;

    /// Hands the device one period of samples.
    ///
    /// `period` is exactly `period_bytes` long — the length
    /// [`PlaybackDevice::set_params`] fixed — and stays the caller's
    /// throughout: the device reads it before the future resolves and
    /// holds no reference afterwards. The status it resolves to carries
    /// how much the device still had unplayed when it took this period,
    /// which is the latency the caller is running at.
    fn write<'a>(
        &'a self,
        stream: StreamId,
        period: &'a [u8],
    ) -> impl Future<Output = AudioResult<XferStatus>> + Send + 'a;

    /// The next thing the device reported without being asked.
    ///
    /// A device that reports has to be read: its event ring is its whole
    /// buffer pool, and a ring nobody drains stops the device announcing
    /// anything at all. One reader per device.
    fn next_event(&self) -> impl Future<Output = AudioResult<AudioEvent>> + Send + '_;
}

/// A shared handle to a playback device is a playback device.
///
/// A backend hands the same driver to two owners at once — the kernel
/// task that drains its events and the interrupt route that wakes it —
/// so the shared handle satisfies the contract without every backend
/// writing the same eleven forwarding methods.
impl<Device: PlaybackDevice + ?Sized> PlaybackDevice for alloc::sync::Arc<Device> {
    fn stream_topology(&self) -> &StreamList {
        Device::stream_topology(self)
    }

    fn jacks(&self) -> impl Future<Output = AudioResult<JackList>> + Send + '_ {
        Device::jacks(self)
    }

    fn channel_maps(&self) -> impl Future<Output = AudioResult<ChannelMapList>> + Send + '_ {
        Device::channel_maps(self)
    }

    fn set_params(
        &self,
        stream: StreamId,
        params: PcmParams,
    ) -> impl Future<Output = AudioResult<()>> + Send + '_ {
        Device::set_params(self, stream, params)
    }

    fn prepare(&self, stream: StreamId) -> impl Future<Output = AudioResult<()>> + Send + '_ {
        Device::prepare(self, stream)
    }

    fn start(&self, stream: StreamId) -> impl Future<Output = AudioResult<()>> + Send + '_ {
        Device::start(self, stream)
    }

    fn stop(&self, stream: StreamId) -> impl Future<Output = AudioResult<()>> + Send + '_ {
        Device::stop(self, stream)
    }

    fn release(&self, stream: StreamId) -> impl Future<Output = AudioResult<()>> + Send + '_ {
        Device::release(self, stream)
    }

    fn write<'a>(
        &'a self,
        stream: StreamId,
        period: &'a [u8],
    ) -> impl Future<Output = AudioResult<XferStatus>> + Send + 'a {
        Device::write(self, stream, period)
    }

    fn next_event(&self) -> impl Future<Output = AudioResult<AudioEvent>> + Send + '_ {
        Device::next_event(self)
    }
}

#[cfg(test)]
mod tests {
    use super::{PcmParams, SampleFormat, SampleFormats, SampleRate, SampleRates};
    use alloc::format;
    use alloc::vec::Vec;

    /// The set QEMU's virtio-sound device offers, as a boot line spells
    /// it: the names in the contract's own order, never the wire's.
    #[test]
    fn a_format_set_renders_as_the_boot_line_carries_it() {
        let formats = SampleFormats::new()
            .with(SampleFormat::S32)
            .with(SampleFormat::S16)
            .with(SampleFormat::Float);

        assert_eq!(format!("{formats}"), "S16,S32,FLOAT");
        assert_eq!(formats.len(), 3);
        assert!(formats.contains(SampleFormat::S16));
        assert!(!formats.contains(SampleFormat::U8));
    }

    #[test]
    fn an_empty_format_set_says_so() {
        assert_eq!(format!("{}", SampleFormats::new()), "none");
        assert!(SampleFormats::new().is_empty());
    }

    /// A rate set is dense, so its line is the span it reaches rather
    /// than fourteen numbers.
    #[test]
    fn a_rate_set_renders_as_the_span_it_reaches() {
        let mut rates = SampleRates::new();
        for rate in [
            SampleRate::Hz48000,
            SampleRate::Hz8000,
            SampleRate::Hz192000,
        ] {
            rates.insert(rate);
        }

        assert_eq!(format!("{rates}"), "8000..192000");
        assert_eq!(rates.lowest(), Some(SampleRate::Hz8000));
        assert_eq!(rates.highest(), Some(SampleRate::Hz192000));
        assert_eq!(
            rates.iter().collect::<Vec<_>>(),
            alloc::vec![
                SampleRate::Hz8000,
                SampleRate::Hz48000,
                SampleRate::Hz192000
            ]
        );
    }

    #[test]
    fn an_empty_rate_set_says_so() {
        assert_eq!(format!("{}", SampleRates::new()), "none");
        assert_eq!(SampleRates::new().lowest(), None);
    }

    /// The one arithmetic relation a device requires of a buffer and a
    /// period: the period divides the buffer, and neither is zero.
    #[test]
    fn a_period_that_does_not_divide_the_buffer_describes_no_ring() {
        let params = PcmParams {
            rate: SampleRate::Hz48000,
            channels: 2,
            format: SampleFormat::S16,
            buffer_bytes: 8192,
            period_bytes: 2048,
        };

        assert_eq!(params.periods(), Some(4));
        assert_eq!(params.frame_bytes(), 4);
        assert_eq!(
            PcmParams {
                period_bytes: 3000,
                ..params
            }
            .periods(),
            None
        );
        assert_eq!(
            PcmParams {
                period_bytes: 0,
                ..params
            }
            .periods(),
            None
        );
    }
}
