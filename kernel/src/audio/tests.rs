//! What the audio path has to keep true.
//!
//! The device is scripted rather than emulated: what these tests assert
//! is the kernel's own behaviour around it — which formats it refuses,
//! how many periods it lets the device hold, what it does with an
//! underrun, and that a stream comes back when its owner dies.

use alloc::vec::Vec;
use core::future::Future;
use core::pin::pin;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::task::Poll;

use arrayvec::ArrayVec;
use futures::channel::oneshot;
use futures::future::{Either, join, select};
use futures_lite::future::{block_on, poll_once, yield_now};
use helios_hal::audio::{
    AudioError, AudioEvent, AudioResult, ChannelMapList, JackList, PcmParams, PlaybackDevice,
    SampleFormat, SampleFormats, SampleRate, SampleRates, StreamDirection, StreamId, StreamInfo,
    StreamList, XferStatus,
};
use helios_hal::vmm::VirtAddr;
use spin::Mutex;
use triomphe::Arc;

use crate::component::{ProviderReceiver, provider_channel};
use crate::device::{DeviceOwnership, DeviceWindow, LinearMemory, test_hooks};
use crate::exec::Timer;
use crate::test_support::{ManualClockCpu, TestCpu};

use super::owner::{drain_audio_events, play, serve_playback};
use super::service::{
    AudioService, AudioShared, ClaimState, Feedback, PERIODS_IN_FLIGHT, PeriodWriter,
    PlaybackFormat, PlaybackRequest, REQUEST_QUEUE_DEPTH, StreamShared, Written,
};
use super::{AudioOwnership, AudioServiceError};

const RESERVATION_BYTES: u64 = 1 << 32;
const WINDOW_BASE: usize = 0x1_0000_0000;

/// The stream QEMU's virtio-sound device presents: 48 kHz and 44.1 kHz,
/// S16 and S32, stereo only.
fn playback_stream(index: u32) -> StreamInfo {
    StreamInfo {
        id: StreamId::new(index),
        direction: StreamDirection::Playback,
        formats: SampleFormats::new()
            .with(SampleFormat::S16)
            .with(SampleFormat::S32),
        rates: SampleRates::new()
            .with(SampleRate::Hz44100)
            .with(SampleRate::Hz48000),
        channels_min: 2,
        channels_max: 2,
    }
}

fn capture_stream(index: u32) -> StreamInfo {
    StreamInfo {
        direction: StreamDirection::Capture,
        ..playback_stream(index)
    }
}

/// The format `audio-test` plays: 48 kHz, stereo, S16.
const TONE: PlaybackFormat = PlaybackFormat {
    rate: SampleRate::Hz48000,
    channels: 2,
    format: SampleFormat::S16,
};

fn service_of(streams: &[StreamInfo]) -> (AudioService, Vec<ProviderReceiver<PlaybackRequest>>) {
    test_hooks::install();
    let mut playback = ArrayVec::new();
    let mut capture = ArrayVec::new();
    let mut inboxes = Vec::new();
    for info in streams {
        if info.direction != StreamDirection::Playback {
            capture.push(info.id);
            continue;
        }
        let (requests, inbox) = provider_channel(REQUEST_QUEUE_DEPTH);
        playback.push(Arc::new(StreamShared::new(*info, requests)));
        inboxes.push(inbox);
    }
    (
        AudioService::from_shared(Arc::new(AudioShared { playback, capture })),
        inboxes,
    )
}

fn owner() -> DeviceOwnership {
    let mut ownership = DeviceOwnership::new();
    ownership.set_memory(LinearMemory {
        base: VirtAddr::new(WINDOW_BASE),
        reservation_bytes: RESERVATION_BYTES,
    });
    ownership
}

fn window() -> DeviceWindow {
    owner()
        .audio_window()
        .expect("a four-gigabyte reservation carries an audio window")
}

/// A device that answers every configuration step and records the
/// periods it was handed.
struct ScriptedDevice {
    streams: StreamList,
    /// The bytes of every period the device took, in order.
    played: Mutex<Vec<Vec<u8>>>,
    /// How many periods are in the device's hands right now, and the
    /// most it ever held at once.
    in_flight: AtomicU32,
    high_water: AtomicU32,
    /// Set once `start` has been called, so a test can assert that no
    /// period reached the device before the clock ran.
    started: AtomicU32,
    /// Events the drain will be handed, most recent last.
    events: Mutex<Vec<AudioResult<AudioEvent>>>,
    /// How many bytes the device claims to still hold when it takes a
    /// period.
    latency_bytes: u32,
    /// When `Some`, `start` answers this error instead of running.
    start_error: Option<AudioError>,
    /// When `Some`, `write` answers this error on its first poll
    /// instead of recording the period — once it has taken
    /// `writes_before_error` periods, so a refusal can land while the
    /// device is still holding earlier ones.
    write_error: Option<AudioError>,
    /// Periods `write` takes before `write_error` starts answering.
    writes_before_error: u32,
    /// Periods `write` has taken so far.
    writes_taken: AtomicU32,
    /// Writes from this call index on stay in the device's hands until
    /// `release` completes them — the behaviour the spec makes the
    /// device guarantee, which is what the pump's teardown collects.
    holds_after: u32,
    /// When `Some`, `release` answers this error and completes nothing —
    /// a device breaking the one promise the teardown's collection
    /// rests on, leaving whatever it still held held.
    release_error: Option<AudioError>,
    /// Set by `release`; the wake list for the writes it completes.
    released: AtomicBool,
    held_wakers: Mutex<Vec<core::task::Waker>>,
    /// Whether `write` yields once before completing. A device that
    /// answers on its first poll is what the pump's priming has to
    /// keep up with.
    yield_on_write: bool,
}

impl ScriptedDevice {
    fn new(streams: StreamList) -> Self {
        Self {
            streams,
            played: Mutex::new(Vec::new()),
            in_flight: AtomicU32::new(0),
            high_water: AtomicU32::new(0),
            started: AtomicU32::new(0),
            events: Mutex::new(Vec::new()),
            latency_bytes: 0,
            start_error: None,
            write_error: None,
            writes_before_error: 0,
            writes_taken: AtomicU32::new(0),
            holds_after: u32::MAX,
            release_error: None,
            released: AtomicBool::new(false),
            held_wakers: Mutex::new(Vec::new()),
            yield_on_write: true,
        }
    }

    fn with_events(mut self, mut events: Vec<AudioResult<AudioEvent>>) -> Self {
        events.reverse();
        self.events = Mutex::new(events);
        self
    }

    fn played_bytes(&self) -> usize {
        self.played.lock().iter().map(Vec::len).sum()
    }
}

impl PlaybackDevice for ScriptedDevice {
    fn stream_topology(&self) -> &StreamList {
        &self.streams
    }

    async fn jacks(&self) -> AudioResult<JackList> {
        Ok(JackList::new())
    }

    async fn channel_maps(&self) -> AudioResult<ChannelMapList> {
        Ok(ChannelMapList::new())
    }

    async fn set_params(&self, _stream: StreamId, _params: PcmParams) -> AudioResult<()> {
        Ok(())
    }

    async fn prepare(&self, _stream: StreamId) -> AudioResult<()> {
        Ok(())
    }

    async fn start(&self, _stream: StreamId) -> AudioResult<()> {
        self.started.fetch_add(1, Ordering::AcqRel);
        match self.start_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn stop(&self, _stream: StreamId) -> AudioResult<()> {
        Ok(())
    }

    async fn release(&self, _stream: StreamId) -> AudioResult<()> {
        // The promise the spec makes of the device: a release answers
        // only once every message it still holds for the stream has
        // completed, so the held writes end here rather than being
        // left outstanding. A refused release made no such promise —
        // what it still held stays held.
        if let Some(error) = self.release_error {
            return Err(error);
        }
        self.released.store(true, Ordering::Release);
        for waker in self.held_wakers.lock().drain(..) {
            waker.wake();
        }
        Ok(())
    }

    async fn write(&self, _stream: StreamId, period: &[u8]) -> AudioResult<XferStatus> {
        let call = self.writes_taken.load(Ordering::Acquire);
        if let Some(error) = self.write_error
            && call >= self.writes_before_error
        {
            return Err(error);
        }
        self.writes_taken.fetch_add(1, Ordering::AcqRel);
        let held = self.in_flight.fetch_add(1, Ordering::AcqRel) + 1;
        self.high_water.fetch_max(held, Ordering::AcqRel);
        if call >= self.holds_after {
            // Armed before the flag is read a second time, so a release
            // that lands between the look and the park is not slept
            // through.
            core::future::poll_fn(|cx| {
                if self.released.load(Ordering::Acquire) {
                    return Poll::Ready(());
                }
                self.held_wakers.lock().push(cx.waker().clone());
                if self.released.load(Ordering::Acquire) {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
        } else if self.yield_on_write {
            // One yield, so several writes are genuinely outstanding at
            // once rather than each completing before the next is
            // pushed.
            crate::yield_now().await;
        }
        self.played.lock().push(period.to_vec());
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
        Ok(XferStatus {
            latency_bytes: self.latency_bytes,
        })
    }

    async fn next_event(&self) -> AudioResult<AudioEvent> {
        self.events
            .lock()
            .pop()
            .unwrap_or(Err(AudioError::DeviceIo))
    }
}

/// The rates and formats a device published are the whole of what it
/// takes. Answering with the nearest thing it does take would play a
/// caller's samples at a speed it never asked for.
#[test]
fn negotiation_refuses_a_rate_the_device_did_not_list() {
    let (service, _inboxes) = service_of(&[playback_stream(0)]);
    let mut audio = AudioOwnership::new();
    audio
        .claim(&service, 0, window())
        .expect("the stream is free");

    assert_eq!(
        audio
            .negotiate(PlaybackFormat {
                rate: SampleRate::Hz96000,
                ..TONE
            })
            .err(),
        Some(AudioServiceError::UnsupportedFormat)
    );
    assert_eq!(
        audio
            .negotiate(PlaybackFormat {
                format: SampleFormat::Float,
                ..TONE
            })
            .err(),
        Some(AudioServiceError::UnsupportedFormat)
    );
    assert_eq!(
        audio
            .negotiate(PlaybackFormat {
                channels: 6,
                ..TONE
            })
            .err(),
        Some(AudioServiceError::UnsupportedFormat)
    );
    assert_eq!(
        audio.pinned_bytes(),
        0,
        "a refused format pins nothing in the instance's memory"
    );
}

/// The period follows from the format, and the pages behind it come out
/// of the claiming instance's own window.
#[test]
fn a_negotiated_stream_pins_its_periods_in_the_instance_s_window() {
    let (service, _inboxes) = service_of(&[playback_stream(0)]);
    let mut audio = AudioOwnership::new();
    audio
        .claim(&service, 0, window())
        .expect("the stream is free");

    let (ring, params) = audio.negotiate(TONE).expect("the device takes this format");

    // Ten milliseconds of 48 kHz stereo S16.
    assert_eq!(params.period_bytes, 480 * 4);
    assert_eq!(params.buffer_bytes, 480 * 4 * PERIODS_IN_FLIGHT as u32);
    assert_eq!(ring.period_count(), PERIODS_IN_FLIGHT);
    assert_eq!(audio.format(), Some(TONE));
    assert!(
        audio.pinned_bytes() >= u64::from(params.buffer_bytes),
        "every period is pinned in the instance's own memory"
    );
    assert_eq!(
        audio.negotiate(TONE).err(),
        Some(AudioServiceError::AlreadyNegotiated),
        "a stream is negotiated once per claim"
    );
}

/// A second claimant is refused rather than queued, and so is an
/// instance that already holds a stream.
#[test]
fn a_second_claimant_is_refused_the_stream_the_first_holds() {
    let (service, _inboxes) = service_of(&[playback_stream(0), playback_stream(1)]);
    let mut first = AudioOwnership::new();
    first
        .claim(&service, 0, window())
        .expect("the stream is free");

    let mut second = AudioOwnership::new();
    assert_eq!(
        second.claim(&service, 0, window()).err(),
        Some(AudioServiceError::AlreadyClaimed)
    );
    assert_eq!(
        first.claim(&service, 1, window()).err(),
        Some(AudioServiceError::AlreadyClaimed),
        "an instance holds one playback stream at a time"
    );
    assert!(
        second.claim(&service, 1, window()).is_ok(),
        "the other stream is somebody else's to take"
    );
}

/// A stream the device records with is named as such rather than as one
/// that does not exist, and one nothing published is neither.
#[test]
fn a_capture_stream_is_refused_as_a_capture_stream() {
    let (service, _inboxes) = service_of(&[playback_stream(0), capture_stream(1)]);
    let mut audio = AudioOwnership::new();

    assert_eq!(
        audio.claim(&service, 1, window()).err(),
        Some(AudioServiceError::NotPlayback)
    );
    assert_eq!(
        audio.claim(&service, 7, window()).err(),
        Some(AudioServiceError::NoSuchStream)
    );
    assert_eq!(
        service.available().count(),
        1,
        "only the streams that play are offered"
    );
}

/// Dying while holding a stream leaves the claim word in `RELEASING`
/// and the pages with the playback task: the device may still be
/// reading them, so nothing is freed until it has been stopped.
#[test]
fn dying_hands_the_stream_back_through_the_playback_task() {
    let (service, inboxes) = service_of(&[playback_stream(0)]);
    let mut audio = AudioOwnership::new();
    audio
        .claim(&service, 0, window())
        .expect("the stream is free");
    audio.negotiate(TONE).expect("the device takes this format");

    drop(audio);

    let mut other = AudioOwnership::new();
    assert_eq!(
        other.claim(&service, 0, window()).err(),
        Some(AudioServiceError::AlreadyClaimed),
        "the stream is not free until the device has stopped reading its pages"
    );

    let device = ScriptedDevice::new(topology(&[playback_stream(0)]));
    let timer = Timer::new(TestCpu::without_entropy());
    let inbox = &inboxes[0];
    block_on(async {
        // One turn of the task's loop: it takes the release permit,
        // stops the device and drops the arena.
        let served = poll_once(pin!(serve_playback(
            &device,
            stream_of(&service),
            inbox,
            &timer
        )))
        .await;
        assert_eq!(
            served, None,
            "the task keeps serving after it has released a claim"
        );
    });

    assert!(
        other.claim(&service, 0, window()).is_ok(),
        "the stream is free once the device has been stopped and the pages given back"
    );
}

/// The device is never handed more than [`PERIODS_IN_FLIGHT`] periods
/// at once, whatever the producer does, and the clock does not start
/// until a chain has actually been submitted.
#[test]
fn the_device_holds_no_more_than_the_periods_in_flight() {
    test_hooks::install();
    let (service, _inboxes) = service_of(&[playback_stream(0)]);
    let mut audio = AudioOwnership::new();
    audio
        .claim(&service, 0, window())
        .expect("the stream is free");
    let (ring, params) = audio.negotiate(TONE).expect("the device takes this format");

    let device = ScriptedDevice::new(topology(&[playback_stream(0)]));
    let timer = Timer::new(TestCpu::without_entropy());
    let periods = 9_usize;
    let tone = alloc::vec![0x5a_u8; params.period_bytes as usize * periods];

    block_on(async {
        let mut writer = PeriodWriter::new(ring.clone());
        let produce = async {
            let mut offset = 0;
            while offset < tone.len() {
                let taken = write_some(&mut writer, &tone[offset..]).await;
                offset += taken;
            }
            writer.finish();
        };
        futures::future::join(
            produce,
            play(&device, stream_of(&service), &ring, params, &timer),
        )
        .await;
    });

    assert_eq!(
        device.high_water.load(Ordering::Acquire) as usize,
        PERIODS_IN_FLIGHT,
        "the pump keeps the ring full and never overfills it"
    );
    assert_eq!(
        device.started.load(Ordering::Acquire),
        1,
        "the clock starts once, after a chain has been submitted"
    );
    assert_eq!(device.played_bytes(), tone.len());
    assert!(
        device.played.lock().iter().all(|period| {
            period.len() == params.period_bytes as usize && period.iter().all(|byte| *byte == 0x5a)
        }),
        "every period the device took is a whole period of the caller's own samples"
    );
}

/// A producer that stops mid-period has its tail zeroed so that the
/// samples before it can be played at all — and that is the only
/// silence the kernel ever writes.
#[test]
fn a_final_part_period_is_played_with_its_tail_zeroed() {
    test_hooks::install();
    let (service, _inboxes) = service_of(&[playback_stream(0)]);
    let mut audio = AudioOwnership::new();
    audio
        .claim(&service, 0, window())
        .expect("the stream is free");
    let (ring, params) = audio.negotiate(TONE).expect("the device takes this format");

    let device = ScriptedDevice::new(topology(&[playback_stream(0)]));
    let timer = Timer::new(TestCpu::without_entropy());
    let tail = 100_usize;
    let tone = alloc::vec![0x33_u8; params.period_bytes as usize + tail];

    block_on(async {
        let mut writer = PeriodWriter::new(ring.clone());
        let produce = async {
            let mut offset = 0;
            while offset < tone.len() {
                offset += write_some(&mut writer, &tone[offset..]).await;
            }
            writer.finish();
        };
        futures::future::join(
            produce,
            play(&device, stream_of(&service), &ring, params, &timer),
        )
        .await;
    });

    let played = device.played.lock();
    assert_eq!(played.len(), 2);
    assert!(played[1][..tail].iter().all(|byte| *byte == 0x33));
    assert!(
        played[1][tail..].iter().all(|byte| *byte == 0),
        "the tail of the last period is zeroed, and nothing else ever is"
    );
}

/// An underrun the device reports reaches the player as `xrun`. Nothing
/// is played in place of the samples that did not arrive.
#[test]
fn an_underrun_reaches_the_player_as_feedback_and_nothing_is_substituted() {
    let (service, _inboxes) = service_of(&[playback_stream(0)]);
    let mut audio = AudioOwnership::new();
    audio
        .claim(&service, 0, window())
        .expect("the stream is free");
    let claim = audio.claim_ref().expect("the claim was just taken");
    let mut feedback = claim.feedback();

    let device = ScriptedDevice::new(topology(&[playback_stream(0)])).with_events(alloc::vec![
        Ok(AudioEvent::PeriodElapsed(StreamId::new(0))),
        Ok(AudioEvent::Underrun(StreamId::new(0))),
        Err(AudioError::DeviceIo),
    ]);

    block_on(async {
        assert_eq!(
            poll_once(pin!(drain_audio_events(&device, shared_of(&service)))).await,
            Some(()),
            "a faulted device ends the drain instead of spinning on it"
        );
    });

    let burst = block_on(core::future::poll_fn(|cx| feedback.poll_burst(cx)))
        .expect("the stream is still publishing");
    assert_eq!(burst.as_slice(), &[Feedback::Xrun]);
    assert_eq!(
        device.played_bytes(),
        0,
        "an underrun is reported, never filled"
    );
    assert_eq!(service.snapshot()[0].xruns, 1);
}

/// The latency the device reports when it takes a period is what the
/// player is told, one item per completion.
#[test]
fn every_period_the_device_takes_reports_its_latency() {
    test_hooks::install();
    let (service, _inboxes) = service_of(&[playback_stream(0)]);
    let mut audio = AudioOwnership::new();
    audio
        .claim(&service, 0, window())
        .expect("the stream is free");
    let mut feedback = audio
        .claim_ref()
        .expect("the claim was just taken")
        .feedback();
    let (ring, params) = audio.negotiate(TONE).expect("the device takes this format");

    let mut device = ScriptedDevice::new(topology(&[playback_stream(0)]));
    device.latency_bytes = 640;
    // A clock this test moves by hand, because the pump waits out the
    // latency the device reported before it lets the stream be stopped.
    let clock = ManualClockCpu::new();
    let timer = Timer::new(clock.clone());
    let tone = alloc::vec![0x01_u8; params.period_bytes as usize * 2];

    block_on(async {
        let mut writer = PeriodWriter::new(ring.clone());
        let produce = async {
            let mut offset = 0;
            while offset < tone.len() {
                offset += write_some(&mut writer, &tone[offset..]).await;
            }
            writer.finish();
        };
        let played = async {
            futures::future::join(
                produce,
                play(&device, stream_of(&service), &ring, params, &timer),
            )
            .await;
        };
        // The only thing driving this timer is the test, so the clock is
        // stepped beside the work rather than by an executor.
        let clocked = async {
            loop {
                clock.advance(1_000_000);
                timer.fire_expired();
                crate::yield_now().await;
            }
        };
        let played = pin!(played);
        let clocked = pin!(clocked);
        let ended = matches!(
            futures::future::select(played, clocked).await,
            futures::future::Either::Left(_)
        );
        assert!(ended, "the stream ends; the clock does not");
    });

    let burst = block_on(core::future::poll_fn(|cx| feedback.poll_burst(cx)))
        .expect("the stream is still publishing");
    assert_eq!(
        burst.as_slice(),
        &[Feedback::LatencyBytes(640), Feedback::LatencyBytes(640)]
    );
    assert_eq!(service.snapshot()[0].played_bytes, tone.len() as u64);
}

/// An instance that has already grown over the window cannot be given a
/// stream: the pages would land on memory it is using.
#[test]
fn an_instance_that_grew_over_the_window_cannot_be_given_a_stream() {
    let (service, _inboxes) = service_of(&[playback_stream(0)]);
    let mut ownership = owner();
    ownership.note_growth(window().offset() + 1);

    assert_eq!(
        ownership.claim_audio(&service, 0).err(),
        Some(AudioServiceError::WindowExhausted)
    );
}

/// The cap exists so a `memory.grow` cannot land on a period buffer,
/// and while a stream is held the audio window is the lowest of the
/// three an instance can hold.
#[test]
fn holding_a_stream_caps_the_instance_s_growth_at_the_audio_window() {
    let (service, _inboxes) = service_of(&[playback_stream(0)]);
    let mut ownership = owner();
    assert!(ownership.growth_limit().is_none());

    ownership
        .claim_audio(&service, 0)
        .expect("the stream is free");

    assert_eq!(ownership.growth_limit(), Some(window().offset()));
    assert!(ownership.audio().holds_stream());
}

fn topology(streams: &[StreamInfo]) -> StreamList {
    streams.iter().copied().collect()
}

fn shared_of(service: &AudioService) -> &AudioShared {
    service.shared_for_tests()
}

fn stream_of(service: &AudioService) -> &StreamShared {
    &shared_of(service).playback[0]
}

/// A `stop` asked for after the player's own material ran out is
/// answered, rather than left waiting on a task that has already left
/// the stream.
///
/// This is the ordinary end of a player and the order the two sides
/// arrive in is not the player's to choose: dropping the sample writer
/// closes the ring, the playback task stops the device and goes back to
/// waiting for the next claim, and only then does the player get round
/// to asking. Nobody is on the stream to hear it, so the ring answers.
#[test]
fn a_stop_asked_for_after_the_material_ran_out_is_answered() {
    test_hooks::install();
    let (service, inboxes) = service_of(&[playback_stream(0)]);
    let mut audio = AudioOwnership::new();
    audio
        .claim(&service, 0, window())
        .expect("the stream is free");
    let (ring, params) = audio.negotiate(TONE).expect("the device takes this format");
    let sender = audio
        .claim_ref()
        .expect("the claim was just taken")
        .sender();

    let device = ScriptedDevice::new(topology(&[playback_stream(0)]));
    let timer = Timer::new(TestCpu::without_entropy());
    let tone = alloc::vec![0x5a_u8; params.period_bytes as usize * 2];
    let inbox = &inboxes[0];

    let answered = block_on(async {
        let player = async {
            sender
                .negotiate(params, ring.clone())
                .await
                .expect("the device takes this format");
            let mut writer = PeriodWriter::new(ring.clone());
            let mut offset = 0;
            while offset < tone.len() {
                offset += write_some(&mut writer, &tone[offset..]).await;
            }
            // What a guest dropping its sample writer does.
            writer.finish();
            // And then the task runs to the end of its teardown, which
            // is where it is by the time a real player asks: the sample
            // stream ends before `stop` is called, and everything the
            // stop waits for has already happened.
            while !ring.is_finished() {
                yield_now().await;
            }
            let (reply, answer) = oneshot::channel();
            ring.request_stop(reply);
            answer.await
        };
        let served = pin!(serve_playback(&device, stream_of(&service), inbox, &timer));
        match select(pin!(player), served).await {
            Either::Left((answered, _)) => answered,
            Either::Right(((), _)) => {
                panic!("the playback task returned while a player still held the stream")
            }
        }
    });
    assert_eq!(
        answered,
        Ok(Ok(())),
        "a stop that arrives after the teardown is answered by it"
    );
    assert_eq!(
        device.started.load(Ordering::Acquire),
        1,
        "the tone reached the device before any of this"
    );
}

/// A player's feedback ends when the stream it belongs to does.
///
/// Reading the feedback to its close is the only way a player can be
/// sure it saw the last underrun, so a stream that stopped and left its
/// feedback open is a player that never finishes.
#[test]
fn a_players_feedback_ends_when_the_stream_is_torn_down() {
    test_hooks::install();
    let (service, inboxes) = service_of(&[playback_stream(0)]);
    let mut audio = AudioOwnership::new();
    audio
        .claim(&service, 0, window())
        .expect("the stream is free");
    let (ring, params) = audio.negotiate(TONE).expect("the device takes this format");
    let claim = audio.claim_ref().expect("the claim was just taken");
    let sender = claim.sender();
    let mut feedback = claim.feedback();

    let device = ScriptedDevice::new(topology(&[playback_stream(0)]));
    let timer = Timer::new(TestCpu::without_entropy());
    let tone = alloc::vec![0x5a_u8; params.period_bytes as usize * 2];
    let inbox = &inboxes[0];

    let items = block_on(async {
        let player = async {
            sender
                .negotiate(params, ring.clone())
                .await
                .expect("the device takes this format");
            let mut writer = PeriodWriter::new(ring.clone());
            let mut offset = 0;
            while offset < tone.len() {
                offset += write_some(&mut writer, &tone[offset..]).await;
            }
            writer.finish();
            let mut items = 0_usize;
            while let Some(burst) = core::future::poll_fn(|cx| feedback.poll_burst(cx)).await {
                items += burst.len();
            }
            items
        };
        let served = pin!(serve_playback(&device, stream_of(&service), inbox, &timer));
        match select(pin!(player), served).await {
            Either::Left((items, _)) => items,
            Either::Right(((), _)) => {
                panic!("the playback task returned while a player still held the stream")
            }
        }
    });
    assert_eq!(
        items, 2,
        "one item per period the device took, and then the end"
    );
}

/// A feedback reader belongs to the claim that opened it: once that
/// claim is let go, the reader's stream ends, and what the next claim
/// publishes is never the released reader's to see.
///
/// The reader is armed before the release lands, so this is also the
/// check that a wait parked through a hand-off wakes to its own end
/// rather than to somebody else's queue.
#[test]
fn a_released_claims_reader_ends_and_never_sees_the_next_claims_feedback() {
    test_hooks::install();
    let (service, inboxes) = service_of(&[playback_stream(0)]);
    let mut audio = AudioOwnership::new();
    audio
        .claim(&service, 0, window())
        .expect("the stream is free");
    let mut feedback = audio
        .claim_ref()
        .expect("the claim was just taken")
        .feedback();

    block_on(async {
        assert_eq!(
            poll_once(core::future::poll_fn(|cx| feedback.poll_burst(cx))).await,
            None,
            "a live claim's reader parks when nothing is queued"
        );
    });

    audio.release();

    let device = ScriptedDevice::new(topology(&[playback_stream(0)]));
    let timer = Timer::new(TestCpu::without_entropy());
    let inbox = &inboxes[0];
    block_on(async {
        // One turn of the task's loop takes the release: the claim word
        // goes free and the feedback is closed under the armed reader.
        let served = poll_once(pin!(serve_playback(
            &device,
            stream_of(&service),
            inbox,
            &timer
        )))
        .await;
        assert_eq!(
            served, None,
            "the task keeps serving after it has released a claim"
        );
    });

    let mut second = AudioOwnership::new();
    second
        .claim(&service, 0, window())
        .expect("the released stream is free to take");
    let mut fresh = second
        .claim_ref()
        .expect("the claim was just taken")
        .feedback();
    stream_of(&service).publish(Feedback::Xrun);

    block_on(async {
        assert_eq!(
            poll_once(core::future::poll_fn(|cx| feedback.poll_burst(cx))).await,
            Some(None),
            "the released claim's reader ended rather than reading on"
        );
        let burst = core::future::poll_fn(|cx| fresh.poll_burst(cx))
            .await
            .expect("the holding claim's reader still reads");
        assert_eq!(burst.as_slice(), &[Feedback::Xrun]);
    });
}

/// A `stop` asked for while a player is still writing ends that
/// player's stream.
///
/// The alternative is worse than a truncated write: stopping the stream
/// is what makes the playback task leave, so nothing is ever going to
/// give the producer another period, and a producer that parked waiting
/// for one would park for good.
#[test]
fn a_stop_mid_stream_ends_the_producer_rather_than_parking_it() {
    test_hooks::install();
    let (service, _inboxes) = service_of(&[playback_stream(0)]);
    let mut audio = AudioOwnership::new();
    audio
        .claim(&service, 0, window())
        .expect("the stream is free");
    let (ring, params) = audio.negotiate(TONE).expect("the device takes this format");
    let mut writer = PeriodWriter::new(ring.clone());
    let period = alloc::vec![0x11_u8; params.period_bytes as usize];

    block_on(async {
        // Nothing is pumping the ring, so filling it is what puts the
        // producer where a stop can strand it.
        for _ in 0..PERIODS_IN_FLIGHT {
            assert_eq!(write_some(&mut writer, &period).await, period.len());
        }
        assert_eq!(
            poll_once(pin!(write_once(&mut writer, &period))).await,
            None,
            "a ring whose every period is with the device parks its producer"
        );

        let (reply, answer) = oneshot::channel();
        ring.request_stop(reply);
        assert_eq!(
            poll_once(pin!(write_once(&mut writer, &period))).await,
            Some(Written::Ended),
            "a stopped ring ends its producer instead of parking it again"
        );
        // Nobody is serving the stream, so the answer is still owed; the
        // task that stops the device is what sends it.
        drop(answer);
    });
}

/// One `poll_write`, whatever it comes to.
fn write_once<'a>(
    writer: &'a mut PeriodWriter,
    bytes: &'a [u8],
) -> impl Future<Output = Written> + 'a {
    core::future::poll_fn(move |cx| writer.poll_write(cx, bytes))
}

/// One `poll_write` that has to make progress, which is what the
/// producers above want: the ring is four periods deep and the pump is
/// running beside them.
async fn write_some(writer: &mut PeriodWriter, bytes: &[u8]) -> usize {
    match write_once(writer, bytes).await {
        Written::Took(taken) => taken,
        Written::Ended => panic!("the ring ended under a producer that still had material"),
    }
}

/// The claim word a released claim leaves behind, so a test can say
/// which of the three states the stream is in.
#[test]
fn a_released_claim_frees_its_word_only_after_the_device_is_stopped() {
    let (service, _inboxes) = service_of(&[playback_stream(0)]);
    let mut audio = AudioOwnership::new();
    audio
        .claim(&service, 0, window())
        .expect("the stream is free");

    audio.release();

    assert_eq!(
        stream_of(&service).claim.load(Ordering::Acquire),
        ClaimState::RELEASING,
        "the word says the stream is neither held nor free"
    );
}

/// A pump that dies before the clock starts still ends the producer,
/// and it takes back every chain it already handed the device.
///
/// The device refused `start`, so no period is ever coming back on its
/// own; a producer left parked on one would wait for ever. Every exit
/// of the pump closes the ring, which is what the producer's next write
/// sees, and the release it then asks for is what makes the device
/// complete the writes it is still holding — a pump that left without
/// collecting them would have left the device holding periods for ever.
#[test]
fn a_device_that_refuses_to_start_ends_its_producer() {
    test_hooks::install();
    let (service, _inboxes) = service_of(&[playback_stream(0)]);
    let mut audio = AudioOwnership::new();
    audio
        .claim(&service, 0, window())
        .expect("the stream is free");
    let (ring, params) = audio.negotiate(TONE).expect("the device takes this format");

    let mut device = ScriptedDevice::new(topology(&[playback_stream(0)]));
    device.start_error = Some(AudioError::DeviceIo);
    // The primed writes stay in the device's hands until the release
    // the teardown asks for completes them, which is the real device's
    // promise — the `in_flight` count below is the leak this would
    // leave if the pump dropped them instead.
    device.holds_after = 0;
    let timer = Timer::new(TestCpu::without_entropy());
    let period = alloc::vec![0x11_u8; params.period_bytes as usize];

    let outcome = block_on(async {
        let player = async {
            let mut writer = PeriodWriter::new(ring.clone());
            // Fill the ring, so the next write has to wait on a period
            // the dead pump will never hand back.
            for _ in 0..PERIODS_IN_FLIGHT {
                assert_eq!(write_some(&mut writer, &period).await, period.len());
            }
            write_once(&mut writer, &period).await
        };
        let pump = pin!(play(&device, stream_of(&service), &ring, params, &timer));
        join(pin!(player), pump).await.0
    });
    assert_eq!(
        outcome,
        Written::Ended,
        "the producer is ended, not parked, when the pump dies"
    );
    assert_eq!(
        device.in_flight.load(Ordering::Acquire),
        0,
        "every chain the pump submitted was collected before it left"
    );
}

/// A period the device refuses kills the stream the same way: the pump
/// closes the ring as it leaves, the producer's next write ends rather
/// than parking on a reclaim that cannot come — and the chains already
/// submitted are collected rather than left with the device.
///
/// The refusal is synchronous — the write answers on the first poll,
/// inside the pump's priming pass — so this is also the check that a
/// first-poll completion takes the same path as every other. The first
/// period is taken before the refusals start, which is what leaves a
/// chain outstanding when the stream ends.
#[test]
fn a_rejected_period_ends_the_stream_and_its_producer() {
    test_hooks::install();
    let (service, _inboxes) = service_of(&[playback_stream(0)]);
    let mut audio = AudioOwnership::new();
    audio
        .claim(&service, 0, window())
        .expect("the stream is free");
    let (ring, params) = audio.negotiate(TONE).expect("the device takes this format");

    let mut device = ScriptedDevice::new(topology(&[playback_stream(0)]));
    // The first write is taken and held — the release in the teardown
    // is what completes it — so the refusal lands with a chain
    // genuinely outstanding.
    device.holds_after = 0;
    device.writes_before_error = 1;
    device.write_error = Some(AudioError::PeriodLength {
        stream: StreamId::new(0),
        period_bytes: params.period_bytes,
        actual: params.period_bytes as usize + 1,
    });
    let timer = Timer::new(TestCpu::without_entropy());
    let period = alloc::vec![0x22_u8; params.period_bytes as usize];

    let outcome = block_on(async {
        let player = async {
            let mut writer = PeriodWriter::new(ring.clone());
            for _ in 0..PERIODS_IN_FLIGHT {
                assert_eq!(write_some(&mut writer, &period).await, period.len());
            }
            write_once(&mut writer, &period).await
        };
        let pump = pin!(play(&device, stream_of(&service), &ring, params, &timer));
        join(pin!(player), pump).await.0
    });
    assert_eq!(
        outcome,
        Written::Ended,
        "a refused period ends the producer rather than losing the refusal"
    );
    assert_eq!(
        device.started.load(Ordering::Acquire),
        1,
        "the clock was asked to start before the refusal settled"
    );
    assert_eq!(
        device.in_flight.load(Ordering::Acquire),
        0,
        "the chain the device was still holding came back with the rest"
    );
}

/// The same refusal arriving mid-stream — after the clock has started
/// and with other chains still in the device's hands — ends the pump
/// the same way, and the outstanding chains are collected before it
/// leaves rather than dropped on the device.
#[test]
fn a_rejected_period_mid_stream_collects_the_rest_of_the_ring() {
    test_hooks::install();
    let (service, _inboxes) = service_of(&[playback_stream(0)]);
    let mut audio = AudioOwnership::new();
    audio
        .claim(&service, 0, window())
        .expect("the stream is free");
    let (ring, params) = audio.negotiate(TONE).expect("the device takes this format");

    let mut device = ScriptedDevice::new(topology(&[playback_stream(0)]));
    // The first write plays out so its reclaim lets the producer commit
    // the period the refusal arrives on; the rest stay in the device's
    // hands until the release completes them — a mid-stream refusal
    // with three chains genuinely outstanding.
    device.holds_after = 1;
    device.writes_before_error = PERIODS_IN_FLIGHT as u32;
    device.write_error = Some(AudioError::DeviceIo);
    let timer = Timer::new(TestCpu::without_entropy());
    let period = alloc::vec![0x22_u8; params.period_bytes as usize];

    let periods_written = block_on(async {
        let player = async {
            let mut writer = PeriodWriter::new(ring.clone());
            // The write that lands between a reclaim and the refusal is
            // still taken; what the pump owes the producer is an end,
            // not a refusal of its own.
            let mut written = 0_usize;
            while let Written::Took(taken) = write_once(&mut writer, &period).await {
                assert_eq!(taken, period.len());
                written += 1;
            }
            written
        };
        let pump = pin!(play(&device, stream_of(&service), &ring, params, &timer));
        join(pin!(player), pump).await.0
    });
    assert!(
        periods_written > PERIODS_IN_FLIGHT,
        "the refusal arrived mid-stream, past the ring's first fill"
    );
    assert_eq!(
        device.in_flight.load(Ordering::Acquire),
        0,
        "the chains already with the device were collected, not left behind"
    );
}

/// A device that refuses `release` does not wedge the stream.
///
/// The teardown's collection of outstanding chains rests on the release
/// having completed them, so a release the device would not take is the
/// one time the pump leaves without them: waiting could park for ever,
/// which is the wedge this whole path exists to avoid. What it owes
/// instead is the error line — stream, step, the device's refusal, and
/// how many chains it left the device holding — and the claim's word,
/// which frees even so.
#[test]
fn a_device_that_refuses_to_release_keeps_its_chains_but_frees_the_claim() {
    test_hooks::install();
    let (service, inboxes) = service_of(&[playback_stream(0)]);
    let mut audio = AudioOwnership::new();
    audio
        .claim(&service, 0, window())
        .expect("the stream is free");
    let (ring, params) = audio.negotiate(TONE).expect("the device takes this format");

    let mut device = ScriptedDevice::new(topology(&[playback_stream(0)]));
    // Two writes stay in the device's hands and the third is refused,
    // which is what ends the pump while the held two are outstanding —
    // and the release refuses too, so their completions never come.
    device.holds_after = 0;
    device.writes_before_error = 2;
    device.write_error = Some(AudioError::DeviceIo);
    device.release_error = Some(AudioError::DeviceIo);
    let timer = Timer::new(TestCpu::without_entropy());
    let period = alloc::vec![0x22_u8; params.period_bytes as usize];

    let periods_written = block_on(async {
        let player = async {
            let mut writer = PeriodWriter::new(ring.clone());
            let mut written = 0_usize;
            while let Written::Took(taken) = write_once(&mut writer, &period).await {
                assert_eq!(taken, period.len());
                written += 1;
            }
            written
        };
        let pump = pin!(play(&device, stream_of(&service), &ring, params, &timer));
        join(pin!(player), pump).await.0
    });
    // The writer loop only leaves on `Ended`: the pump died with two
    // chains in the device's hands and still ended its producer.
    assert!(
        periods_written >= 3,
        "the producer committed past the two the device still holds"
    );
    assert_eq!(
        device.in_flight.load(Ordering::Acquire),
        2,
        "the two chains the device never completed stayed with it — the count the teardown logged"
    );

    audio.release();
    let inbox = &inboxes[0];
    block_on(async {
        let served = poll_once(pin!(serve_playback(
            &device,
            stream_of(&service),
            inbox,
            &timer
        )))
        .await;
        assert_eq!(
            served, None,
            "the task keeps serving after a release the device refused"
        );
    });

    assert_eq!(
        stream_of(&service).claim.load(Ordering::Acquire),
        ClaimState::FREE,
        "the claim word freed even though the device refused to release"
    );
    let mut second = AudioOwnership::new();
    second
        .claim(&service, 0, window())
        .expect("a refused release did not wedge the stream");
}

/// A `write` that answers on its first poll — a device that completes
/// without an interrupt — is a completion like any other: the period is
/// reclaimed, its bytes counted, its latency published.
///
/// The priming poll is the only place such an answer could be lost, and
/// losing it would drop the period's index with it: the free list would
/// shrink by one and the player's accounting would be a period short.
#[test]
fn a_write_that_completes_on_its_first_poll_is_settled_the_same() {
    test_hooks::install();
    let (service, _inboxes) = service_of(&[playback_stream(0)]);
    let mut audio = AudioOwnership::new();
    audio
        .claim(&service, 0, window())
        .expect("the stream is free");
    let (ring, params) = audio.negotiate(TONE).expect("the device takes this format");

    let mut device = ScriptedDevice::new(topology(&[playback_stream(0)]));
    device.yield_on_write = false;
    let timer = Timer::new(TestCpu::without_entropy());
    let periods = 9;
    let tone = alloc::vec![0x5a_u8; params.period_bytes as usize * periods];

    let written = block_on(async {
        let produce = async {
            let mut writer = PeriodWriter::new(ring.clone());
            let mut offset = 0;
            while offset < tone.len() {
                offset += write_some(&mut writer, &tone[offset..]).await;
            }
            writer.finish();
            writer.accepted()
        };
        let pump = pin!(play(&device, stream_of(&service), &ring, params, &timer));
        join(pin!(produce), pump).await.0
    });
    assert_eq!(written, tone.len() as u64);
    assert_eq!(
        device.played_bytes(),
        tone.len(),
        "every period reached the device"
    );
    assert_eq!(
        stream_of(&service).played_bytes.load(Ordering::Acquire),
        tone.len() as u64,
        "every completion was settled, including the ones the priming poll found"
    );
    assert_eq!(
        device.started.load(Ordering::Acquire),
        1,
        "the clock started once, on the primed chains"
    );
}

/// A closed ring takes nothing, even while its free list still shows
/// periods: the pump that would have played them is gone, so handing
/// one out would only pretend the bytes were going somewhere.
#[test]
fn a_closed_ring_takes_no_more_bytes() {
    test_hooks::install();
    let (service, _inboxes) = service_of(&[playback_stream(0)]);
    let mut audio = AudioOwnership::new();
    audio
        .claim(&service, 0, window())
        .expect("the stream is free");
    let (ring, params) = audio.negotiate(TONE).expect("the device takes this format");
    let mut writer = PeriodWriter::new(ring.clone());
    let period = alloc::vec![0x33_u8; params.period_bytes as usize];

    ring.close();
    block_on(async {
        assert_eq!(
            write_once(&mut writer, &period).await,
            Written::Ended,
            "a free period on a closed ring is not the producer's to take"
        );
    });
}

/// The cursor a producer is handed writes into the pinned period
/// itself: the bytes the device is later handed are the bytes the
/// cursor wrote, at the same address — there is no staging copy.
#[test]
fn a_period_cursor_writes_into_the_pinned_period_itself() {
    test_hooks::install();
    let (service, _inboxes) = service_of(&[playback_stream(0)]);
    let mut audio = AudioOwnership::new();
    audio
        .claim(&service, 0, window())
        .expect("the stream is free");
    let (ring, params) = audio.negotiate(TONE).expect("the device takes this format");

    block_on(async {
        let mut writer = PeriodWriter::new(ring.clone());

        // A cursor returned short of full leaves the period open; the
        // next one resumes it where it left off.
        let mut cursor = core::future::poll_fn(|cx| writer.poll_cursor(cx))
            .await
            .expect("the free list has a period");
        let head = alloc::vec![0xa5_u8; 64];
        cursor.fill(&head);
        writer.return_cursor(cursor);

        let mut cursor = core::future::poll_fn(|cx| writer.poll_cursor(cx))
            .await
            .expect("the same period is handed back out");
        assert_eq!(
            cursor.room(),
            params.period_bytes as usize - head.len(),
            "the resumed period keeps what the first cursor wrote"
        );
        let tail = cursor.tail().as_ptr() as usize;
        let body = alloc::vec![0x5a_u8; cursor.room()];
        cursor.fill(&body);
        writer.return_cursor(cursor);

        let index = ring.take_filled().expect("a full period was committed");
        // SAFETY: the index came off the filled list, which is this
        // test's side of the ring's ownership split.
        let period = unsafe { ring.period(index) };
        assert_eq!(
            period[head.len()..].as_ptr() as usize,
            tail,
            "the cursor's buffer is the pinned period's own memory"
        );
        assert!(
            period[..head.len()].iter().all(|byte| *byte == 0xa5)
                && period[head.len()..].iter().all(|byte| *byte == 0x5a),
            "the bytes the cursors wrote are the bytes the device is handed"
        );
    });
}
