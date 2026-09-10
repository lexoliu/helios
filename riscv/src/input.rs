//! virtio-input over the platform MMIO bus for the RISC-V backend.
//!
//! A machine with a desktop presents several input devices at once — a
//! keyboard, a relative pointer, an absolute tablet — so every node the
//! device tree names as one is brought up here and handed to the
//! kernel.
//!
//! Concurrency contract: the devices are discovered and programmed on
//! the bootstrap hart before external interrupts are unmasked.
//! Afterwards each reports on the PLIC source the device tree names,
//! delivered to that same hart, which is why the kernel task that
//! drains a device is local to it.

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::num::NonZeroU32;

use fdt::Fdt;
use helios_kernel::ExternalInterruptHandler;

use crate::net::InterruptSourceId;

/// The interrupt route's view of one input device.
///
/// The kernel's reader holds the same `Arc`, which satisfies the input
/// contract through hal's shared-handle impl; this newtype exists only
/// to give the route an interrupt handler to dispatch to.
#[derive(Clone)]
pub(crate) struct VirtioInputDevice {
    inner: Arc<helios_virtio::VirtioMmioInputDevice>,
}

/// One input device the bootstrap hart brought up, together with the
/// PLIC source the device tree routes it to.
pub(crate) struct InputInterrupt {
    pub(crate) source: InterruptSourceId,
    pub(crate) device: VirtioInputDevice,
}

impl ExternalInterruptHandler for VirtioInputDevice {
    fn handle_interrupt(&self) {
        self.inner.handle_interrupt();
    }
}

pub(crate) fn count_input_devices(fdt: &Fdt<'_>) -> usize {
    crate::count_virtio_mmio_devices(fdt, helios_virtio::DeviceType::Input)
}

/// Brings up every input device on the platform bus, hands them all to
/// the kernel, and publishes the service `helios:system/input` is served
/// from.
///
/// One call for every device rather than one per device: the kernel owns
/// the machine's input devices as a set, because that is what a program
/// asking "what can I read?" is answered from.
pub(crate) fn install<WatchdogImpl>(
    kernel: &helios_kernel::Kernel<crate::RiscvCpu, WatchdogImpl>,
    fdt: &Fdt<'_>,
    debug_state: &crate::debug_state::RuntimeState,
) -> Vec<InputInterrupt>
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let discovered = discover_input_devices(fdt);
    let service = helios_kernel::install_input_devices(
        kernel,
        discovered
            .iter()
            .map(|installed| installed.device.inner.clone()),
    );
    debug_state.install_input_service(service);
    discovered
}

fn discover_input_devices(fdt: &Fdt<'_>) -> Vec<InputInterrupt> {
    helios_virtio::mmio_candidates(fdt)
        .filter(|candidate| {
            crate::matches_virtio_mmio_device(candidate.base, helios_virtio::DeviceType::Input)
        })
        .map(|candidate| {
            let base = candidate.base;
            let header = core::ptr::NonNull::new(base as *mut u8)
                .unwrap_or_else(|| panic!("virtio MMIO base {base:#x} was unexpectedly null"));
            let source = candidate
                .interrupt
                .and_then(|interrupt| NonZeroU32::new(interrupt.number))
                .map(InterruptSourceId)
                .unwrap_or_else(|| {
                    panic!("virtio-input node at {base:#x} has no valid interrupt source")
                });
            let device = unsafe { helios_virtio::input_from_mmio(header, candidate.size) }
                .unwrap_or_else(|error| {
                    panic!("failed to initialize virtio-input device at {base:#x}: {error}")
                });
            InputInterrupt {
                source,
                device: VirtioInputDevice {
                    inner: Arc::new(device),
                },
            }
        })
        .collect()
}
