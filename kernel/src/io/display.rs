//! The kernel's ownership of a display device.
//!
//! A display device is one of the few pieces of hardware whose state
//! changes without anybody asking: a monitor is plugged in, unplugged,
//! or resized by whatever is hosting this machine, and the device
//! announces it through a configuration-change interrupt. The
//! announcement has to be consumed by somebody — a driver that nobody
//! reads leaves the event latched and the *next* change raises no
//! interrupt at all — so the kernel owns the device and follows its
//! topology for as long as the machine runs.
//!
//! What is displayed is not decided here. This module holds the device
//! and keeps its topology current; the interface that hands scanouts and
//! frame buffers to user space is a separate contract.
//!
//! # SMP contract
//!
//! The task is local to the processor that brought the device up,
//! because that is the processor its interrupt is routed to. It parks on
//! the device's own notification and never polls.

use helios_hal::cpu::Cpu;
use helios_hal::display::DisplayDevice;
use helios_hal::watchdog::Watchdog;

use crate::Kernel;

/// Spawns the task that owns `device` and follows its display topology.
///
/// The task is the device's only reader until a display service exists,
/// and it is what keeps the device's change notification live: each
/// announcement is collected and the new set of scanouts read back, so
/// the device is never left with an event nobody took.
pub fn install_display_device<CpuImpl, WatchdogImpl, Device>(
    kernel: &Kernel<CpuImpl, WatchdogImpl>,
    device: Device,
) where
    CpuImpl: Cpu + Clone + Send + Sync + 'static,
    WatchdogImpl: Watchdog + Clone,
    Device: DisplayDevice,
{
    kernel.spawn_local_detached(async move {
        follow_display_topology(device).await;
    });
}

async fn follow_display_topology<Device: DisplayDevice>(device: Device) {
    loop {
        device.display_changed().await;
        match device.scanouts().await {
            Ok(scanouts) => {
                let enabled = scanouts.iter().filter(|info| info.enabled).count();
                tracing::info!(
                    scanouts = scanouts.len(),
                    enabled,
                    "display topology changed"
                );
                for info in &scanouts {
                    tracing::debug!(
                        scanout = info.id.index(),
                        x = info.geometry.x,
                        y = info.geometry.y,
                        width = info.geometry.width,
                        height = info.geometry.height,
                        enabled = info.enabled,
                        "scanout geometry"
                    );
                }
            }
            Err(error) => {
                // The device announced a change and then refused to
                // describe it. The topology the kernel holds is the one
                // it read last; the next announcement re-reads it.
                tracing::warn!(
                    %error,
                    "the display device would not report its topology after announcing a change"
                );
            }
        }
    }
}
