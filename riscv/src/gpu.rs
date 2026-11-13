//! virtio-gpu over the platform MMIO bus for the RISC-V backend.
//!
//! Concurrency contract: the device is discovered and programmed on the
//! bootstrap hart before external interrupts are unmasked. Afterwards
//! its completions and its display-change announcements arrive on the
//! PLIC line the device tree names, on the hart that takes that PLIC's
//! interrupts. The kernel task that follows the topology stays local to
//! the bootstrap hart, which spawned it; the handler's notification
//! wakes it from whichever hart that is.

extern crate alloc;

use alloc::sync::Arc;

use fdt::Fdt;
use helios_kernel::ExternalInterruptHandler;

use crate::net::PlicInterrupt;

/// The interrupt route's view of the platform's display engine.
///
/// The kernel's topology task holds the same `Arc`, which satisfies the
/// display contract through hal's shared-handle impl; this newtype
/// exists only to give the route an interrupt handler to dispatch to.
#[derive(Clone)]
pub(crate) struct VirtioDisplayDevice {
    inner: Arc<helios_virtio::VirtioMmioGpuDevice>,
}

/// The display device the bootstrap hart brought up, together with the
/// PLIC line the device tree routes it to.
pub(crate) struct DisplayInterrupt {
    pub(crate) line: PlicInterrupt,
    pub(crate) device: VirtioDisplayDevice,
}

impl ExternalInterruptHandler for VirtioDisplayDevice {
    fn handle_interrupt(&self) {
        self.inner.handle_interrupt();
    }
}

pub(crate) fn has_display_device(fdt: &Fdt<'_>) -> bool {
    crate::count_virtio_mmio_devices(fdt, helios_virtio::DeviceType::Gpu) != 0
}

/// Brings the platform's display engine up and hands it to the kernel.
pub(crate) fn install<WatchdogImpl>(
    cpu: &crate::RiscvCpu,
    kernel: &helios_kernel::Kernel<crate::RiscvCpu, WatchdogImpl>,
    fdt: &Fdt<'_>,
    debug_state: &crate::debug_state::RuntimeState,
) -> Option<DisplayInterrupt>
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let (device, line) = discover_display_device(fdt)?;
    let service = helios_kernel::install_display_device(kernel, cpu, device.inner.clone());
    debug_state.install_display_service(service);
    let service = helios_kernel::install_gpu3d_device(kernel, cpu, device.inner.clone());
    debug_state.install_gpu3d_service(service);
    Some(DisplayInterrupt { line, device })
}

fn discover_display_device(fdt: &Fdt<'_>) -> Option<(VirtioDisplayDevice, PlicInterrupt)> {
    let candidate = helios_virtio::mmio_candidates(fdt).find(|candidate| {
        crate::matches_virtio_mmio_device(candidate.base, helios_virtio::DeviceType::Gpu)
    })?;
    let base = candidate.base;
    let header = core::ptr::NonNull::new(base as *mut u8)
        .unwrap_or_else(|| panic!("virtio MMIO base {base:#x} was unexpectedly null"));
    let line = candidate
        .interrupt
        .and_then(PlicInterrupt::from_specifier)
        .unwrap_or_else(|| panic!("virtio-gpu node at {base:#x} has no valid interrupt source"));
    let device =
        unsafe { helios_virtio::gpu_from_mmio(header, candidate.size) }.unwrap_or_else(|error| {
            panic!("failed to initialize virtio-gpu device at {base:#x}: {error}")
        });
    Some((
        VirtioDisplayDevice {
            inner: Arc::new(device),
        },
        line,
    ))
}
