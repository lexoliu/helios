//! The handle the rest of the kernel reaches the sound device through.
//!
//! A sound device is a backend type — a virtio-snd device behind
//! whichever transport the platform exposes it on — and the component
//! host that serves `helios:system/audio` never names it. What crosses
//! that boundary here is a queue and a ring of buffers rather than a
//! trait object: the device stays owned, whole, by the tasks in
//! [`super::owner`], and everything else either sends them a message and
//! awaits the reply the message carried along, or fills a period buffer
//! and hands them its index. There is no vtable between a player and its
//! sound card, and no lock around the device.
//!
//! # Concurrency contract
//!
//! [`AudioService`] is cloneable and every method on it may be called
//! from any processor. `claim` is a compare-and-exchange on one word per
//! stream.
//!
//! [`PeriodRing`] is the sample path, and it has exactly two parties:
//! the task that runs the claiming instance's store, which takes a
//! period off the free list, writes into it and commits it, and the
//! stream's playback task, which takes a committed period, hands it to
//! the device and returns it to the free list when the device is done.
//! A period belongs to whichever of the two took it off a list, and to
//! nobody in between, which is what makes writing through the kernel's
//! alias of those pages sound while the device may be reading another
//! one. Neither party ever waits on the other with a lock: each parks on
//! a [`Notify`] armed before it looks at the list it is waiting on, so a
//! period committed or reclaimed between the look and the park cannot be
//! slept through.
//!
//! The feedback queue has one producer per direction — the playback task
//! for a completed period, the device's event drain for an underrun —
//! and one consumer per stream the claiming instance opened. Neither
//! producer ever waits: a drain that waits is a virtio ring that stops,
//! and a playback task that waits is a period the device does not get.
//! What a reader that has fallen behind loses is the oldest feedback,
//! counted and reported on `helios:system/stats`.
//!
//! A claim's release is not a message. The store that holds a claim is
//! dropped by whatever kills its instance, and a drop cannot await a
//! queue that is full, so the release is one permit on a [`Notify`] the
//! playback task races against its inbox. The claim word moves to
//! [`ClaimState::RELEASING`] at the same moment, so nobody is handed the
//! stream between the moment its last owner let go and the moment the
//! device has stopped reading its pages.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use core::task::{Context, Poll};

use alloc::vec::Vec;
use arrayvec::ArrayVec;
use concurrent_queue::ConcurrentQueue;
use futures::channel::oneshot;
use helios_hal::audio::{
    MAX_STREAMS, PcmParams, SampleFormat, SampleRate, StreamDirection, StreamId, StreamInfo,
};
use helios_hal::vmm::VirtAddr;
use triomphe::Arc;

use crate::component::{ProviderError, ProviderSender};
use crate::pins::PinnedRun;
use crate::exec::{Notify, NotifyWaiter};

use super::AudioServiceError;
use super::instance::AudioPins;

/// Periods the kernel keeps in the device's hands at once.
///
/// One period in flight is a stream that underruns between every period
/// it plays, because the device is done with a period before the task
/// that would refill it has run. Four is the depth every PCM stack this
/// kernel targets settles on: enough that a scheduling delay of a whole
/// period is absorbed, few enough that the latency a player feels stays
/// a small multiple of one period.
pub const PERIODS_IN_FLIGHT: usize = 4;

/// How much sound one period carries, in microseconds.
///
/// Ten milliseconds is the unit a period is: it is short enough that
/// [`PERIODS_IN_FLIGHT`] of them is forty milliseconds of latency, and
/// long enough that a stream at 48 kHz wakes its playback task a hundred
/// times a second rather than a thousand.
pub const PERIOD_MICROS: u64 = 10_000;

/// Feedback items one claim's queue holds before the oldest is lost.
///
/// A reader this far behind has not looked at its stream for a whole
/// second of playback, which is longer than any of this feedback stays
/// worth acting on.
pub const FEEDBACK_QUEUE_DEPTH: usize = 128;

/// Requests one claim may have in flight before its next one waits for
/// room.
///
/// Small on purpose: the only request is "configure this stream", and a
/// caller with several outstanding is a caller whose stream has not
/// answered the first.
pub const REQUEST_QUEUE_DEPTH: usize = 4;

/// The format one playback stream is running at.
///
/// Everything about a stream that a caller chooses. The period and the
/// device-side buffer are not here: they follow from the rate and the
/// frame size by [`PERIOD_MICROS`] and [`PERIODS_IN_FLIGHT`], and a
/// caller that picked them itself would be picking its own latency
/// without being able to see what the device does with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaybackFormat {
    pub rate: SampleRate,
    pub channels: u8,
    pub format: SampleFormat,
}

impl PlaybackFormat {
    /// One frame, in bytes.
    pub const fn frame_bytes(&self) -> usize {
        self.format.bytes_per_sample() * self.channels as usize
    }

    /// One period, in bytes.
    ///
    /// `None` when the format describes no frames — a channel count of
    /// zero — or when a period of [`PERIOD_MICROS`] holds no whole
    /// frame at this rate, which no rate this contract names does.
    pub fn period_bytes(&self) -> Option<u32> {
        let frames = u64::from(self.rate.hz()) * PERIOD_MICROS / 1_000_000;
        let bytes = frames.checked_mul(self.frame_bytes() as u64)?;
        u32::try_from(bytes).ok().filter(|bytes| *bytes != 0)
    }

    /// The parameters this format asks the device for.
    ///
    /// `None` for a format whose period has no bytes, which is a format
    /// no device could be configured with.
    pub fn pcm_params(&self) -> Option<PcmParams> {
        let period_bytes = self.period_bytes()?;
        Some(PcmParams {
            rate: self.rate,
            channels: self.channels,
            format: self.format,
            buffer_bytes: period_bytes * (PERIODS_IN_FLIGHT as u32),
            period_bytes,
        })
    }

    /// Whether `info` describes a stream that accepts this format.
    pub fn accepted_by(&self, info: &StreamInfo) -> bool {
        info.direction == StreamDirection::Playback
            && info.formats.contains(self.format)
            && info.rates.contains(self.rate)
            && self.channels >= info.channels_min
            && self.channels <= info.channels_max
    }
}

/// Something the kernel reports back about a stream that is playing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feedback {
    /// The device took a period, and still held this many bytes
    /// unplayed when it did.
    ///
    /// This is the latency the player is running at, in the only unit
    /// the device can state it in; divided by the frame size and the
    /// rate it is a time.
    LatencyBytes(u32),
    /// The device ran out of samples before the next period arrived,
    /// and what came out of the jack is a gap.
    ///
    /// Nothing was substituted for the samples that did not arrive. A
    /// player that wants silence sends silence.
    Xrun,
}

/// What a stream's claim word holds.
///
/// Three states rather than two, because giving the stream back is
/// asynchronous: the device has to be told to stop and to release what
/// it allocated before the pages behind the period buffers can go back
/// to a pool, and until it has, the stream is neither held by anybody
/// nor free for anybody.
pub(super) struct ClaimState;

impl ClaimState {
    /// Nobody holds this stream.
    pub(super) const FREE: u8 = 0;
    /// One instance holds it.
    pub(super) const HELD: u8 = 1;
    /// Its last owner let go and the playback task has not finished
    /// handing the device's resources back.
    pub(super) const RELEASING: u8 = 2;
}

/// Work for the task that owns one playback stream.
///
/// It carries the claim generation it was made under: a request that
/// outlived its claim names a stream its instance no longer holds, and
/// serving it against whoever holds the stream now would let a dead
/// player reconfigure a live one's sound.
///
/// One variant, because there is one thing to ask that task for while
/// it is idle. Stopping a stream that is *playing* is not a message: the
/// task is inside the pump then and is not reading this queue, and a
/// pump interrupted between a chain's submission and its completion
/// would leave a descriptor in the device's ring that nobody reaps. A
/// stop closes the ring instead, which is what the pump is watching.
pub(super) enum PlaybackRequest {
    /// Configure the stream for `params` and play what arrives on
    /// `ring`.
    Negotiate {
        generation: u64,
        params: PcmParams,
        ring: Arc<PeriodRing>,
        reply: oneshot::Sender<Result<(), AudioServiceError>>,
    },
}

/// The period buffers of one playback session, and the two lists that
/// say who owns each of them.
///
/// See the module's concurrency contract: a period belongs to whichever
/// party took it off a list, so the addresses here are handed out as
/// slices only through the `unsafe` accessors, whose contract is that
/// ownership.
pub struct PeriodRing {
    /// How many bytes of each period the device plays. The pages behind
    /// a period are the rounded-up whole granules the arena pinned; the
    /// device is told this many.
    period_bytes: usize,
    /// Where each period appears in the kernel's own address space, in
    /// the order they were pinned.
    periods: ArrayVec<VirtAddr, PERIODS_IN_FLIGHT>,
    /// Periods the producer has written and committed, oldest first.
    filled: ConcurrentQueue<u8>,
    /// Periods the producer may write next.
    free: ConcurrentQueue<u8>,
    /// Raised once per committed period, and once when the ring closes.
    committed: Notify,
    /// Raised once per period the device gave back.
    reclaimed: Notify,
    /// Set when the producer's stream ends, a stop is asked for, or the
    /// claim is let go.
    closed: AtomicBool,
    /// The reply a `stop` is waiting for, once one has been asked for.
    ///
    /// One slot: a claim is one instance's, and its store serves one
    /// host call at a time, so there is never a second stop to answer.
    stop: ConcurrentQueue<oneshot::Sender<Result<(), AudioServiceError>>>,
    /// Bytes the producer has committed, which is what the device is
    /// asked to play.
    committed_bytes: AtomicU64,
}

impl PeriodRing {
    /// A ring over `runs`, playing `period_bytes` of each.
    ///
    /// # Panics
    ///
    /// Panics when a run is shorter than the period it is to carry,
    /// which would have the device read past pages the instance owns.
    pub(super) fn new(runs: &[PinnedRun], period_bytes: u32) -> Self {
        let period_bytes = period_bytes as usize;
        let mut periods = ArrayVec::new();
        for run in runs {
            assert!(
                run.bytes >= period_bytes as u64,
                "a {} byte pinned run cannot carry a {period_bytes} byte period",
                run.bytes
            );
            periods.push(run.kernel_alias());
        }
        let free = ConcurrentQueue::bounded(periods.len().max(1));
        for index in 0..periods.len() {
            free.push(index as u8)
                .expect("the free list is as deep as the ring");
        }
        Self {
            period_bytes,
            filled: ConcurrentQueue::bounded(periods.len().max(1)),
            free,
            periods,
            committed: Notify::new(),
            reclaimed: Notify::new(),
            closed: AtomicBool::new(false),
            stop: ConcurrentQueue::bounded(1),
            committed_bytes: AtomicU64::new(0),
        }
    }

    /// How many bytes of one period the device plays.
    pub const fn period_bytes(&self) -> usize {
        self.period_bytes
    }

    /// How many periods this ring holds.
    pub fn period_count(&self) -> usize {
        self.periods.len()
    }

    /// Bytes the producer has committed since the ring was built.
    pub fn committed_bytes(&self) -> u64 {
        self.committed_bytes.load(Ordering::Acquire)
    }

    /// Whether the producer has ended.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// End the ring: no more periods will be committed.
    ///
    /// Called when the guest's sample stream ends and again when the
    /// claim is let go, both of which may happen from a drop, so it
    /// neither allocates nor waits.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.committed.notify_all();
        self.reclaimed.notify_all();
    }

    /// Ask the playback task to stop the stream, and be told when the
    /// device has.
    ///
    /// Closing the ring is what the pump watches, so this is the whole
    /// of the request; the reply travels back when the task has stopped
    /// the stream and let the device release what it allocated.
    pub fn request_stop(&self, reply: oneshot::Sender<Result<(), AudioServiceError>>) {
        // A second stop on the same ring answers the first caller and
        // waits for the same teardown, which is what a store serving one
        // host call at a time can never produce anyway.
        if self.stop.push(reply).is_err() {
            tracing::warn!(
                target: "helios_kernel::audio",
                "a playback stream was asked to stop twice at once"
            );
        }
        self.close();
    }

    /// The reply a stop is waiting for, if one asked.
    pub fn take_stop_reply(&self) -> Option<oneshot::Sender<Result<(), AudioServiceError>>> {
        self.stop.pop().ok()
    }

    /// A wait on the next commit, armed now.
    ///
    /// Armed at creation, so a period committed between the caller's
    /// look at the filled list and its park still completes the wait.
    pub fn committed(&self) -> crate::exec::Notified<'_> {
        self.committed.notified()
    }

    /// A wait on the next period the device gives back, armed now.
    pub fn reclaim_waiter(&self) -> NotifyWaiter {
        self.reclaimed.waiter()
    }

    pub fn poll_committed(&self, cx: &mut Context<'_>, waiter: &mut NotifyWaiter) -> Poll<()> {
        self.committed.poll_notified(cx, waiter)
    }

    pub fn poll_reclaimed(&self, cx: &mut Context<'_>, waiter: &mut NotifyWaiter) -> Poll<()> {
        self.reclaimed.poll_notified(cx, waiter)
    }

    /// Take a period the producer may write into.
    pub fn take_free(&self) -> Option<u8> {
        self.free.pop().ok()
    }

    /// Hand a written period to the playback task.
    pub fn commit(&self, index: u8) {
        self.committed_bytes
            .fetch_add(self.period_bytes as u64, Ordering::AcqRel);
        self.filled.push(index).expect(
            "the filled list is as deep as the ring and this period came out of the free list",
        );
        self.committed.notify_all();
    }

    /// Take the oldest period the producer committed.
    pub fn take_filled(&self) -> Option<u8> {
        self.filled.pop().ok()
    }

    /// Give a period the device has finished with back to the producer.
    pub fn reclaim(&self, index: u8) {
        self.free.push(index).expect(
            "the free list is as deep as the ring and this period came out of the filled list",
        );
        self.reclaimed.notify_all();
    }

    /// The bytes of period `index`, to play.
    ///
    /// # Safety
    ///
    /// The caller must own `index`: it must have taken it off the filled
    /// list and not yet reclaimed it. The two lists are what make that
    /// exclusive.
    ///
    /// # Panics
    ///
    /// Panics when `index` names no period in this ring, which would
    /// mean the two lists carry something they were never given.
    pub unsafe fn period(&self, index: u8) -> &[u8] {
        let base = self.address_of(index);
        // SAFETY: the arena pinned at least `period_bytes` readable,
        // writable bytes at this address and holds them for as long as
        // this ring exists, and the caller owns the period.
        unsafe { core::slice::from_raw_parts(base.raw() as *const u8, self.period_bytes) }
    }

    /// The bytes of period `index`, to write.
    ///
    /// # Safety
    ///
    /// The caller must own `index`: it must have taken it off the free
    /// list and not yet committed it.
    ///
    /// # Panics
    ///
    /// Panics for the reason [`Self::period`] does.
    #[expect(
        clippy::mut_from_ref,
        reason = "the ring is shared between the producer and the playback task, and which of \
                  them may write a period is decided by the free and filled lists rather than by \
                  a borrow; that is this function's safety contract"
    )]
    pub unsafe fn period_mut(&self, index: u8) -> &mut [u8] {
        let base = self.address_of(index);
        // SAFETY: as `period`, plus the caller's ownership of the
        // period being the exclusive one the free list hands out.
        unsafe { core::slice::from_raw_parts_mut(base.raw() as *mut u8, self.period_bytes) }
    }

    fn address_of(&self, index: u8) -> VirtAddr {
        *self
            .periods
            .get(index as usize)
            .unwrap_or_else(|| panic!("period {index} does not exist in this ring"))
    }
}

/// The producer's side of one playback session's period buffers.
///
/// # Concurrency contract
///
/// Owned by the one task that runs the claiming instance's store, and
/// never shared. The wait it carries is armed before every look at the
/// free list, so a period the device gives back between a look and a
/// park cannot be slept through.
pub struct PeriodWriter {
    ring: Arc<PeriodRing>,
    /// The period being filled and how many of its bytes are written.
    /// Present only between taking one off the free list and committing
    /// it, which is exactly when this task owns it.
    current: Option<(u8, usize)>,
    waiter: NotifyWaiter,
    accepted: u64,
}

impl PeriodWriter {
    pub fn new(ring: Arc<PeriodRing>) -> Self {
        let waiter = ring.reclaim_waiter();
        Self {
            ring,
            current: None,
            waiter,
            accepted: 0,
        }
    }

    /// Bytes this writer has taken from its producer.
    pub const fn accepted(&self) -> u64 {
        self.accepted
    }

    /// Copy as much of `bytes` into the period buffers as there is room
    /// for, committing each period as it fills.
    ///
    /// Ready with how many bytes were taken, which is at least one;
    /// pending when every period is in the device's hands, in which case
    /// the wait is registered and one reclaimed period wakes it.
    pub fn poll_write(&mut self, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<usize> {
        if bytes.is_empty() {
            return Poll::Ready(0);
        }
        let period_bytes = self.ring.period_bytes();
        let mut taken = 0;
        while taken < bytes.len() {
            let (index, filled) = match self.current {
                Some(current) => current,
                None => match self.next_period(cx) {
                    Poll::Ready(index) => (index, 0),
                    // Every period is with the device. Anything already
                    // taken is committed and reported; a first pass that
                    // took nothing parks, which is the backpressure that
                    // paces a player to its own stream.
                    Poll::Pending if taken == 0 => return Poll::Pending,
                    Poll::Pending => break,
                },
            };
            let room = period_bytes - filled;
            let take = room.min(bytes.len() - taken);
            // SAFETY: `index` came off the free list and has not been
            // committed, so this task owns the period.
            let period = unsafe { self.ring.period_mut(index) };
            period[filled..filled + take].copy_from_slice(&bytes[taken..taken + take]);
            taken += take;
            let filled = filled + take;
            if filled == period_bytes {
                self.current = None;
                self.ring.commit(index);
            } else {
                self.current = Some((index, filled));
            }
        }
        self.accepted += taken as u64;
        Poll::Ready(taken)
    }

    /// End this writer's half of the stream.
    ///
    /// A period the producer left part-written is committed with its
    /// tail zeroed. That is the one place the kernel adds silence, and
    /// it is not a substitute for samples that did not arrive: it is
    /// what makes the samples that did arrive playable at all, because
    /// the device takes whole periods and would otherwise play whatever
    /// the rest of the buffer held.
    pub fn finish(&mut self) {
        if let Some((index, filled)) = self.current.take() {
            // SAFETY: as `poll_write`; the period is still this task's.
            let period = unsafe { self.ring.period_mut(index) };
            period[filled..].fill(0);
            self.ring.commit(index);
        }
        self.ring.close();
    }

    fn next_period(&mut self, cx: &mut Context<'_>) -> Poll<u8> {
        loop {
            if let Some(index) = self.ring.take_free() {
                return Poll::Ready(index);
            }
            match self.ring.poll_reclaimed(cx, &mut self.waiter) {
                Poll::Ready(()) => continue,
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl Drop for PeriodWriter {
    fn drop(&mut self) {
        // A producer that died mid-period still owes the playback task
        // an end: without it the task would wait for a period nobody is
        // going to commit.
        self.finish();
    }
}

/// One playback stream, as everything outside its owner tasks sees it.
pub(super) struct StreamShared {
    info: StreamInfo,
    requests: ProviderSender<PlaybackRequest>,
    /// One of [`ClaimState`]'s three values.
    pub(super) claim: AtomicU8,
    /// Bumped by every successful claim, so a request that outlived its
    /// claim can be told from one that did not.
    pub(super) generation: AtomicU64,
    /// One permit per claim that has been let go.
    pub(super) release: Notify,
    /// The period buffers of a claim that has been let go, waiting for
    /// the playback task to stop the device reading them.
    ///
    /// One slot, and one is provably enough: there is at most one claim
    /// at a time on a stream, and the claim word does not return to
    /// [`ClaimState::FREE`] until this has been drained.
    pub(super) returned: ConcurrentQueue<AudioPins>,
    /// What the kernel has to say about the stream that is playing.
    feedback: ConcurrentQueue<Feedback>,
    /// Raised once per feedback item published.
    published: Notify,
    /// Bytes the device has taken since boot.
    pub(super) played_bytes: AtomicU64,
    /// Underruns the device reported since boot.
    pub(super) xruns: AtomicU64,
    /// Feedback dropped because its reader had not kept up.
    pub(super) lost_feedback: AtomicU64,
}

impl StreamShared {
    pub(super) fn new(info: StreamInfo, requests: ProviderSender<PlaybackRequest>) -> Self {
        Self {
            info,
            requests,
            claim: AtomicU8::new(ClaimState::FREE),
            generation: AtomicU64::new(0),
            release: Notify::new(),
            returned: ConcurrentQueue::bounded(1),
            feedback: ConcurrentQueue::bounded(FEEDBACK_QUEUE_DEPTH),
            published: Notify::new(),
            played_bytes: AtomicU64::new(0),
            xruns: AtomicU64::new(0),
            lost_feedback: AtomicU64::new(0),
        }
    }

    pub(super) const fn info(&self) -> &StreamInfo {
        &self.info
    }

    pub(super) fn id(&self) -> StreamId {
        self.info.id
    }

    pub(super) fn is_claimed(&self) -> bool {
        self.claim.load(Ordering::Acquire) != ClaimState::FREE
    }

    /// Hand one item to whoever holds this stream.
    ///
    /// Never waits. The callers are the playback task, whose wait would
    /// be a period the device does not get, and the device's event
    /// drain, whose wait would be a ring that stops moving. What a
    /// reader that has fallen behind loses is the oldest item, counted.
    pub(super) fn publish(&self, item: Feedback) {
        if !self.is_claimed() {
            return;
        }
        if self.feedback.push(item).is_err() {
            // Make room by dropping the oldest, which is the item whose
            // moment has most thoroughly passed. An underrun the reader
            // never sees would otherwise be indistinguishable from one
            // that never happened.
            let _ = self.feedback.pop();
            self.lost_feedback.fetch_add(1, Ordering::AcqRel);
            let _ = self.feedback.push(item);
        }
        self.published.notify_all();
    }

    /// Throw away whatever the queue still holds.
    ///
    /// Called when a claim is taken and again when it is let go, so no
    /// instance is handed feedback that was meant for the one before it.
    pub(super) fn drain_feedback(&self) {
        while self.feedback.pop().is_ok() {}
    }

    fn snapshot(&self) -> AudioStreamSnapshot {
        AudioStreamSnapshot {
            id: self.info.id.index(),
            claimed: self.is_claimed(),
            played_bytes: self.played_bytes.load(Ordering::Acquire),
            xruns: self.xruns.load(Ordering::Acquire),
            lost_feedback: self.lost_feedback.load(Ordering::Acquire),
        }
    }
}

/// What the stats surface reports about one playback stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioStreamSnapshot {
    /// The device's own id for the stream.
    pub id: u32,
    /// Whether an instance holds it right now.
    pub claimed: bool,
    /// Bytes the device has taken since boot.
    pub played_bytes: u64,
    /// Underruns the device reported. Persistently moving is a player
    /// that is not keeping up with its own stream.
    pub xruns: u64,
    /// Feedback dropped because a reader had not kept up.
    pub lost_feedback: u64,
}

/// The machine's sound device, as everything outside the owner tasks
/// sees it.
pub(super) struct AudioShared {
    /// Every playback stream, in the order the device presented them.
    pub(super) playback: ArrayVec<Arc<StreamShared>, MAX_STREAMS>,
    /// The ids of the streams that capture. Named rather than dropped so
    /// that a caller that asks for one is told what is wrong with its
    /// request instead of that the stream does not exist.
    pub(super) capture: ArrayVec<StreamId, MAX_STREAMS>,
}

/// The kernel's sound device, as everything outside the owner tasks
/// sees it.
#[derive(Clone)]
pub struct AudioService {
    shared: Arc<AudioShared>,
}

impl AudioService {
    pub(super) const fn from_shared(shared: Arc<AudioShared>) -> Self {
        Self { shared }
    }

    /// The shared state behind this handle, so the module's own tests
    /// can drive an owner task without a kernel to spawn it on.
    #[cfg(test)]
    pub(super) fn shared_for_tests(&self) -> &AudioShared {
        &self.shared
    }

    /// How many playback streams the device presents.
    pub fn playback_count(&self) -> usize {
        self.shared.playback.len()
    }

    /// What the device says about every stream that plays, in the order
    /// it presented them.
    pub fn available(&self) -> impl Iterator<Item = &StreamInfo> + '_ {
        self.shared.playback.iter().map(|stream| stream.info())
    }

    /// What the stats surface reports about every playback stream.
    pub fn snapshot(&self) -> Vec<AudioStreamSnapshot> {
        self.shared
            .playback
            .iter()
            .map(|stream| stream.snapshot())
            .collect()
    }

    /// Take exclusive ownership of the stream `id` names.
    ///
    /// The second caller is refused rather than queued: a player waiting
    /// for a stream another player holds is a provisioning mistake, not
    /// a shortage. This is the rule `display.claim` and `input.claim`
    /// both follow.
    ///
    /// The feedback queue is emptied before the claim is handed back, so
    /// the new owner's stream starts from what happens after it.
    pub fn claim(&self, id: u32) -> Result<AudioClaim, AudioServiceError> {
        if self.shared.playback.is_empty() && self.shared.capture.is_empty() {
            return Err(AudioServiceError::Unavailable);
        }
        let id = StreamId::new(id);
        let Some(stream) = self.shared.playback.iter().find(|stream| stream.id() == id) else {
            return Err(if self.shared.capture.contains(&id) {
                AudioServiceError::NotPlayback
            } else {
                AudioServiceError::NoSuchStream
            });
        };
        stream
            .claim
            .compare_exchange(
                ClaimState::FREE,
                ClaimState::HELD,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| AudioServiceError::AlreadyClaimed)?;
        let generation = stream.generation.fetch_add(1, Ordering::AcqRel) + 1;
        stream.drain_feedback();
        Ok(AudioClaim {
            shared: stream.clone(),
            generation,
        })
    }
}

/// One instance's hold on one playback stream.
///
/// Dropping it — or dying while holding it — stops the stream, hands
/// every pinned page back to the instance's pool and leaves the device
/// reading nothing before the stream is offered to anyone else.
pub struct AudioClaim {
    shared: Arc<StreamShared>,
    generation: u64,
}

impl AudioClaim {
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The device's own id for the stream this claim holds.
    pub fn id(&self) -> u32 {
        self.shared.id().index()
    }

    /// What the device says about this stream.
    pub fn info(&self) -> &StreamInfo {
        self.shared.info()
    }

    /// A handle that can queue work for this claim.
    ///
    /// Cheap and cloneable, so a host call that has to keep sending
    /// after it has given the store back carries one instead of
    /// borrowing the claim. A sender that outlives its claim sends
    /// requests the playback task drops, because they no longer name the
    /// generation it is serving.
    pub fn sender(&self) -> AudioSender {
        AudioSender {
            shared: self.shared.clone(),
            generation: self.generation,
        }
    }

    /// A reader of this claim's feedback.
    ///
    /// The wait is armed as the reader is built, so an item published
    /// between here and the first poll wakes it rather than being waited
    /// past.
    pub fn feedback(&self) -> FeedbackReader {
        FeedbackReader {
            waiter: self.shared.published.waiter(),
            shared: self.shared.clone(),
        }
    }

    /// Hand this claim's period buffers to the playback task.
    ///
    /// The pages are not freed here: the device may still be reading
    /// them, and the playback task is what knows when it has stopped.
    /// Called by the store's own drop, so it neither allocates nor
    /// waits.
    ///
    /// # Panics
    ///
    /// Panics when the slot is already occupied, which would mean two
    /// claims existed at once — the one invariant the claim word is
    /// there to keep.
    pub(super) fn return_pins(&self, pins: AudioPins) {
        self.shared
            .returned
            .push(pins)
            .unwrap_or_else(|_| panic!("two audio claims returned their period buffers at once"));
    }
}

impl Drop for AudioClaim {
    fn drop(&mut self) {
        // Not a message: a drop cannot await room in a queue, and the
        // instance this claim belonged to may already be dead. One
        // permit is enough, because there is exactly one claim to
        // release and the playback task clears the word once it has.
        self.shared
            .claim
            .store(ClaimState::RELEASING, Ordering::Release);
        self.shared.release.notify_one();
    }
}

/// The right to queue work for one claim.
#[derive(Clone)]
pub struct AudioSender {
    shared: Arc<StreamShared>,
    generation: u64,
}

impl AudioSender {
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Configure the stream for `params` and play what arrives on
    /// `ring`.
    pub async fn negotiate(
        &self,
        params: PcmParams,
        ring: Arc<PeriodRing>,
    ) -> Result<(), AudioServiceError> {
        let (reply, answer) = oneshot::channel();
        self.send(PlaybackRequest::Negotiate {
            generation: self.generation,
            params,
            ring,
            reply,
        })
        .await?;
        answer.await.map_err(|_| AudioServiceError::Closed)?
    }

    async fn send(&self, request: PlaybackRequest) -> Result<(), AudioServiceError> {
        self.shared.requests.send(request).await.map_err(closed)
    }
}

/// One reader of one claim's feedback.
///
/// # Concurrency contract
///
/// Owned by whatever task drives the stream, and the wait it carries is
/// armed before every look at the queue: [`FeedbackReader::poll_burst`]
/// re-arms as part of completing a wait, and the constructor arms the
/// first one, so an item published between a look and a park cannot be
/// slept through.
pub struct FeedbackReader {
    shared: Arc<StreamShared>,
    waiter: NotifyWaiter,
}

/// Feedback published since a reader last looked.
pub type FeedbackBurst = ArrayVec<Feedback, FEEDBACK_QUEUE_DEPTH>;

impl FeedbackReader {
    /// Every item queued right now, or a park until one is.
    pub fn poll_burst(&mut self, cx: &mut Context<'_>) -> Poll<FeedbackBurst> {
        loop {
            let mut burst = FeedbackBurst::new();
            while !burst.is_full() {
                match self.shared.feedback.pop() {
                    Ok(item) => burst.push(item),
                    Err(_) => break,
                }
            }
            if !burst.is_empty() {
                return Poll::Ready(burst);
            }
            match self.shared.published.poll_notified(cx, &mut self.waiter) {
                Poll::Ready(()) => continue,
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

const fn closed(error: ProviderError) -> AudioServiceError {
    match error {
        ProviderError::Unavailable | ProviderError::Closed | ProviderError::Full => {
            AudioServiceError::Closed
        }
    }
}
