//! The machine's sound device, and the samples a program plays down it.
//!
//! Exactly one program holds a playback stream at a time.
//! [`Playback::claim`] takes one by the id [`available`] lists; dropping
//! what that returns — or dying — stops the stream and hands every
//! pinned page back.
//!
//! The path is one agreement and then one byte stream.
//! [`Playback::negotiate`] fixes the rate, the channel count and the
//! sample layout — a format the device does not take is refused rather
//! than answered with the nearest one it does — and
//! [`Playback::samples`] hands back the writer every frame goes into.
//! The kernel takes bytes as fast as the device takes periods and no
//! faster, so a writer that is never made to wait is a writer that has
//! run ahead of the sound.
//!
//! # What is never played
//!
//! Silence the kernel made up. Samples that stop arriving are an
//! underrun, and what comes back is [`Feedback::Xrun`] on
//! [`Playback::feedback`]: a program that wants silence sends silence.

use std::vec::Vec;

use thiserror::Error;

use crate::bindings::helios::system::audio as raw;
use crate::bindings::wit_stream;
use crate::wit_bindgen::{FutureReader, StreamReader, StreamWriter};

pub use crate::bindings::helios::system::audio::{Feedback, Format, SampleFormat, StreamInfo};

/// Why a playback request was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum AudioError {
    #[error("this machine has no sound device")]
    Unavailable,
    #[error("this sound device has no stream of that id")]
    NoSuchStream,
    #[error("that stream captures; it cannot be played to")]
    NotPlayback,
    #[error("another program already holds that playback stream")]
    AlreadyClaimed,
    #[error("this program does not hold that playback stream")]
    NotClaimed,
    #[error("this playback stream has already agreed a format")]
    AlreadyNegotiated,
    #[error("this playback stream has no negotiated format")]
    NotNegotiated,
    #[error("this stream does not accept that format")]
    UnsupportedFormat,
    #[error("no memory left for this stream's period buffers")]
    OutOfMemory,
    #[error("the sound device faulted")]
    DeviceFault,
}

impl From<raw::Error> for AudioError {
    fn from(error: raw::Error) -> Self {
        match error {
            raw::Error::Unavailable => Self::Unavailable,
            raw::Error::NoSuchStream => Self::NoSuchStream,
            raw::Error::NotPlayback => Self::NotPlayback,
            raw::Error::AlreadyClaimed => Self::AlreadyClaimed,
            raw::Error::NotClaimed => Self::NotClaimed,
            raw::Error::AlreadyNegotiated => Self::AlreadyNegotiated,
            raw::Error::NotNegotiated => Self::NotNegotiated,
            raw::Error::UnsupportedFormat => Self::UnsupportedFormat,
            raw::Error::OutOfMemory => Self::OutOfMemory,
            raw::Error::DeviceFault => Self::DeviceFault,
        }
    }
}

/// What the device says about every stream that plays, whether or not
/// somebody holds it.
pub fn available() -> Vec<StreamInfo> {
    raw::available()
}

/// How many bytes one frame of `format` occupies.
///
/// A frame is one sample per channel, interleaved, which is the unit
/// every length in this interface is a whole number of.
pub const fn frame_bytes(format: Format) -> usize {
    let sample = match format.sample_format {
        SampleFormat::Signed8 | SampleFormat::Unsigned8 => 1,
        SampleFormat::Signed16 | SampleFormat::Unsigned16 => 2,
        SampleFormat::Signed32 | SampleFormat::Unsigned32 | SampleFormat::Float32 => 4,
        SampleFormat::Float64 => 8,
    };
    sample * format.channels as usize
}

/// This program's hold on one playback stream.
pub struct Playback {
    raw: raw::Playback,
}

impl Playback {
    /// Take exclusive ownership of the stream `stream` names.
    ///
    /// The second caller is refused rather than queued: a program
    /// waiting for a stream another program holds is a provisioning
    /// mistake, not a shortage.
    pub fn claim(stream: u32) -> Result<Self, AudioError> {
        raw::claim(stream)
            .map(|raw| Self { raw })
            .map_err(AudioError::from)
    }

    /// Take the first stream this machine offers.
    ///
    /// A machine with one sound card has one playback stream, and a
    /// program that just wants to make a sound should not have to read
    /// the topology to find it.
    pub fn claim_default() -> Result<Self, AudioError> {
        let first = available()
            .into_iter()
            .next()
            .ok_or(AudioError::Unavailable)?;
        Self::claim(first.id)
    }

    /// What the device says about the stream this holds.
    pub fn info(&self) -> StreamInfo {
        self.raw.info()
    }

    /// Agree the format this stream will play at.
    ///
    /// What comes back is the format the device was configured with. A
    /// format the stream does not accept is refused: the samples were
    /// generated for the format that was asked for, and playing them as
    /// another is not something a caller can be handed silently.
    ///
    /// Called once: the period buffers are pinned here.
    pub async fn negotiate(&self, desired: Format) -> Result<Format, AudioError> {
        self.raw.negotiate(desired).await.map_err(AudioError::from)
    }

    /// Open the write side of this stream's samples.
    ///
    /// The bytes are frames in the negotiated format, interleaved, with
    /// no framing of any kind. The future resolves when the kernel has
    /// taken the last of them; dropping the writer is what says the
    /// material is over.
    pub fn samples(&self) -> (StreamWriter<u8>, SamplesTaken) {
        let (writer, reader) = wit_stream::new::<u8>();
        (writer, SamplesTaken(self.raw.samples(reader)))
    }

    /// Everything the kernel has to say about this stream while it
    /// plays: one item per period the device takes, and one per
    /// underrun.
    pub fn feedback(&self) -> StreamReader<Feedback> {
        self.raw.feedback()
    }

    /// Stop the clock and let the device release what it allocated.
    ///
    /// The samples stream ends with it. The stream stays claimed —
    /// nobody else may take it — but it cannot be negotiated again;
    /// dropping this handle is what gives it back.
    pub async fn stop(&self) -> Result<(), AudioError> {
        self.raw.stop().await.map_err(AudioError::from)
    }
}

/// The kernel's answer to a whole stream of samples: ready once the last
/// byte written has been taken.
pub struct SamplesTaken(FutureReader<Result<(), raw::Error>>);

impl SamplesTaken {
    /// Wait for the kernel to take everything that was written.
    pub async fn finished(self) -> Result<(), AudioError> {
        std::future::IntoFuture::into_future(self.0)
            .await
            .map_err(AudioError::from)
    }
}
