//! virtio-vsock over the platform MMIO bus for the RISC-V backend.
//!
//! Concurrency contract: the device is discovered and programmed on the
//! bootstrap hart before external interrupts are unmasked. Afterwards
//! the kernel's vsock service owns it — one receive pump plus whichever
//! tasks are transmitting — and completions arrive on the PLIC source
//! the device tree names, so every waiter parks on a notification
//! rather than polling.

extern crate alloc;

use alloc::sync::Arc;
use core::num::NonZeroU32;

use fdt::Fdt;
use helios_kernel::ExternalInterruptHandler;

use crate::net::InterruptSourceId;

/// The interrupt route's view of the platform's vsock device.
///
/// The kernel's vsock service holds the same `Arc`, which satisfies the
/// device contract through hal's shared-handle impl; this newtype exists
/// only to give the route an interrupt handler to dispatch to.
#[derive(Clone)]
pub(crate) struct VirtioVsockDevice {
    inner: Arc<helios_virtio::VirtioMmioVsockDevice>,
}

/// The vsock device the bootstrap hart brought up, together with the
/// PLIC source the device tree routes it to.
pub(crate) struct VsockInterrupt {
    pub(crate) source: InterruptSourceId,
    pub(crate) device: VirtioVsockDevice,
}

impl ExternalInterruptHandler for VirtioVsockDevice {
    fn handle_interrupt(&self) {
        self.inner.handle_interrupt();
    }
}

pub(crate) fn has_vsock_device(fdt: &Fdt<'_>) -> bool {
    crate::count_virtio_mmio_devices(fdt, helios_virtio::DeviceType::Vsock) != 0
}

/// Brings up the vsock device and publishes it as the machine's host
/// link.
pub(crate) fn install<WatchdogImpl>(
    kernel: &helios_kernel::Kernel<crate::RiscvCpu, WatchdogImpl>,
    cpu: &crate::RiscvCpu,
    fdt: &Fdt<'_>,
    debug_state: &crate::debug_state::RuntimeState,
) -> Option<VsockInterrupt>
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let (device, source) = discover_vsock_device(fdt)?;
    let service = helios_kernel::install_vsock_device(kernel, cpu, device.inner.clone());
    let guest_cid = service.guest_cid();
    debug_state.install_vsock_service(helios_kernel::ComponentHostVsockService::from_service(
        service,
    ));
    tracing::info!(guest_cid, irq = source.0.get(), "virtio vsock online");
    Some(VsockInterrupt { source, device })
}

fn discover_vsock_device(fdt: &Fdt<'_>) -> Option<(VirtioVsockDevice, InterruptSourceId)> {
    let candidate = helios_virtio::mmio_candidates(fdt).find(|candidate| {
        crate::matches_virtio_mmio_device(candidate.base, helios_virtio::DeviceType::Vsock)
    })?;
    let base = candidate.base;
    let header = core::ptr::NonNull::new(base as *mut u8)
        .unwrap_or_else(|| panic!("virtio MMIO base {base:#x} was unexpectedly null"));
    let source = candidate
        .interrupt
        .and_then(|interrupt| NonZeroU32::new(interrupt.number))
        .map(InterruptSourceId)
        .unwrap_or_else(|| panic!("virtio-vsock node at {base:#x} has no valid interrupt source"));
    let device =
        unsafe { helios_virtio::vsock_from_mmio(header, candidate.size) }.unwrap_or_else(|error| {
            panic!("failed to initialize virtio-vsock device at {base:#x}: {error}")
        });
    Some((
        VirtioVsockDevice {
            inner: Arc::new(device),
        },
        source,
    ))
}
