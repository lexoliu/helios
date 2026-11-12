//! The handle the rest of the kernel reaches the input devices through.
//!
//! An input device is a backend type — a virtio-input device behind
//! whichever transport the platform exposes it on — and the component
//! host that serves `helios:system/input` never names it. What crosses
//! that boundary here is a queue rather than a trait object: the device
//! stays owned, whole, by the tasks in [`super::owner`], and everything
//! else either reads the queue those tasks fill or sends them a message
//! and awaits the reply the message carried along. There is no vtable
//! between a compositor and its keyboard, and no lock around the device.
//!
//! # Concurrency contract
//!
//! [`InputService`] is cloneable and every method on it may be called
//! from any processor. `claim` is a compare-and-exchange on one word per
//! device.
//!
//! The event queue belongs to a claim, not to the device: every
//! successful `claim` builds a [`ClaimEvents`] (queue, [`Notify`], alive
//! flag) and installs it in the device's slot, and the claim's drop
//! retires and uninstalls it. A stream a claim opened holds that claim's
//! own state, so a stream kept past its claim's release can never pop
//! the next owner's events: it reads what was left in its own queue and
//! then ends. The slot is a pointer cell, written by `claim` and by the
//! claim's drop (any processor) and read by the drain task once per
//! report (the device's processor, task context); it is never held
//! across an await and no interrupt handler reaches it, so it needs no
//! local interrupt mask.
//!
//! Each claim's queue has exactly one producer — the drain task that
//! owns the device — and one consumer per stream the claiming instance
//! opened. The producer never waits: a report that does not fit is
//! dropped whole and counted, because parking the producer would park
//! the drain, and a drain that is parked is a virtio ring the host runs
//! out of buffers on. The consumer parks on [`Notify`], armed before it
//! looks at the queue, so a report published between the look and the
//! park wakes it. The alive flag is written once, by the claim's drop,
//! and read by consumers; it is set false before the final notify, so a
//! consumer woken by the retirement sees it.
//!
//! The indicator queue is separate, because the direction is: a
//! `set-led` must not wait behind an event nobody has read, and the
//! device's own contract puts it on a queue of its own.
//!
//! A claim's release is not a message. The store that holds a claim is
//! dropped by whatever kills its instance, and a drop cannot await, so
//! the drop retires the claim's event state, moves the generation so
//! indicator requests queued under the claim fail their check, and
//! stores the claim word back to free. Nothing is owed to the device:
//! the kernel's drain has never stopped, and the next claimant starts
//! from the next report.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use core::task::{Context, Poll};

use alloc::string::String;
use alloc::vec::Vec;
use arrayvec::ArrayVec;
use concurrent_queue::ConcurrentQueue;
use futures::channel::oneshot;
use helios_hal::input::{InputCapabilities, InputEvent};
use spin::Mutex;
use triomphe::Arc;

use crate::component::{ProviderError, ProviderSender};
use crate::exec::{Notify, NotifyWaiter};
use crate::process::InputClaimCap;

use super::{InputServiceError, MAX_CLAIMED_DEVICES};

/// Events one device's queue holds before a report is lost.
///
/// The driver's event ring is 64 buffers deep, and this is the same
/// number on purpose: a reader that has fallen a whole ring behind is a
/// reader the device has already outrun, and buffering more would only
/// hand it input that is older than the ring the events came out of.
pub const EVENT_QUEUE_DEPTH: usize = 64;

/// Events one report may carry.
///
/// As deep as the queue, because a report longer than the queue could
/// never be delivered whole however fast the reader is, and delivering
/// part of one is the thing this path exists to prevent.
pub const MAX_REPORT_EVENTS: usize = EVENT_QUEUE_DEPTH;

/// Indicator changes one claim may have in flight before its next one
/// waits for room.
///
/// Small on purpose: a caller that has four indicator changes
/// outstanding is a caller whose keyboard has not answered the first,
/// and making it wait is the backpressure the path needs.
pub(super) const LED_QUEUE_DEPTH: usize = 4;

/// Which of the machine's input devices this is.
///
/// Assigned when the kernel brings the devices up, in the order the
/// backend discovered them, and stable for the life of the machine. It
/// is what tells two devices of the same name apart once one of them has
/// been claimed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeviceIndex(u8);

impl DeviceIndex {
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// What a device's claim word holds.
///
/// Two states, unlike the display's three: letting an input device go
/// hands nothing back — no pinned page the hardware may still be reading,
/// no device-side resource to destroy — so a released device is free the
/// instant its last owner's drop has run.
struct ClaimState;

impl ClaimState {
    /// Nobody holds this device.
    const FREE: u8 = 0;
    /// One instance holds it.
    const HELD: u8 = 1;
}

/// Work for the task that owns one device's indicators.
pub(super) struct LedRequest {
    /// The claim this was made under. A request that outlived its claim
    /// names a device its instance no longer holds, and serving it
    /// against whoever holds the device now would let a dead compositor
    /// light somebody else's caps lock.
    pub(super) generation: u64,
    pub(super) code: u16,
    pub(super) on: bool,
    pub(super) reply: oneshot::Sender<Result<(), InputServiceError>>,
}

/// What became of one report the drain task offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReportOutcome {
    /// It is in the queue, and its reader has been woken.
    Delivered,
    /// Its reader had not kept up, so the whole report was dropped and
    /// counted.
    Dropped,
    /// Nobody holds this device, so nobody was owed the report.
    Unclaimed,
}

/// The event state of one claim.
///
/// Built by `claim`, installed in [`InputShared`] and shared with every
/// stream the claim opens; retired by the claim's drop. Because it dies
/// with the claim, a stream that outlives the claim cannot reach a later
/// owner's events.
///
/// # Concurrency contract
///
/// `events` has one producer, the drain task (the device's processor),
/// and consumers on whichever processor drives each stream. `published`
/// is raised by the producer and by the retirement. `alive` is written
/// once, by the claim's drop, and read by consumers and the producer.
pub(super) struct ClaimEvents {
    /// Whole reports the drain task committed, oldest first.
    events: ConcurrentQueue<InputEvent>,
    /// Raised once per committed report, and once on retirement.
    published: Notify,
    /// Cleared when the claim is dropped.
    alive: AtomicBool,
}

impl ClaimEvents {
    fn new() -> Self {
        Self {
            events: ConcurrentQueue::bounded(EVENT_QUEUE_DEPTH),
            published: Notify::new(),
            alive: AtomicBool::new(true),
        }
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// End every reader once it has read what is queued.
    fn retire(&self) {
        self.alive.store(false, Ordering::Release);
        self.published.notify_all();
    }
}

/// One device, as everything outside its owner tasks sees it.
pub(super) struct InputShared {
    index: DeviceIndex,
    /// What the device said about itself while it was brought up.
    /// Immutable from then on, so this is read without a lock.
    capabilities: InputCapabilities,
    /// The holding claim's event state, if any.
    ///
    /// Written by `claim` and the claim's drop (any processor), read by
    /// the drain task once per report to clone the `Arc` (device
    /// processor, task context). A pointer cell only: never held across
    /// an await, never reached from an interrupt, and the queue it
    /// points at is lock-free.
    installed: Mutex<Option<Arc<ClaimEvents>>>,
    /// One of [`ClaimState`]'s two values.
    claim: AtomicU8,
    /// Bumped by every successful claim and by every claim's drop, so a
    /// request that outlived its claim can be told from one that did
    /// not.
    generation: AtomicU64,
    /// Events this device has handed to a reader.
    delivered: AtomicU64,
    /// Reports dropped because the reader had not kept up.
    lost_reports: AtomicU64,
    leds: ProviderSender<LedRequest>,
}

impl InputShared {
    pub(super) fn new(
        index: usize,
        capabilities: InputCapabilities,
        leds: ProviderSender<LedRequest>,
    ) -> Self {
        Self {
            index: DeviceIndex(
                u8::try_from(index).expect("a machine brings up at most 256 input devices"),
            ),
            capabilities,
            installed: Mutex::new(None),
            claim: AtomicU8::new(ClaimState::FREE),
            generation: AtomicU64::new(0),
            delivered: AtomicU64::new(0),
            lost_reports: AtomicU64::new(0),
            leds,
        }
    }

    pub(super) const fn capabilities(&self) -> &InputCapabilities {
        &self.capabilities
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Whether an instance is reading this device right now.
    pub(super) fn is_claimed(&self) -> bool {
        self.claim.load(Ordering::Acquire) == ClaimState::HELD
    }

    /// Hand one whole report to whoever holds this device.
    ///
    /// `truncated` says the drain task could not even hold the report,
    /// which costs the report exactly what a full queue does: it is one
    /// report the reader will not see, and it is counted as one.
    ///
    /// Never waits. The caller is the task that keeps the device's ring
    /// moving, and a ring that stops moving is a machine whose next
    /// keystroke has nowhere to go.
    pub(super) fn publish_report(&self, report: &[InputEvent], truncated: bool) -> ReportOutcome {
        let Some(claim) = self.installed.lock().clone() else {
            return ReportOutcome::Unclaimed;
        };
        if !claim.is_alive() {
            return ReportOutcome::Unclaimed;
        }
        if truncated || claim.events.len() + report.len() > EVENT_QUEUE_DEPTH {
            let lost = self.lost_reports.fetch_add(1, Ordering::AcqRel) + 1;
            if lost == 1 {
                tracing::warn!(
                    target: "helios_kernel::input",
                    device = self.capabilities.name(),
                    events = report.len(),
                    "an input report was dropped: its reader is not keeping up with the device"
                );
            }
            return ReportOutcome::Dropped;
        }
        for event in report {
            claim
                .events
                .push(*event)
                .expect("the only producer just measured room for this whole report");
        }
        self.delivered
            .fetch_add(report.len() as u64, Ordering::AcqRel);
        claim.published.notify_all();
        ReportOutcome::Delivered
    }

    fn snapshot(&self) -> InputDeviceSnapshot {
        InputDeviceSnapshot {
            index: self.index,
            name: self.capabilities.name().into(),
            claimed: self.is_claimed(),
            events_delivered: self.delivered.load(Ordering::Acquire),
            lost_reports: self.lost_reports.load(Ordering::Acquire),
        }
    }
}

/// What the stats surface reports about one input device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputDeviceSnapshot {
    /// Which of the machine's devices this is.
    pub index: DeviceIndex,
    /// The name the device presents itself by.
    pub name: String,
    /// Whether an instance is reading it right now.
    pub claimed: bool,
    /// Events the kernel has handed to a reader since boot.
    pub events_delivered: u64,
    /// Reports dropped because a reader had not kept up. Persistently
    /// moving is a compositor that is not draining its stream.
    pub lost_reports: u64,
}

/// The machine's input devices, as everything outside the owner tasks
/// sees them.
#[derive(Clone)]
pub struct InputService {
    devices: Arc<ArrayVec<Arc<InputShared>, MAX_CLAIMED_DEVICES>>,
}

impl InputService {
    pub(super) const fn from_devices(
        devices: Arc<ArrayVec<Arc<InputShared>, MAX_CLAIMED_DEVICES>>,
    ) -> Self {
        Self { devices }
    }

    /// How many input devices the kernel brought up.
    pub fn device_count(&self) -> usize {
        self.devices.len()
    }

    /// What every device on this machine says about itself, in the order
    /// the backend discovered them.
    pub fn available(&self) -> impl Iterator<Item = &InputCapabilities> + '_ {
        self.devices.iter().map(|device| &device.capabilities)
    }

    /// What the stats surface reports about every device.
    pub fn snapshot(&self) -> Vec<InputDeviceSnapshot> {
        self.devices
            .iter()
            .map(|device| device.snapshot())
            .collect()
    }

    /// Take exclusive ownership of the device `name` names.
    ///
    /// The first free device of that name is taken, so a machine with
    /// two identical keyboards hands out both to a caller that asks
    /// twice. A caller that asks for a name no device has is told so
    /// rather than being handed something else.
    ///
    /// The claim gets an event queue of its own, so the new owner's
    /// stream starts from the reports that follow. A report the device
    /// was publishing at that instant may still reach it, which is the
    /// input the user was giving as the claim was taken.
    ///
    /// `_cap` is the proof that the caller's authority names the input
    /// class; the check happened where the cap was derived.
    pub fn claim(&self, name: &str, _cap: InputClaimCap) -> Result<InputClaim, InputServiceError> {
        if self.devices.is_empty() {
            return Err(InputServiceError::Unavailable);
        }
        let mut named = false;
        for device in self.devices.iter() {
            if device.capabilities.name() != name {
                continue;
            }
            named = true;
            if device
                .claim
                .compare_exchange(
                    ClaimState::FREE,
                    ClaimState::HELD,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                continue;
            }
            let generation = device.generation.fetch_add(1, Ordering::AcqRel) + 1;
            let events = Arc::new(ClaimEvents::new());
            *device.installed.lock() = Some(events.clone());
            return Ok(InputClaim {
                shared: device.clone(),
                events,
                generation,
            });
        }
        Err(if named {
            InputServiceError::AlreadyClaimed
        } else {
            InputServiceError::NoSuchDevice
        })
    }
}

/// One instance's hold on one input device.
///
/// Dropping it — or dying while holding it — hands the device back to
/// the kernel's own drain, ends every stream it opened once that stream
/// has read what was already queued for it, and fails every indicator
/// request still queued under it.
pub struct InputClaim {
    shared: Arc<InputShared>,
    /// This claim's own event state; also installed in `shared`.
    events: Arc<ClaimEvents>,
    generation: u64,
}

impl InputClaim {
    /// Which of the machine's devices this claim holds.
    pub fn index(&self) -> DeviceIndex {
        self.shared.index
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// What the device says about itself.
    pub fn capabilities(&self) -> &InputCapabilities {
        self.shared.capabilities()
    }

    /// A reader of this device's events.
    ///
    /// The wait is armed as the reader is built, so an event published
    /// between here and the first poll wakes it rather than being
    /// waited past.
    pub fn events(&self) -> InputEvents {
        InputEvents {
            waiter: self.events.published.waiter(),
            events: self.events.clone(),
        }
    }

    /// A handle that can change this device's indicators.
    ///
    /// Cheap and cloneable, so a host call that has to keep sending
    /// after it has given the store back carries one instead of
    /// borrowing the claim. A sender that outlives its claim sends
    /// requests the owner task drops, because they no longer name the
    /// generation it is serving.
    pub fn leds(&self) -> InputLedSender {
        InputLedSender {
            shared: self.shared.clone(),
            generation: self.generation,
        }
    }
}

impl Drop for InputClaim {
    fn drop(&mut self) {
        // Not a message: a drop cannot await room in a queue, and the
        // instance this claim belonged to may already be dead. The
        // device itself needs nothing — the kernel's drain never
        // stopped — so retiring this claim's events and the word going
        // back to free are the whole release.
        //
        // The generation moves here, not only on the next claim: an
        // indicator request queued under this claim must fail its check
        // however long the device then sits unclaimed.
        self.events.retire();
        *self.shared.installed.lock() = None;
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        // Last: a new claim installs its own state only after this word
        // is free, so the uninstall above can never remove its state.
        self.shared.claim.store(ClaimState::FREE, Ordering::Release);
    }
}

/// The right to change one claim's indicators.
#[derive(Clone)]
pub struct InputLedSender {
    shared: Arc<InputShared>,
    generation: u64,
}

impl InputLedSender {
    /// Turn one indicator on or off, and await the device's answer.
    pub async fn set_led(&self, code: u16, on: bool) -> Result<(), InputServiceError> {
        let (reply, answer) = oneshot::channel();
        self.shared
            .leds
            .send(LedRequest {
                generation: self.generation,
                code,
                on,
                reply,
            })
            .await
            .map_err(closed)?;
        answer.await.map_err(|_| InputServiceError::Closed)?
    }
}

/// Events a device reported, as many as one drain of its queue yields.
pub type EventBurst = ArrayVec<InputEvent, EVENT_QUEUE_DEPTH>;

/// One reader of one device's events.
///
/// # Concurrency contract
///
/// Owned by whatever task drives the stream, and the wait it carries is
/// armed before every look at the queue: [`InputEvents::poll_burst`]
/// re-arms as part of completing a wait, and the constructor arms the
/// first one, so a report committed between a look and a park cannot be
/// slept through. It holds its claim's own queue, never the device's
/// current one, so it ends with its claim.
pub struct InputEvents {
    events: Arc<ClaimEvents>,
    waiter: NotifyWaiter,
}

impl InputEvents {
    /// Every event queued right now, a park until one is, or `None` once
    /// the claim is gone and nothing is left to read.
    ///
    /// Events come back in the order the device produced them,
    /// `SYN_REPORT` included, and a burst may end mid-report: the queue
    /// holds only reports the drain committed whole, so the rest of one
    /// is already on its way rather than lost.
    pub fn poll_burst(&mut self, cx: &mut Context<'_>) -> Poll<Option<EventBurst>> {
        loop {
            let mut burst = EventBurst::new();
            while !burst.is_full() {
                match self.events.events.pop() {
                    Ok(event) => burst.push(event),
                    Err(_) => break,
                }
            }
            if !burst.is_empty() {
                return Poll::Ready(Some(burst));
            }
            if !self.events.is_alive() {
                return Poll::Ready(None);
            }
            match self.events.published.poll_notified(cx, &mut self.waiter) {
                Poll::Ready(()) => continue,
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

const fn closed(error: ProviderError) -> InputServiceError {
    match error {
        ProviderError::Unavailable | ProviderError::Closed | ProviderError::Full => {
            InputServiceError::Closed
        }
    }
}
