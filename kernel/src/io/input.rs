//! The kernel's ownership of the machine's input devices.
//!
//! An input device is a device that reports without being asked, and a
//! device nobody reads is a device that stops working: its event ring
//! is its whole buffer pool, so once the guest has left every buffer
//! full the host has nowhere to put the next keystroke. The kernel
//! therefore owns each device it brings up and drains it for as long as
//! the machine runs.
//!
//! What is *done* with the events is not decided here. This module
//! holds the device and keeps its ring moving; the interface that hands
//! an event stream to user space is a separate contract, and when it
//! arrives it takes the device from this task rather than adding a
//! second reader.
//!
//! # SMP contract
//!
//! The task is local to the processor that brought the device up,
//! because that is the processor its interrupt is routed to. It parks
//! on the device's own notification and never polls.

use helios_hal::cpu::Cpu;
use helios_hal::input::InputDevice;
use helios_hal::watchdog::Watchdog;

use crate::Kernel;

/// Spawns the task that owns `device` and drains its event ring.
pub fn install_input_device<CpuImpl, WatchdogImpl, Device>(
    kernel: &Kernel<CpuImpl, WatchdogImpl>,
    device: Device,
) where
    CpuImpl: Cpu + Clone + Send + Sync + 'static,
    WatchdogImpl: Watchdog + Clone,
    Device: InputDevice,
{
    kernel.spawn_local_detached(async move {
        follow_input_events(device).await;
    });
}

/// Reports every event the device produces, one line each, and counts
/// the frames they arrive in.
///
/// The frame number is what makes the stream readable: evdev reports a
/// pointer's two axes as separate events, and the number says which
/// ones happened at the same instant.
async fn follow_input_events<Device: InputDevice>(device: Device) {
    let name = device.capabilities().name();
    let mut frame = 0_u64;
    loop {
        match device.next_event().await {
            Ok(event) if event.ends_frame() => frame += 1,
            Ok(event) => {
                tracing::info!(
                    device = name,
                    frame,
                    kind = event.type_name().unwrap_or("unnamed"),
                    code = event.code_name().unwrap_or("unnamed"),
                    value = event.value,
                    "input event"
                );
            }
            Err(error) => {
                // The device and its ring disagree about what is in it.
                // Asking again would fail the same way without ever
                // parking, so the reader stops here and says which
                // device fell silent; nothing else in the kernel
                // depends on it.
                tracing::error!(
                    %error,
                    device = name,
                    "input device faulted; its events are no longer being read"
                );
                return;
            }
        }
    }
}
