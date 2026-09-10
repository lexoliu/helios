//! `helios:system/audio` for the component host.
//!
//! This is the whole of what a program sees of the machine's sound
//! device, and none of it is on the path a sample takes to the jack. A
//! claim moves one stream's claim word; a negotiation pins the period
//! buffers in the claiming instance's own memory and tells the kernel's
//! playback task to configure the device. From then on the bytes the
//! guest writes are copied straight into those buffers, one period at a
//! time, and the task hands their indices to the device.
//!
//! The claim lives in the instance's store, so it is single-owned: the
//! resource handle a player holds carries the stream id and the claim
//! generation to check against and nothing else. A handle that outlived
//! a release then names a stream its instance no longer holds and is
//! refused, rather than being answered against whoever holds the stream
//! now.
//!
//! # Concurrency contract
//!
//! Every call here runs on the task that owns the store, so the claim
//! and its arena need no lock. What is shared is the period ring, whose
//! two parties are this task and the stream's playback task and whose
//! ownership rule is the free and filled lists, and the request queue
//! into that task, which carries its own synchronisation.

use alloc::vec::Vec;
use core::pin::Pin;
use core::task::{Context, Poll};

use futures::channel::oneshot;
use helios_hal::audio::{SampleFormat, SampleRate, StreamInfo};
use triomphe::Arc;
use wasmtime::StoreContextMut;
use wasmtime::component::{
    Access, Accessor, Destination, FutureReader, HasSelf, Linker, Resource, Source, StreamConsumer,
    StreamProducer, StreamReader, StreamResult, VecBuffer,
};

use crate::ComponentHostNetwork;
use crate::audio::{
    AudioSender, AudioServiceError, Feedback, FeedbackReader, PeriodRing, PeriodWriter,
    PlaybackFormat,
};
use crate::wasmtime_adapter::bindings::audio::bindings::helios::system::audio as audio_wit;

use super::super::StoreData;

/// Register `helios:system/audio` in a linker.
///
/// Every program linker gets it, for the reason the display and input
/// interfaces do: the claim is the capability, not the import. A program
/// that never claims a stream learns nothing from having the import, and
/// one that claims on a machine with no sound device is told
/// `unavailable`.
pub(in crate::wasmtime_adapter::component_host) fn add_audio_to_linker<CpuImpl, Net, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, Net, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    audio_wit::add_to_linker::<_, HasSelf<StoreData<CpuImpl, Net, HostFs>>>(linker, |state| state)
}

/// A player's hold on one playback stream, as its store records it.
///
/// It carries the stream and the generation and nothing else: the claim
/// itself is in the store's [`crate::AudioOwnership`], which is what the
/// store's own drop releases.
pub struct PlaybackHandle {
    stream: u32,
    generation: u64,
}

impl PlaybackHandle {
    pub const fn new(stream: u32, generation: u64) -> Self {
        Self { stream, generation }
    }

    pub const fn stream(&self) -> u32 {
        self.stream
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

const fn to_wit_error(error: AudioServiceError) -> audio_wit::Error {
    match error {
        AudioServiceError::Unavailable => audio_wit::Error::Unavailable,
        AudioServiceError::NoSuchStream => audio_wit::Error::NoSuchStream,
        AudioServiceError::NotPlayback => audio_wit::Error::NotPlayback,
        AudioServiceError::AlreadyClaimed => audio_wit::Error::AlreadyClaimed,
        AudioServiceError::NotClaimed => audio_wit::Error::NotClaimed,
        AudioServiceError::AlreadyNegotiated => audio_wit::Error::AlreadyNegotiated,
        AudioServiceError::NotNegotiated => audio_wit::Error::NotNegotiated,
        AudioServiceError::UnsupportedFormat => audio_wit::Error::UnsupportedFormat,
        // A window with no room left and a machine with no contiguous
        // run left are the same answer to a player: the period buffers
        // this format needs could not be pinned.
        AudioServiceError::OutOfMemory | AudioServiceError::WindowExhausted => {
            audio_wit::Error::OutOfMemory
        }
        // An owner task that has stopped serving is a machine on its way
        // down. There is nothing a player can do about it that it would
        // not also do about a device fault.
        AudioServiceError::DeviceFault | AudioServiceError::Closed => audio_wit::Error::DeviceFault,
    }
}

const fn to_wit_format(format: SampleFormat) -> audio_wit::SampleFormat {
    match format {
        SampleFormat::S8 => audio_wit::SampleFormat::Signed8,
        SampleFormat::U8 => audio_wit::SampleFormat::Unsigned8,
        SampleFormat::S16 => audio_wit::SampleFormat::Signed16,
        SampleFormat::U16 => audio_wit::SampleFormat::Unsigned16,
        SampleFormat::S32 => audio_wit::SampleFormat::Signed32,
        SampleFormat::U32 => audio_wit::SampleFormat::Unsigned32,
        SampleFormat::Float => audio_wit::SampleFormat::Float32,
        SampleFormat::Float64 => audio_wit::SampleFormat::Float64,
    }
}

const fn from_wit_format(format: audio_wit::SampleFormat) -> SampleFormat {
    match format {
        audio_wit::SampleFormat::Signed8 => SampleFormat::S8,
        audio_wit::SampleFormat::Unsigned8 => SampleFormat::U8,
        audio_wit::SampleFormat::Signed16 => SampleFormat::S16,
        audio_wit::SampleFormat::Unsigned16 => SampleFormat::U16,
        audio_wit::SampleFormat::Signed32 => SampleFormat::S32,
        audio_wit::SampleFormat::Unsigned32 => SampleFormat::U32,
        audio_wit::SampleFormat::Float32 => SampleFormat::Float,
        audio_wit::SampleFormat::Float64 => SampleFormat::Float64,
    }
}

/// The rate a caller named, as one of the rates the contract knows.
///
/// A number that names no rate any PCM device is clocked at is refused
/// here rather than rounded to the nearest one: the samples were
/// generated for the rate that was asked for, and playing them at
/// another is a pitch shift nobody asked for.
fn from_wit_rate(hz: u32) -> Option<SampleRate> {
    SampleRate::ALL.into_iter().find(|rate| rate.hz() == hz)
}

fn to_wit_stream_info(info: &StreamInfo) -> audio_wit::StreamInfo {
    audio_wit::StreamInfo {
        id: info.id.index(),
        rates: info.rates.iter().map(SampleRate::hz).collect(),
        formats: info.formats.iter().map(to_wit_format).collect(),
        channels_min: info.channels_min,
        channels_max: info.channels_max,
    }
}

const fn to_wit_feedback(item: Feedback) -> audio_wit::Feedback {
    match item {
        Feedback::LatencyBytes(bytes) => audio_wit::Feedback::LatencyBytes(bytes),
        Feedback::Xrun => audio_wit::Feedback::Xrun,
    }
}

impl<CpuImpl, Net, HostFs> StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    /// The queue into the playback task, checked against the generation
    /// the handle names.
    fn audio_sender(&self, stream: u32, generation: u64) -> Result<AudioSender, AudioServiceError> {
        let claim = self.device.audio().claim_ref()?;
        if claim.id() != stream || claim.generation() != generation {
            return Err(AudioServiceError::NotClaimed);
        }
        Ok(claim.sender())
    }

    /// The period ring of the session this handle names.
    fn audio_ring(
        &self,
        stream: u32,
        generation: u64,
    ) -> Result<Arc<PeriodRing>, AudioServiceError> {
        let audio = self.device.audio();
        let claim = audio.claim_ref()?;
        if claim.id() != stream || claim.generation() != generation {
            return Err(AudioServiceError::NotClaimed);
        }
        audio.ring()
    }
}

impl<CpuImpl, Net, HostFs> audio_wit::Host for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn available(&mut self) -> wasmtime::Result<Vec<audio_wit::StreamInfo>> {
        let Some(service) = self.runtime_state.audio_service() else {
            return Ok(Vec::new());
        };
        Ok(service.available().map(to_wit_stream_info).collect())
    }

    fn claim(
        &mut self,
        stream: u32,
    ) -> wasmtime::Result<Result<Resource<PlaybackHandle>, audio_wit::Error>> {
        let Some(service) = self.runtime_state.audio_service() else {
            return Ok(Err(audio_wit::Error::Unavailable));
        };
        if let Err(error) = self.device.claim_audio(&service, stream) {
            tracing::warn!(
                target: "helios_kernel::audio",
                ?error,
                stream,
                "an instance was refused a playback stream"
            );
            return Ok(Err(to_wit_error(error)));
        }
        let generation = self
            .device
            .audio()
            .claim_ref()
            .expect("a successful claim leaves one")
            .generation();
        let handle = self.table.push(PlaybackHandle::new(stream, generation))?;
        tracing::info!(
            target: "helios_kernel::audio",
            stream,
            generation,
            "an instance took ownership of a playback stream"
        );
        Ok(Ok(handle))
    }
}

impl<CpuImpl, Net, HostFs> audio_wit::HostPlayback for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn info(
        &mut self,
        handle: Resource<PlaybackHandle>,
    ) -> wasmtime::Result<audio_wit::StreamInfo> {
        let handle = self.table.get(&handle)?;
        let (stream, generation) = (handle.stream(), handle.generation());
        let claim = self.device.audio().claim_ref().map_err(as_trap)?;
        if claim.id() != stream || claim.generation() != generation {
            return Err(as_trap(AudioServiceError::NotClaimed));
        }
        Ok(to_wit_stream_info(claim.info()))
    }

    fn release(&mut self, handle: Resource<PlaybackHandle>) -> wasmtime::Result<()> {
        let handle = self.table.get(&handle)?;
        let (stream, generation) = (handle.stream(), handle.generation());
        self.release_playback(stream, generation);
        Ok(())
    }

    fn drop(&mut self, handle: Resource<PlaybackHandle>) -> wasmtime::Result<()> {
        let handle = self.table.delete(handle)?;
        // Dropping the handle is a player saying it is done, and it
        // costs exactly what dying costs: the stream stops, every pinned
        // page goes back to this instance's pool, and only then is the
        // stream offered to anyone else.
        self.release_playback(handle.stream(), handle.generation());
        Ok(())
    }
}

impl<CpuImpl, Net, HostFs> StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn release_playback(&mut self, stream: u32, generation: u64) {
        let held = self
            .device
            .audio()
            .claim_ref()
            .is_ok_and(|claim| claim.id() == stream && claim.generation() == generation);
        if held {
            self.device.audio_mut().release();
        }
    }
}

fn as_trap(error: AudioServiceError) -> wasmtime::Error {
    wasmtime::Error::msg(alloc::format!("{error}"))
}

/// The stream a player reads its claim's feedback from.
struct FeedbackStreamProducer {
    feedback: FeedbackReader,
}

impl Unpin for FeedbackStreamProducer {}

impl<T: 'static> StreamProducer<T> for FeedbackStreamProducer {
    type Item = audio_wit::Feedback;
    type Buffer = VecBuffer<Self::Item>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _store: StoreContextMut<'_, T>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }
        match self.feedback.poll_burst(cx) {
            Poll::Ready(burst) => {
                let items: Vec<Self::Item> = burst.into_iter().map(to_wit_feedback).collect();
                destination.set_buffer(VecBuffer::from(items));
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// The consumer that copies a player's samples into the kernel's period
/// buffers.
///
/// It parks when every period is with the device, which is the whole of
/// the backpressure a playback path has: a writer that is never made to
/// wait is a writer that has run ahead of the sound.
struct SampleStreamConsumer {
    writer: PeriodWriter,
    /// The batch taken off the guest's stream and how much of it has
    /// been copied. Held here — never dropped — until the ring has room
    /// for the rest of it.
    pending: Option<(Vec<u8>, usize)>,
    completion: Option<oneshot::Sender<Result<(), AudioServiceError>>>,
}

impl Unpin for SampleStreamConsumer {}

impl SampleStreamConsumer {
    fn new(
        writer: PeriodWriter,
        completion: oneshot::Sender<Result<(), AudioServiceError>>,
    ) -> Self {
        Self {
            writer,
            pending: None,
            completion: Some(completion),
        }
    }
}

impl Drop for SampleStreamConsumer {
    fn drop(&mut self) {
        // The producer is done — cleanly or because its instance died —
        // so the last part-filled period is committed and the ring is
        // closed. Without that the playback task would wait for a period
        // nobody is going to send.
        self.writer.finish();
        if let Some(reply) = self.completion.take() {
            let _ = reply.send(Ok(()));
        }
    }
}

impl<T: 'static> StreamConsumer<T> for SampleStreamConsumer {
    type Item = u8;

    fn poll_consume(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'_, T>,
        mut source: Source<'_, Self::Item>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if self.pending.is_none() {
            let available = source.remaining(&mut store);
            if available == 0 {
                return Poll::Ready(Ok(StreamResult::Completed));
            }
            let mut bytes = Vec::with_capacity(available);
            source.read(&mut store, &mut bytes)?;
            self.pending = Some((bytes, 0));
        }
        let consumer = &mut *self;
        let (bytes, offset) = consumer
            .pending
            .as_mut()
            .expect("a batch was just taken from the guest's stream");
        while *offset < bytes.len() {
            match consumer.writer.poll_write(cx, &bytes[*offset..]) {
                Poll::Ready(taken) => *offset += taken,
                Poll::Pending => return Poll::Pending,
            }
        }
        consumer.pending = None;
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

impl<CpuImpl, Net, HostFs, U> audio_wit::HostPlaybackWithStore<U>
    for HasSelf<StoreData<CpuImpl, Net, HostFs>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    async fn negotiate(
        accessor: &Accessor<U, Self>,
        handle: Resource<PlaybackHandle>,
        desired: audio_wit::Format,
    ) -> wasmtime::Result<Result<audio_wit::Format, audio_wit::Error>> {
        let Some(rate) = from_wit_rate(desired.rate) else {
            return Ok(Err(audio_wit::Error::UnsupportedFormat));
        };
        let format = PlaybackFormat {
            rate,
            channels: desired.channels,
            format: from_wit_format(desired.sample_format),
        };
        // The store is given back before anything is awaited: an
        // `Access` guard held across a wait would hold the store for as
        // long as the device takes to answer.
        let prepared = accessor.with(|mut access| {
            let data = access.get();
            let named = data.table.get(&handle)?;
            let (stream, generation) = (named.stream(), named.generation());
            let sender = match data.audio_sender(stream, generation) {
                Ok(sender) => sender,
                Err(error) => return Ok::<_, wasmtime::Error>(Err(error)),
            };
            Ok(data
                .device
                .audio_mut()
                .negotiate(format)
                .map(|(ring, params)| (sender, ring, params)))
        })?;
        let (sender, ring, params) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => return Ok(Err(to_wit_error(error))),
        };
        match sender.negotiate(params, ring).await {
            Ok(()) => Ok(Ok(desired)),
            Err(error) => {
                // Nothing reached the device, so the pages go straight
                // back rather than waiting for the claim to end.
                accessor.with(|mut access| access.get().device.audio_mut().discard_negotiation());
                Ok(Err(to_wit_error(error)))
            }
        }
    }

    async fn stop(
        accessor: &Accessor<U, Self>,
        handle: Resource<PlaybackHandle>,
    ) -> wasmtime::Result<Result<(), audio_wit::Error>> {
        let ring = accessor.with(|mut access| {
            let data = access.get();
            let named = data.table.get(&handle)?;
            let (stream, generation) = (named.stream(), named.generation());
            Ok::<_, wasmtime::Error>(data.audio_ring(stream, generation))
        })?;
        let ring = match ring {
            Ok(ring) => ring,
            Err(error) => return Ok(Err(to_wit_error(error))),
        };
        let (reply, answer) = oneshot::channel();
        ring.request_stop(reply);
        match answer.await {
            Ok(outcome) => Ok(outcome.map_err(to_wit_error)),
            // The playback task went away with the machine, which is
            // the one thing that ends this wait early.
            Err(_) => Ok(Err(audio_wit::Error::DeviceFault)),
        }
    }

    fn samples(
        mut access: Access<'_, U, Self>,
        handle: Resource<PlaybackHandle>,
        data: StreamReader<u8>,
    ) -> wasmtime::Result<FutureReader<Result<(), audio_wit::Error>>> {
        let ring = {
            let store = access.get();
            let named = store.table.get(&handle)?;
            let (stream, generation) = (named.stream(), named.generation());
            store.audio_ring(stream, generation)
        };
        let ring = match ring {
            Ok(ring) => ring,
            Err(error) => {
                let error = to_wit_error(error);
                return FutureReader::new(&mut access, async move {
                    Ok::<_, wasmtime::Error>(Err(error))
                });
            }
        };
        let (tx, rx) = oneshot::channel();
        data.pipe(
            &mut access,
            SampleStreamConsumer::new(PeriodWriter::new(ring), tx),
        )?;
        FutureReader::new(&mut access, async move {
            Ok::<_, wasmtime::Error>(match rx.await {
                Ok(outcome) => outcome.map_err(to_wit_error),
                // The consumer was dropped without reporting, which is
                // the guest's own stream ending.
                Err(_) => Ok(()),
            })
        })
    }

    fn feedback(
        mut accessor: Access<'_, U, Self>,
        handle: Resource<PlaybackHandle>,
    ) -> wasmtime::Result<StreamReader<audio_wit::Feedback>> {
        let (stream, generation) = {
            let named = accessor.get().table.get(&handle)?;
            (named.stream(), named.generation())
        };
        // Armed here, before the reader is handed over: an item
        // published between this call and the player's first read is one
        // the player is owed.
        let feedback = {
            let data = accessor.get();
            let claim = data.device.audio().claim_ref().map_err(as_trap)?;
            if claim.id() != stream || claim.generation() != generation {
                return Err(as_trap(AudioServiceError::NotClaimed));
            }
            claim.feedback()
        };
        StreamReader::new(&mut accessor, FeedbackStreamProducer { feedback })
    }
}
