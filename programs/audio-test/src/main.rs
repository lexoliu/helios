//! `audio-test`: plays a tone down the machine's sound device and says
//! what came of it.
//!
//! It is the guest side of the audio path's acceptance evidence. It
//! claims the first playback stream `helios:system/audio` lists,
//! negotiates 48 kHz stereo S16, and writes two seconds of a 440 Hz
//! sine into the stream's sample path while reading the feedback the
//! kernel publishes back.
//!
//! Every step prints a line, because a capture that comes back silent
//! has to be told apart from a program that never claimed a stream.
//!
//! The tone is deterministic on purpose. A host reading the recording
//! back knows what it should hold — one partial, at a frequency it can
//! measure, for a length it can count — so the check is "is this the
//! sound" rather than "is this not silence".

use std::env;

use helios_api::audio::{
    AudioError, Feedback, Format, Playback, SampleFormat, StreamInfo, frame_bytes,
};
use helios_api::channel::bounded;
use helios_api::task::spawn;
use thiserror::Error;

/// The partial the tone is made of, in hertz.
///
/// A' above middle C: high enough that a two-second recording holds
/// nearly nine hundred cycles, low enough that every rate a device
/// offers reproduces it far below Nyquist.
const TONE_HZ: f64 = 440.0;

/// The rate the tone is generated at.
const RATE_HZ: u32 = 48_000;

/// How far from full scale each sample peaks.
///
/// Short of the rail so that neither the device's own mixing nor a
/// host backend's resampling clips it, which would put partials in the
/// recording that the guest never played.
const AMPLITUDE: f64 = 0.5;

/// How much of the tone is written per call.
///
/// One period's worth of frames, which is what the kernel takes from
/// the stream at a time; a larger batch only sits in the writer waiting
/// for the same room.
const BATCH_FRAMES: usize = 480;

#[derive(Debug, Error)]
enum AudioTestError {
    #[error("usage: audio-test [--seconds <n>] [--stream <id>]")]
    Usage,
    #[error("--{option} needs a number, not {value:?}")]
    NotANumber { option: &'static str, value: String },
    #[error("the audio service refused: {0}")]
    Audio(#[from] AudioError),
    #[error("the kernel stopped taking samples after {written} of {total} bytes")]
    Truncated { written: usize, total: usize },
}

struct Options {
    /// How many seconds of tone to play.
    seconds: u32,
    /// The stream to claim. Absent means the first one the machine
    /// offers.
    stream: Option<u32>,
}

fn parse_options() -> Result<Options, AudioTestError> {
    let mut options = Options {
        seconds: 2,
        stream: None,
    };
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--seconds" => {
                let value = arguments.next().ok_or(AudioTestError::Usage)?;
                options.seconds = value.parse().map_err(|_| AudioTestError::NotANumber {
                    option: "seconds",
                    value,
                })?;
            }
            "--stream" => {
                let value = arguments.next().ok_or(AudioTestError::Usage)?;
                options.stream = Some(value.parse().map_err(|_| AudioTestError::NotANumber {
                    option: "stream",
                    value,
                })?);
            }
            _ => return Err(AudioTestError::Usage),
        }
    }
    Ok(options)
}

/// The name a line spells a sample layout with.
const fn format_name(format: SampleFormat) -> &'static str {
    match format {
        SampleFormat::Signed8 => "S8",
        SampleFormat::Unsigned8 => "U8",
        SampleFormat::Signed16 => "S16",
        SampleFormat::Unsigned16 => "U16",
        SampleFormat::Signed32 => "S32",
        SampleFormat::Unsigned32 => "U32",
        SampleFormat::Float32 => "FLOAT32",
        SampleFormat::Float64 => "FLOAT64",
    }
}

fn print_stream(info: &StreamInfo) {
    println!(
        "audio-test:claimed stream={} rates={} formats={} channels={}..{}",
        info.id,
        info.rates.len(),
        info.formats.len(),
        info.channels_min,
        info.channels_max
    );
}

/// What the feedback reader saw over the whole run.
#[derive(Clone, Copy)]
struct Reports {
    /// The most recent latency the device reported.
    latency_bytes: u32,
    /// How many times it ran dry.
    xruns: u32,
}

/// Feedback items one read of the stream takes at once.
const FEEDBACK_BATCH: usize = 64;

/// One batch of the tone, starting at frame `from`.
///
/// The phase is a function of the absolute frame number rather than of
/// anything carried between batches, so a batch boundary leaves no step
/// in the waveform and the recording holds one partial and nothing else.
fn tone_batch(from: u64, frames: usize, channels: u8) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(frames * (channels as usize) * 2);
    for frame in 0..frames {
        let phase =
            core::f64::consts::TAU * TONE_HZ * ((from + frame as u64) as f64) / f64::from(RATE_HZ);
        let sample = (AMPLITUDE * phase.sin() * f64::from(i16::MAX)) as i16;
        for _ in 0..channels {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
    }
    bytes
}

#[helios_api::main]
async fn main() -> Result<(), AudioTestError> {
    let options = parse_options()?;

    let playback = match options.stream {
        Some(stream) => Playback::claim(stream)?,
        None => Playback::claim_default()?,
    };
    print_stream(&playback.info());

    let format = playback
        .negotiate(Format {
            rate: RATE_HZ,
            channels: 2,
            sample_format: SampleFormat::Signed16,
        })
        .await?;
    println!(
        "audio-test:negotiated rate={} channels={} format={}",
        format.rate,
        format.channels,
        format_name(format.sample_format)
    );

    // The feedback reader is opened before a byte is written, so an
    // underrun caused by the very first period is one this program sees.
    // It is read by a task of its own, because the kernel publishes an
    // item per period while this program is busy writing the next one,
    // and a reader that only looked at the end would find the oldest of
    // them already dropped.
    let mut feedback = playback.feedback();
    let (report, reports) = bounded::<Reports>(1);
    spawn(async move {
        let mut summary = Reports {
            latency_bytes: 0,
            xruns: 0,
        };
        loop {
            let (result, burst) = feedback.read(Vec::with_capacity(FEEDBACK_BATCH)).await;
            for item in burst {
                match item {
                    Feedback::LatencyBytes(bytes) => summary.latency_bytes = bytes,
                    Feedback::Xrun => summary.xruns += 1,
                }
            }
            if helios_api::stream_closed(result) {
                let _ = report.send(summary).await;
                return;
            }
        }
    });

    let total_frames = u64::from(options.seconds) * u64::from(RATE_HZ);
    let total_bytes = (total_frames as usize) * frame_bytes(format);
    let (mut samples, taken) = playback.samples();
    let mut written = 0_usize;
    let mut frame = 0_u64;
    while frame < total_frames {
        let frames = BATCH_FRAMES.min((total_frames - frame) as usize);
        let batch = tone_batch(frame, frames, format.channels);
        let batch_len = batch.len();
        let unwritten = samples.write_all(batch).await;
        written += batch_len - unwritten.len();
        if !unwritten.is_empty() {
            // The kernel stopped taking samples, which is the stream
            // ending under this program rather than a short write.
            return Err(AudioTestError::Truncated {
                written,
                total: total_bytes,
            });
        }
        frame += frames as u64;
    }
    // Dropping the writer is what says the material is over; the future
    // resolves when the kernel has taken the last of it.
    drop(samples);
    taken.finished().await?;
    println!("audio-test:wrote bytes={written} frames={total_frames}");

    playback.stop().await?;
    let summary = reports.recv().await.unwrap_or(Reports {
        latency_bytes: 0,
        xruns: 0,
    });
    println!(
        "audio-test:done latency-bytes={} xruns={}",
        summary.latency_bytes, summary.xruns
    );
    Ok(())
}
