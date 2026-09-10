//! The input path's own tests.
//!
//! The device side is a scripted fake: it hands out a list of events and
//! then stalls forever, so a test drives the same drain loop the kernel
//! runs and asserts on what reached the queue rather than on what the
//! drain intended. The queue, the claim word and the lost-report counter
//! are the real ones.

use alloc::vec::Vec;
use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

use arrayvec::ArrayVec;
use futures_lite::future::block_on;
use helios_hal::input::{CodeBitmap, InputCapabilities, InputDevice, InputEvent, codes};
use helios_hal::io::IoResult;
use std::sync::Mutex;
use triomphe::Arc;

use super::owner::{device_channels, drain_input_events};
use super::service::{EVENT_QUEUE_DEPTH, InputService, InputShared};
use super::{InputServiceError, MAX_CLAIMED_DEVICES};

/// The two devices a desktop presents, named the way QEMU names them.
const KEYBOARD: &str = "QEMU Virtio Keyboard";
const TABLET: &str = "QEMU Virtio Tablet";

/// A device that reports a scripted list of events and then says nothing
/// more.
///
/// "Says nothing more" is a future that never completes, which is what a
/// real device does between keystrokes: the drain parks on it, and a
/// test that polls the drain once past the end of the script sees
/// exactly the state the events left behind.
struct ScriptedDevice {
    capabilities: InputCapabilities,
    script: Mutex<Vec<InputEvent>>,
    leds: Mutex<Vec<(u16, bool)>>,
}

impl ScriptedDevice {
    fn new(name: &str, script: Vec<InputEvent>) -> Self {
        let mut capabilities = InputCapabilities::new();
        capabilities
            .set_name(name.as_bytes())
            .expect("an ASCII name is accepted");
        capabilities
            .push_event_type(codes::EV_SYN, CodeBitmap::empty())
            .expect("a first declaration of a type is accepted");
        Self {
            capabilities,
            // Reversed, so taking the next event is a pop from the end.
            script: Mutex::new(script.into_iter().rev().collect()),
            leds: Mutex::new(Vec::new()),
        }
    }

    fn leds(&self) -> Vec<(u16, bool)> {
        self.leds.lock().expect("no test panics here").clone()
    }
}

impl InputDevice for ScriptedDevice {
    fn capabilities(&self) -> &InputCapabilities {
        &self.capabilities
    }

    async fn next_event(&self) -> IoResult<InputEvent> {
        let next = self.script.lock().expect("no test panics here").pop();
        match next {
            Some(event) => Ok(event),
            None => core::future::pending().await,
        }
    }

    async fn set_led(&self, code: u16, on: bool) -> IoResult<()> {
        self.leds
            .lock()
            .expect("no test panics here")
            .push((code, on));
        Ok(())
    }
}

/// One report: a key going down and the `SYN_REPORT` that closes it.
fn key_report(code: u16, value: i32) -> [InputEvent; 2] {
    [
        InputEvent::new(codes::EV_KEY, code, value),
        InputEvent::new(codes::EV_SYN, codes::SYN_REPORT, 0),
    ]
}

/// A service holding one device of each name in `names`, with no drain
/// running: a test that wants events published drives the drain itself.
fn service_of(names: &[&str]) -> InputService {
    let mut devices = ArrayVec::<Arc<InputShared>, MAX_CLAIMED_DEVICES>::new();
    for (index, name) in names.iter().enumerate() {
        let (shared, _requests) = device_channels(
            index,
            ScriptedDevice::new(name, Vec::new()).capabilities.clone(),
        );
        // The indicator inbox is dropped with `_requests`; no test here
        // sends one through a service built this way.
        devices.push(shared);
    }
    InputService::from_devices(Arc::new(devices))
}

/// Run `future` until it parks, which for the drain means the device has
/// nothing more to say.
fn poll_until_parked<F: Future>(future: F) {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    for _ in 0..1024 {
        if future.as_mut().poll(&mut context).is_ready() {
            return;
        }
    }
}

/// Only one instance may read a device. The second caller is refused
/// rather than queued, because a compositor waiting for a keyboard
/// another compositor holds is a provisioning mistake.
#[test]
fn a_second_claim_of_the_same_device_is_refused() {
    let service = service_of(&[KEYBOARD, TABLET]);

    let held = service.claim(KEYBOARD).expect("the first claim succeeds");

    assert_eq!(
        service.claim(KEYBOARD).err(),
        Some(InputServiceError::AlreadyClaimed)
    );
    // A different device is unaffected: they are claimed one at a time
    // so a compositor can hold them all and a program that wants only
    // the tablet still can.
    service.claim(TABLET).expect("another device is still free");
    drop(held);
}

/// Two devices of the same name are two devices. Claiming the name twice
/// yields both rather than refusing the second.
#[test]
fn two_devices_of_one_name_are_both_claimable() {
    let service = service_of(&[KEYBOARD, KEYBOARD]);

    let first = service.claim(KEYBOARD).expect("the first is free");
    let second = service.claim(KEYBOARD).expect("the second is free too");

    assert_ne!(first.index(), second.index());
    assert_eq!(
        service.claim(KEYBOARD).err(),
        Some(InputServiceError::AlreadyClaimed)
    );
}

/// A name no device has is said so, rather than being answered with
/// whatever the machine does have.
#[test]
fn an_unknown_device_name_is_refused_by_name() {
    let service = service_of(&[KEYBOARD]);

    assert_eq!(
        service.claim("Some Other Keyboard").err(),
        Some(InputServiceError::NoSuchDevice)
    );
}

/// A machine with no input device says so rather than reporting that
/// every device is held.
#[test]
fn a_machine_with_no_input_device_says_so() {
    let service = service_of(&[]);

    assert_eq!(
        service.claim(KEYBOARD).err(),
        Some(InputServiceError::Unavailable)
    );
    assert_eq!(service.device_count(), 0);
}

/// Dropping a claim — which is what killing an instance does — hands the
/// device straight back. There is nothing to wait for: no page the
/// hardware may still read, no device resource to destroy.
#[test]
fn dropping_a_claim_releases_the_device() {
    let service = service_of(&[KEYBOARD]);

    let held = service.claim(KEYBOARD).expect("the first claim succeeds");
    let first_generation = held.generation();
    drop(held);

    let again = service.claim(KEYBOARD).expect("a released device is free");
    assert!(
        again.generation() > first_generation,
        "every claim gets a generation of its own, so a request that \
         outlived one can be told from a request that did not"
    );
}

/// The events a device reports reach the reader in the order the device
/// produced them, `SYN_REPORT` included: the frame boundary belongs to
/// whoever is interpreting the stream.
#[test]
fn a_claimed_device_relays_whole_reports_in_order() {
    let script: Vec<InputEvent> = key_report(codes::KEY_A, 1)
        .into_iter()
        .chain(key_report(codes::KEY_A, 0))
        .collect();
    let device = ScriptedDevice::new(KEYBOARD, script);
    let (shared, _requests) = device_channels(0, device.capabilities().clone());
    let service = InputService::from_devices(Arc::new(one(shared.clone())));
    let claim = service.claim(KEYBOARD).expect("the device is free");
    let mut events = claim.events();

    poll_until_parked(drain_input_events(&device, &shared));

    let mut context = Context::from_waker(Waker::noop());
    let burst = match events.poll_burst(&mut context) {
        Poll::Ready(burst) => burst,
        Poll::Pending => panic!("two reports were committed, so a read has to find them"),
    };
    assert_eq!(
        burst.as_slice(),
        &[
            InputEvent::new(codes::EV_KEY, codes::KEY_A, 1),
            InputEvent::new(codes::EV_SYN, codes::SYN_REPORT, 0),
            InputEvent::new(codes::EV_KEY, codes::KEY_A, 0),
            InputEvent::new(codes::EV_SYN, codes::SYN_REPORT, 0),
        ]
    );
}

/// Nothing is queued for a device nobody claimed. The drain still runs —
/// the ring has to keep moving — but the events go to the log rather
/// than accumulating for a reader that may never arrive.
#[test]
fn an_unclaimed_device_queues_nothing() {
    let device = ScriptedDevice::new(KEYBOARD, key_report(codes::KEY_A, 1).to_vec());
    let (shared, _requests) = device_channels(0, device.capabilities().clone());
    let service = InputService::from_devices(Arc::new(one(shared.clone())));

    poll_until_parked(drain_input_events(&device, &shared));

    let snapshot = &service.snapshot()[0];
    assert!(!snapshot.claimed);
    assert_eq!(snapshot.events_delivered, 0);
    assert_eq!(snapshot.lost_reports, 0);
}

/// A reader that has not kept up loses a whole report and is counted for
/// exactly one, however many events that report held. Half a report
/// would leave a pointer with one axis of a move whose other half it
/// never saw.
#[test]
fn a_full_queue_drops_a_whole_report_and_counts_one() {
    // One report per two events, so the queue is exactly full after
    // `EVENT_QUEUE_DEPTH / 2` of them, and the next one has nowhere to
    // go.
    let reports = EVENT_QUEUE_DEPTH / 2;
    let script: Vec<InputEvent> = (0..=reports)
        .flat_map(|index| key_report(codes::KEY_A, index as i32))
        .collect();
    let device = ScriptedDevice::new(KEYBOARD, script);
    let (shared, _requests) = device_channels(0, device.capabilities().clone());
    let service = InputService::from_devices(Arc::new(one(shared.clone())));
    let claim = service.claim(KEYBOARD).expect("the device is free");
    let mut events = claim.events();

    poll_until_parked(drain_input_events(&device, &shared));

    let snapshot = &service.snapshot()[0];
    assert_eq!(
        snapshot.lost_reports, 1,
        "one report past a full queue is one report lost, not two events"
    );
    assert_eq!(snapshot.events_delivered, EVENT_QUEUE_DEPTH as u64);

    // What did fit is whole: every report that reached the reader ends
    // with the `SYN_REPORT` that closed it.
    let mut context = Context::from_waker(Waker::noop());
    let burst = match events.poll_burst(&mut context) {
        Poll::Ready(burst) => burst,
        Poll::Pending => panic!("a full queue has events in it"),
    };
    assert_eq!(burst.len(), EVENT_QUEUE_DEPTH);
    assert!(
        burst
            .chunks(2)
            .all(|report| report[1].ends_frame() && !report[0].ends_frame()),
        "every delivered report is a key event and the report that closed it"
    );
}

/// A reader that catches up is served again: the counter records what
/// was lost and the queue goes back to relaying.
#[test]
fn a_reader_that_catches_up_is_served_again() {
    let reports = EVENT_QUEUE_DEPTH / 2;
    let script: Vec<InputEvent> = (0..=reports)
        .flat_map(|index| key_report(codes::KEY_A, index as i32))
        .collect();
    let device = ScriptedDevice::new(KEYBOARD, script);
    let (shared, _requests) = device_channels(0, device.capabilities().clone());
    let service = InputService::from_devices(Arc::new(one(shared.clone())));
    let claim = service.claim(KEYBOARD).expect("the device is free");
    let mut events = claim.events();
    poll_until_parked(drain_input_events(&device, &shared));

    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(events.poll_burst(&mut context), Poll::Ready(_)));

    // The device says one more thing now that the queue has room.
    let follow_up = ScriptedDevice::new(KEYBOARD, key_report(codes::KEY_B, 1).to_vec());
    poll_until_parked(drain_input_events(&follow_up, &shared));

    let burst = match events.poll_burst(&mut context) {
        Poll::Ready(burst) => burst,
        Poll::Pending => panic!("a drained queue accepts the next report"),
    };
    assert_eq!(burst[0], InputEvent::new(codes::EV_KEY, codes::KEY_B, 1));
    assert_eq!(service.snapshot()[0].lost_reports, 1);
}

/// A reader with nothing to read parks rather than spinning, and the
/// wait it parked on is armed before it looked — so a report committed
/// between the look and the park wakes it.
#[test]
fn a_reader_with_nothing_to_read_parks() {
    let device = ScriptedDevice::new(KEYBOARD, key_report(codes::KEY_A, 1).to_vec());
    let (shared, _requests) = device_channels(0, device.capabilities().clone());
    let service = InputService::from_devices(Arc::new(one(shared.clone())));
    let claim = service.claim(KEYBOARD).expect("the device is free");
    let mut events = claim.events();

    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(events.poll_burst(&mut context), Poll::Pending));

    poll_until_parked(drain_input_events(&device, &shared));

    assert!(matches!(events.poll_burst(&mut context), Poll::Ready(_)));
}

/// Whatever a dead claim never read is thrown away with it: the next
/// instance to hold the device starts from the reports that follow its
/// own claim, not from a keystroke the last one was typing.
#[test]
fn a_release_throws_away_what_its_owner_never_read() {
    let device = ScriptedDevice::new(KEYBOARD, key_report(codes::KEY_A, 1).to_vec());
    let (shared, _requests) = device_channels(0, device.capabilities().clone());
    let service = InputService::from_devices(Arc::new(one(shared.clone())));
    let claim = service.claim(KEYBOARD).expect("the device is free");
    poll_until_parked(drain_input_events(&device, &shared));
    drop(claim);

    let claim = service.claim(KEYBOARD).expect("a released device is free");
    let mut events = claim.events();
    let mut context = Context::from_waker(Waker::noop());

    assert!(
        matches!(events.poll_burst(&mut context), Poll::Pending),
        "the new owner is not handed the last owner's keystrokes"
    );
}

/// An indicator change reaches the device, and one made under a claim
/// that has since been released does not.
#[test]
fn an_indicator_change_reaches_the_device_under_its_own_claim() {
    let device = ScriptedDevice::new(KEYBOARD, Vec::new());
    let (shared, requests) = device_channels(0, device.capabilities().clone());
    let service = InputService::from_devices(Arc::new(one(shared.clone())));
    let claim = service.claim(KEYBOARD).expect("the device is free");
    let leds = claim.leds();

    block_on(async {
        let sent = pin!(leds.set_led(codes::LED_CAPSL, true));
        let served = pin!(super::owner::serve_indicators(&device, &shared, &requests));
        // The server never ends, so the pair is driven until the send
        // resolves; what it resolved to is the device's own answer.
        futures::future::select(sent, served).await;
    });

    assert_eq!(device.leds(), alloc::vec![(codes::LED_CAPSL, true)]);
}

/// One device, as the list the service is built from.
fn one(shared: Arc<InputShared>) -> ArrayVec<Arc<InputShared>, MAX_CLAIMED_DEVICES> {
    let mut devices = ArrayVec::new();
    devices.push(shared);
    devices
}
