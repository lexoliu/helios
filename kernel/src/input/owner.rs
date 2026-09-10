//! The kernel's ownership of the machine's input devices.
//!
//! An input device is a device that reports without being asked, and a
//! device nobody reads is a device that stops working: its event ring is
//! its whole buffer pool, so once the guest has left every buffer full
//! the host has nowhere to put the next keystroke. The tasks here own
//! each device and keep its ring moving for as long as the machine runs,
//! whether or not anybody is reading it.
//!
//! What is *done* with the events is decided by whoever holds the claim.
//! It never touches the device: it reads the queue this drain fills, and
//! the tasks here are the only code that speaks to the device.
//!
//! # SMP contract
//!
//! Two tasks per device, both local to the processor that brought the
//! device up, because that is the processor the device's interrupt is
//! routed to.
//!
//! * The drain parks on the device's own notification and never polls.
//!   It is the single producer of that device's event queue, and it
//!   never waits on the queue: a report that does not fit is dropped
//!   whole and counted, because a drain that waits is a ring that stops.
//! * The indicator server owns the direction that travels towards the
//!   device. It is a separate task for the reason the hardware has a
//!   separate queue: lighting a caps lock must not wait behind an event
//!   nobody has read.
//!
//! Both hold the same device handle. The trait's own contract says every
//! method takes `&self`, may be called from several tasks at once, and
//! that an implementation serialises access to its own rings.

use arrayvec::ArrayVec;
use helios_hal::cpu::Cpu;
use helios_hal::input::{InputCapabilities, InputDevice, InputEvent};
use helios_hal::watchdog::Watchdog;
use triomphe::Arc;

use crate::Kernel;
use crate::component::{ProviderReceiver, provider_channel};

use super::service::{InputShared, LED_QUEUE_DEPTH, LedRequest, MAX_REPORT_EVENTS, ReportOutcome};
use super::{InputService, InputServiceError, MAX_CLAIMED_DEVICES};

/// Brings every input device the backend discovered under kernel
/// ownership and publishes the service `helios:system/input` is served
/// from.
///
/// The devices are never handed anywhere else. What callers get back is
/// a handle to the tasks this spawns, which is what a claim and every
/// event after it travels through.
///
/// # Panics
///
/// Panics when the backend describes more than [`MAX_CLAIMED_DEVICES`]
/// devices, which is more than the kernel can route interrupts for: a
/// device past that bound is one whose events would never arrive, and
/// bringing it up silently would leave a keyboard that never reports.
pub fn install_input_devices<CpuImpl, WatchdogImpl, Device, Devices>(
    kernel: &Kernel<CpuImpl, WatchdogImpl>,
    devices: Devices,
) -> InputService
where
    CpuImpl: Cpu + Clone + Send + Sync + 'static,
    WatchdogImpl: Watchdog + Clone,
    Device: InputDevice + Clone,
    Devices: IntoIterator<Item = Device>,
{
    let mut installed = ArrayVec::<Arc<InputShared>, MAX_CLAIMED_DEVICES>::new();
    for device in devices {
        assert!(
            !installed.is_full(),
            "the backend described more than {MAX_CLAIMED_DEVICES} input devices"
        );
        let (shared, led_requests) =
            device_channels(installed.len(), device.capabilities().clone());
        {
            let device = device.clone();
            let shared = shared.clone();
            kernel.spawn_local_detached(async move {
                drain_input_events(&device, &shared).await;
            });
        }
        {
            let shared = shared.clone();
            kernel.spawn_local_detached(async move {
                serve_indicators(&device, &shared, &led_requests).await;
            });
        }
        installed.push(shared);
    }
    InputService::from_devices(Arc::new(installed))
}

/// The shared state and the indicator inbox one device's tasks are built
/// around.
///
/// Split out of [`install_input_devices`] so a test can drive the same
/// loops without a kernel to spawn them on.
pub(super) fn device_channels(
    index: usize,
    capabilities: InputCapabilities,
) -> (Arc<InputShared>, ProviderReceiver<LedRequest>) {
    let (leds, requests) = provider_channel(LED_QUEUE_DEPTH);
    (
        Arc::new(InputShared::new(index, capabilities, leds)),
        requests,
    )
}

/// Keeps one device's ring moving, for as long as the machine runs.
///
/// Events are gathered into the report they belong to and handed over
/// whole: a reader that acted on a pointer's first axis before the
/// report closed would draw a diagonal as a staircase, and a reader that
/// was handed the first axis and never the second would have a pointer
/// that walked sideways. Whoever holds the device gets the report or
/// does not; there is no third answer.
pub(super) async fn drain_input_events<Device: InputDevice>(device: &Device, shared: &InputShared) {
    let name = device.capabilities().name();
    let mut report = ArrayVec::<InputEvent, MAX_REPORT_EVENTS>::new();
    let mut truncated = false;
    let mut frame = 0_u64;
    loop {
        let event = match device.next_event().await {
            Ok(event) => event,
            Err(error) => {
                // The device and its ring disagree about what is in it.
                // Asking again would fail the same way without ever
                // parking, so the drain stops here and says which device
                // fell silent; whoever holds it sees a stream that ends.
                tracing::error!(
                    target: "helios_kernel::input",
                    %error,
                    device = name,
                    "input device faulted; its events are no longer being read"
                );
                return;
            }
        };
        // A report longer than the buffer is one the queue could not
        // carry whole either, so the rest of it is gathered and thrown
        // away together rather than committed as a fragment.
        if report.try_push(event).is_err() {
            truncated = true;
        }
        if !event.ends_frame() {
            continue;
        }
        frame += 1;
        if shared.publish_report(&report, truncated) == ReportOutcome::Unclaimed {
            // Nobody is reading. The events still have to leave the
            // ring, and the log is the only place they can go — which is
            // also what makes a machine with no compositor testable.
            log_report(name, frame, &report);
        }
        report.clear();
        truncated = false;
    }
}

/// Write one report to the kernel's log, one line per event.
///
/// The frame number is what makes the log readable: evdev reports a
/// pointer's two axes as separate events, and the number says which ones
/// happened at the same instant.
fn log_report(name: &str, frame: u64, report: &[InputEvent]) {
    for event in report {
        if event.ends_frame() {
            continue;
        }
        tracing::info!(
            target: "helios_kernel::input",
            device = name,
            frame,
            kind = event.type_name().unwrap_or("unnamed"),
            code = event.code_name().unwrap_or("unnamed"),
            value = event.value,
            "input event"
        );
    }
}

/// Serves one device's indicators for as long as the machine runs.
pub(super) async fn serve_indicators<Device: InputDevice>(
    device: &Device,
    shared: &InputShared,
    requests: &ProviderReceiver<LedRequest>,
) {
    while let Some(request) = requests.recv().await {
        if request.generation != shared.generation() {
            // The claim this was made under is gone. Its reply channel
            // is dropped with the request, which is what tells the
            // caller — if it is still there at all — that its claim no
            // longer names this device.
            continue;
        }
        let outcome = device
            .set_led(request.code, request.on)
            .await
            .map_err(|error| {
                tracing::warn!(
                    target: "helios_kernel::input",
                    %error,
                    device = device.capabilities().name(),
                    code = request.code,
                    "an input device refused an indicator change"
                );
                InputServiceError::DeviceFault
            });
        // A caller that stopped waiting is not an error: the send fails
        // because its future was dropped, and the device has already
        // been told.
        let _ = request.reply.send(outcome);
    }
}
