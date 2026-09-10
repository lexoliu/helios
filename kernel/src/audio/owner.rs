//! The kernel's ownership of the machine's sound device.
//!
//! A sound device reports without being asked — a period elapsed, an
//! underrun, a plug pulled out of a jack — and a device nobody reads is
//! a device that stops reporting: its event ring is its whole buffer
//! pool, so once the guest has left every buffer full the host has
//! nowhere to put the next announcement, and on a transport whose
//! interrupt line is a function of a read-to-clear status register the
//! line never falls again either. The tasks here own the device and keep
//! its rings moving for as long as the machine runs, whether or not
//! anything is playing.
//!
//! What is *played* is decided by whichever instance holds a stream's
//! claim. It never touches the device: it fills the period buffers the
//! kernel pinned in its own memory and hands their indices over, and the
//! tasks here are the only code that speaks to the device.
//!
//! # SMP contract
//!
//! One task for the device's event ring and one per playback stream,
//! all local to the processor that brought the device up, because that
//! is the processor the device's interrupt is routed to.
//!
//! * The event drain parks on the device's own notification and never
//!   polls. It is the device's single event reader — the playback
//!   contract's own rule — and it never waits on anything a guest
//!   controls: an underrun it cannot deliver is dropped and counted,
//!   because a drain that waits is a ring that stops.
//! * A stream's playback task is the only thing that submits to the
//!   device's transmit queue for that stream, and it keeps
//!   [`PERIODS_IN_FLIGHT`] chains there whenever the producer has
//!   supplied them. It is never cancelled while a chain is outstanding,
//!   and it never leaves one outstanding either: every exit — a refused
//!   start, a rejected period — releases the stream first, which is the
//!   request that makes the device complete every message it still
//!   holds, and the task collects each chain it submitted before it
//!   returns. A `write` future dropped between its submission and its
//!   completion would leave a descriptor in the device's ring that
//!   nobody reaps, which is both why a stop closes the producer's ring
//!   rather than interrupting the task and why the pump cannot simply
//!   return on an error. The one time chains are left behind is a
//!   release the device refused: that answer never carried the
//!   completion promise the collection rests on, so waiting on them
//!   could park for ever — they are logged by count instead, the
//!   device's own ring being the device's to account for.
//!
//! Both hold the same device handle. The trait's own contract says every
//! method takes `&self`, may be called from several tasks at once, and
//! that an implementation serialises access to its own rings.

use core::future::Future;
use core::pin::{Pin, pin};
use core::task::Poll;
use core::time::Duration;

use arrayvec::ArrayVec;
use core::sync::atomic::Ordering;
use futures::StreamExt;
use futures::future::{Either, select};
use helios_hal::audio::{
    AudioError, AudioEvent, AudioResult, MAX_STREAMS, PcmParams, PlaybackDevice, StreamDirection,
    StreamId, XferStatus,
};
use helios_hal::cpu::Cpu;
use helios_hal::watchdog::Watchdog;
use triomphe::Arc;

use crate::Kernel;
use crate::component::{ProviderReceiver, provider_channel};
use crate::exec::{InFlight, Timer};

use super::AudioServiceError;
use super::service::{
    AudioService, AudioShared, ClaimState, Feedback, PERIODS_IN_FLIGHT, PeriodRing,
    PlaybackRequest, REQUEST_QUEUE_DEPTH, StreamShared,
};

/// Brings the machine's sound device under kernel ownership and
/// publishes the service `helios:system/audio` is served from.
///
/// The device is never handed anywhere else. What callers get back is a
/// handle to the tasks this spawns, which is what a claim and every
/// period after it travels through.
pub fn install_audio_device<CpuImpl, WatchdogImpl, Device>(
    kernel: &Kernel<CpuImpl, WatchdogImpl>,
    device: Device,
) -> AudioService
where
    CpuImpl: Cpu + Clone + Send + Sync + 'static,
    WatchdogImpl: Watchdog + Clone,
    Device: PlaybackDevice + Clone,
{
    let mut playback = ArrayVec::new();
    let mut capture = ArrayVec::new();
    let mut inboxes = ArrayVec::<ProviderReceiver<PlaybackRequest>, MAX_STREAMS>::new();
    for info in device.stream_topology() {
        if info.direction != StreamDirection::Playback {
            capture.push(info.id);
            continue;
        }
        let (requests, inbox) = provider_channel(REQUEST_QUEUE_DEPTH);
        playback.push(Arc::new(StreamShared::new(*info, requests)));
        inboxes.push(inbox);
    }
    let shared = Arc::new(AudioShared { playback, capture });

    for (stream, inbox) in shared.playback.iter().cloned().zip(inboxes) {
        let device = device.clone();
        let timer = kernel.timer();
        kernel.spawn_local_detached(async move {
            serve_playback(&device, &stream, &inbox, &timer).await;
        });
    }
    {
        let shared = shared.clone();
        kernel.spawn_local_detached(async move {
            drain_audio_events(&device, &shared).await;
        });
    }

    tracing::info!(
        playback = shared.playback.len(),
        capture = shared.capture.len(),
        "audio service online"
    );
    AudioService::from_shared(shared)
}

/// Keeps the device's event ring moving, for as long as the machine
/// runs.
///
/// Every announcement is logged rather than counted, because each of
/// them is rare and each of them means something a person debugging
/// audio wants to see. A period elapsing is the one frequent event, and
/// it goes to the trace level so a running stream does not fill the
/// console. An underrun goes further: it is the one thing the player
/// itself has to be told, so it is relayed onto that stream's feedback
/// as well.
pub(super) async fn drain_audio_events<Device: PlaybackDevice>(
    device: &Device,
    shared: &AudioShared,
) {
    loop {
        let event = match device.next_event().await {
            Ok(event) => event,
            Err(error) => {
                // The device and its ring disagree about what is in it.
                // Asking again would fail the same way without ever
                // parking, so the drain stops here and says so; whoever
                // is playing sees a device that reports nothing more.
                tracing::error!(
                    target: "helios_kernel::audio",
                    %error,
                    "sound device faulted; its events are no longer being read"
                );
                return;
            }
        };
        match event {
            AudioEvent::PeriodElapsed(stream) => tracing::trace!(
                target: "helios_kernel::audio",
                stream = stream.index(),
                "sound period elapsed"
            ),
            AudioEvent::Underrun(stream) => {
                if let Some(shared) = stream_of(shared, stream) {
                    shared.xruns.fetch_add(1, Ordering::AcqRel);
                    shared.publish(Feedback::Xrun);
                }
                // Nothing is substituted for the samples that did not
                // arrive: a player that wants silence sends silence, and
                // one that does not is being told here.
                tracing::warn!(
                    target: "helios_kernel::audio",
                    stream = stream.index(),
                    "sound stream underran; the jack played a gap"
                );
            }
            AudioEvent::JackConnected(jack) => tracing::info!(
                target: "helios_kernel::audio",
                jack = jack.index(),
                "sound jack connected"
            ),
            AudioEvent::JackDisconnected(jack) => tracing::info!(
                target: "helios_kernel::audio",
                jack = jack.index(),
                "sound jack disconnected"
            ),
        }
    }
}

fn stream_of(shared: &AudioShared, id: StreamId) -> Option<&Arc<StreamShared>> {
    shared.playback.iter().find(|stream| stream.id() == id)
}

/// Serves one playback stream for as long as the machine runs.
///
/// Two states and nothing else: idle, where a release or a negotiation
/// is awaited, and playing, where the pump runs to the end of the
/// producer's ring and is not interrupted.
pub(super) async fn serve_playback<Device, CpuImpl>(
    device: &Device,
    shared: &StreamShared,
    inbox: &ProviderReceiver<PlaybackRequest>,
    timer: &Timer<CpuImpl>,
) where
    Device: PlaybackDevice,
    CpuImpl: Cpu + Clone,
{
    loop {
        // The release is a banked permit rather than a message, and it
        // is polled first: a claim that has been let go is torn down
        // before anything queued under it is served, and the generation
        // check below drops whatever was left in the queue.
        let released = pin!(shared.release.notified());
        let next = pin!(inbox.recv());
        match select(released, next).await {
            Either::Left(((), _)) => {
                finish_release(device, shared).await;
            }
            Either::Right((Some(request), _)) => {
                serve_negotiation(device, shared, request, timer).await;
            }
            Either::Right((None, _)) => return,
        }
    }
}

/// Hand everything a released claim held back to the machine.
///
/// The order is what makes it safe to release the caller's pages
/// afterwards: the clock stops, the device gives up what it allocated
/// for the stream, and only then is the arena dropped. A device still
/// latching a period whose pages had gone back to a pool would play
/// another instance's memory.
async fn finish_release<Device: PlaybackDevice>(device: &Device, shared: &StreamShared) {
    if let Err(error) = quiesce(device, shared).await {
        // The claim is going back either way — the device is told, its
        // refusal is reported, and nothing is parked on it.
        tracing::error!(
            target: "helios_kernel::audio",
            %error,
            stream = shared.id().index(),
            "the sound device would not release a stream whose claim is going back"
        );
    }
    while let Ok(pins) = shared.returned.pop() {
        drop(pins);
    }
    shared.drain_feedback();
    // A claim let go without ever playing still owes its reader an end.
    shared.close_feedback();
    shared.claim.store(ClaimState::FREE, Ordering::Release);
    tracing::info!(
        target: "helios_kernel::audio",
        stream = shared.id().index(),
        "playback claim released"
    );
}

/// Stop the stream's clock and let the device release what it
/// allocated, answering whether the release went through.
///
/// A stream that was never configured refuses both steps, which is not
/// an error here — the claim is going back either way — but a refused
/// step is reported rather than walked past: `stop`'s refusal is logged
/// and `release` is still asked for, because it is the one request a
/// device may only answer after completing every write it still holds.
/// Its answer is therefore the caller's to act on: an `Err` means no
/// such completion was promised, and whatever the device is still
/// holding may never come back.
async fn quiesce<Device: PlaybackDevice>(
    device: &Device,
    shared: &StreamShared,
) -> AudioResult<()> {
    let stream = shared.id();
    if let Err(error) = device.stop(stream).await {
        tracing::error!(
            target: "helios_kernel::audio",
            %error,
            stream = stream.index(),
            "the sound device would not stop a stream that is going back"
        );
    }
    device.release(stream).await
}

async fn serve_negotiation<Device, CpuImpl>(
    device: &Device,
    shared: &StreamShared,
    request: PlaybackRequest,
    timer: &Timer<CpuImpl>,
) where
    Device: PlaybackDevice,
    CpuImpl: Cpu + Clone,
{
    let PlaybackRequest::Negotiate {
        generation,
        params,
        ring,
        reply,
    } = request;
    if generation != shared.generation.load(Ordering::Acquire) {
        // A request that outlived its claim. Nothing is answered: the
        // instance that asked is gone, so the reply channel's other end
        // is gone with it, and serving it against whoever holds the
        // stream now would let a dead player reconfigure a live one's
        // sound.
        return;
    }
    match configure(device, shared, params).await {
        Ok(()) => {
            // The producer may start committing the moment this reply
            // lands, and the pump below is what takes those periods, so
            // the reply goes first.
            let _ = reply.send(Ok(()));
        }
        Err(error) => {
            let _ = reply.send(Err(error));
            return;
        }
    }
    // The pump leaves with the stream stopped and released and every
    // chain it submitted collected, which is what a `stop` waits for. A
    // player whose material simply ran out reaches this before it gets
    // round to asking, so the answer is left on the ring rather than
    // handed to whoever is waiting now.
    play(device, shared, &ring, params, timer).await;
    ring.finish_teardown();
    // Nothing will publish another period or another underrun for this
    // stream, so its feedback ends here. A player reads it to the close
    // to be sure it saw the last of it, and a close that never came
    // would be a player that never finished.
    shared.close_feedback();
}

async fn configure<Device: PlaybackDevice>(
    device: &Device,
    shared: &StreamShared,
    params: PcmParams,
) -> Result<(), AudioServiceError> {
    let stream = shared.id();
    device
        .set_params(stream, params)
        .await
        .map_err(|error| refused(stream, "configure", error))?;
    device
        .prepare(stream)
        .await
        .map_err(|error| refused(stream, "prepare", error))?;
    tracing::info!(
        target: "helios_kernel::audio",
        stream = stream.index(),
        rate = params.rate.hz(),
        channels = params.channels,
        format = params.format.name(),
        period_bytes = params.period_bytes,
        periods = PERIODS_IN_FLIGHT,
        "playback stream negotiated"
    );
    Ok(())
}

fn refused(stream: StreamId, step: &'static str, error: AudioError) -> AudioServiceError {
    tracing::warn!(
        target: "helios_kernel::audio",
        %error,
        stream = stream.index(),
        step,
        "the sound device refused a playback stream"
    );
    match error {
        AudioError::InvalidParams(_) | AudioError::NotSupported => {
            AudioServiceError::UnsupportedFormat
        }
        AudioError::UnknownStream(_) => AudioServiceError::NoSuchStream,
        AudioError::NotPlayback(_) => AudioServiceError::NotPlayback,
        _ => AudioServiceError::DeviceFault,
    }
}

/// Keeps the device's transmit ring fed until the producer's ring ends,
/// then leaves the device the way the task found it.
///
/// Every period the producer commits is handed to the device as its own
/// chain, up to [`PERIODS_IN_FLIGHT`] of them at once, and each one goes
/// back on the free list when the device is done with it. The clock is
/// started only once a chain has actually been submitted: a device told
/// to play with nothing queued reports an underrun of the kernel's own
/// making, and an underrun this path reports has to be the player's.
///
/// Every exit — the ring played out, a refused start, a rejected
/// period — runs the same teardown: the clock stops, the device
/// releases the stream, and each chain the pump submitted is collected.
/// The order is what makes collecting them safe: the release is the one
/// request the device must answer only after completing every I/O
/// message it still holds, so the completions of the pump's outstanding
/// writes are in its used ring by then, and settling each of them keeps
/// a `write` future from ever being dropped between its submission and
/// its completion — which is the move that would leave a descriptor in
/// the device's ring that nobody reaps. A release the device refused
/// carried none of that promise, so the chains are left with it —
/// counted in the error line, because the device that broke the
/// contract owns what its ring still holds — rather than parked on
/// completions that may never come.
pub(super) async fn play<Device, CpuImpl>(
    device: &Device,
    shared: &StreamShared,
    ring: &PeriodRing,
    params: PcmParams,
    timer: &Timer<CpuImpl>,
) where
    Device: PlaybackDevice,
    CpuImpl: Cpu + Clone,
{
    let stream = shared.id();
    // A fixed set of write chains: the ring is `PERIODS_IN_FLIGHT` deep
    // and every `write` here is the same concrete future, so the set is
    // an array rather than an allocation per period.
    let mut writes = pin!(InFlight::<_, PERIODS_IN_FLIGHT>::new());
    let mut started = false;
    let mut last_latency = 0_u32;
    // Set when the device refused the stream — a rejected period or a
    // start that failed — because those endings carry no tail to wait
    // out: the latency the device last reported describes sound it
    // never began to play.
    let mut refused = false;
    loop {
        // Armed before the filled list is read, so a period committed
        // between the read and the park still completes this wait.
        let committed = pin!(ring.committed());
        while writes.len() < ring.period_count()
            && let Some(index) = ring.take_filled()
        {
            // SAFETY: `index` came off the filled list and is not
            // reclaimed until this chain completes, so the device is the
            // only reader of that period for as long as it holds it.
            let period = unsafe { ring.period(index) };
            writes
                .as_mut()
                .push(async move { (index, device.write(stream, period).await) });
        }
        if !started && !writes.is_empty() {
            // One poll of the set submits every chain it holds. A write
            // may answer on that very poll — a refusal, or a device
            // that completes without an interrupt — and an answer is a
            // completion the same path settles: it is collected rather
            // than dropped.
            let primed = prime(writes.as_mut()).await;
            match device.start(stream).await {
                Ok(()) => started = true,
                Err(error) => {
                    tracing::error!(
                        target: "helios_kernel::audio",
                        %error,
                        stream = stream.index(),
                        "the sound device would not start a negotiated stream"
                    );
                    refused = true;
                }
            }
            for (index, outcome) in primed {
                if let Completion::Stop =
                    settle(shared, ring, stream, index, outcome, &mut last_latency)
                {
                    refused = true;
                }
            }
            if refused {
                // The producer may be parked mid-write; a pump that
                // leaves the ring open would park it for ever.
                ring.close();
                break;
            }
        }
        if writes.is_empty() {
            if ring.is_closed() {
                break;
            }
            committed.await;
            continue;
        }
        match select(writes.next(), committed).await {
            Either::Left((Some((index, outcome)), _)) => {
                if let Completion::Stop =
                    settle(shared, ring, stream, index, outcome, &mut last_latency)
                {
                    refused = true;
                    ring.close();
                    break;
                }
            }
            // The set is not empty, so its stream cannot have ended.
            Either::Left((None, _)) => unreachable!("a non-empty write set never ends"),
            Either::Right(((), _)) => continue,
        }
    }
    if !refused {
        // Every period is played out of the device's ring except the
        // bytes it still held when it took the last one. Stopping now
        // would cut them off, so the tail is waited out rather than
        // truncated.
        drain_device_latency(last_latency, params, timer).await;
    }
    // The release is what brings every chain the device still holds to
    // its used ring: the device is not allowed to answer it while a
    // message is pending, so once it has, the drain cannot park on a
    // write that is never coming back. A release that failed promised
    // nothing — the writes the device still holds are its ring's to
    // account for now, and they are named and counted rather than
    // waited on.
    match quiesce(device, shared).await {
        Ok(()) => {
            while let Some((index, outcome)) = writes.next().await {
                settle(shared, ring, stream, index, outcome, &mut last_latency);
            }
        }
        Err(error) => {
            tracing::error!(
                target: "helios_kernel::audio",
                %error,
                stream = stream.index(),
                outstanding = writes.len(),
                "the sound device would not release a stream that still had writes outstanding"
            );
        }
    }
}

/// What one finished write leaves the stream doing.
enum Completion {
    /// Its period is back on the free list; the pump keeps feeding.
    Playing,
    /// The device refused it; the stream ends here.
    Stop,
}

/// Reclaim the period a finished write carried, report what the device
/// said about it, and say what the stream does next.
///
/// This is the one path every completion takes — a write that answered
/// on its first poll takes it no differently than one that needed an
/// interrupt.
fn settle(
    shared: &StreamShared,
    ring: &PeriodRing,
    stream: StreamId,
    index: u8,
    outcome: AudioResult<XferStatus>,
    last_latency: &mut u32,
) -> Completion {
    ring.reclaim(index);
    match outcome {
        Ok(status) => {
            *last_latency = status.latency_bytes;
            shared
                .played_bytes
                .fetch_add(ring.period_bytes() as u64, Ordering::AcqRel);
            shared.publish(Feedback::LatencyBytes(status.latency_bytes));
            Completion::Playing
        }
        Err(error) => {
            tracing::error!(
                target: "helios_kernel::audio",
                %error,
                stream = stream.index(),
                "the sound device rejected a period; the stream stops here"
            );
            Completion::Stop
        }
    }
}

/// Wait for the bytes the device still held when it took the last
/// period.
async fn drain_device_latency<CpuImpl: Cpu + Clone>(
    latency_bytes: u32,
    params: PcmParams,
    timer: &Timer<CpuImpl>,
) {
    let frame_bytes = params.frame_bytes() as u64;
    if latency_bytes == 0 || frame_bytes == 0 {
        return;
    }
    let frames = u64::from(latency_bytes) / frame_bytes;
    let micros = frames * 1_000_000 / u64::from(params.rate.hz());
    if micros == 0 {
        return;
    }
    timer.sleep_for(Duration::from_micros(micros)).await;
}

/// Poll a set of writes once, which is what submits the chains they
/// carry, and collect whatever answered on that poll.
///
/// Not a wait: the returned future is ready on its first poll. It exists
/// because a chain reaches the device on the first poll of its write and
/// not before, and starting a stream whose transmit ring is still empty
/// is an underrun the kernel would have caused itself. What answered is
/// returned rather than dropped: a `write` that completes on its first
/// poll — a refusal like `NotConfigured` or `PeriodLength`, or a device
/// that needs no interrupt — is a completion like any other, and its
/// period index and error are the caller's to settle.
async fn prime<Fut: Future, const N: usize>(
    mut writes: Pin<&mut InFlight<Fut, N>>,
) -> ArrayVec<Fut::Output, N> {
    let mut done = ArrayVec::new();
    core::future::poll_fn(|cx| {
        while let Poll::Ready(Some(output)) = writes.as_mut().poll_next(cx) {
            done.push(output);
        }
        Poll::Ready(())
    })
    .await;
    done
}
