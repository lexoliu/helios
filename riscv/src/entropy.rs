//! virtio-entropy over the platform MMIO bus for the RISC-V backend.
//!
//! Concurrency contract: the device is discovered and programmed on the
//! bootstrap hart before external interrupts are unmasked. Afterwards
//! the kernel's reseed task is its only reader and completions arrive on
//! the PLIC source the device tree names, so the reader parks on a
//! notification rather than polling the used ring.

extern crate alloc;

use alloc::sync::Arc;
use core::future::Future;
use core::num::NonZeroU32;

use fdt::Fdt;
use helios_hal::io::IoError;
use helios_kernel::{ExternalInterruptHandler, HardwareEntropySource};

use crate::net::InterruptSourceId;

#[derive(Clone)]
pub(crate) struct VirtioEntropyDevice {
    inner: Arc<helios_virtio::VirtioMmioRngDevice>,
}

/// The entropy device the bootstrap hart brought up, together with the
/// PLIC source the device tree routes it to.
pub(crate) struct EntropyInterrupt {
    pub(crate) source: InterruptSourceId,
    pub(crate) device: VirtioEntropyDevice,
}

impl ExternalInterruptHandler for VirtioEntropyDevice {
    fn handle_interrupt(&self) {
        self.inner.handle_interrupt();
    }
}

impl HardwareEntropySource for VirtioEntropyDevice {
    fn fill<'a>(
        &'a self,
        buffer: &'a mut [u8],
    ) -> impl Future<Output = Result<(), IoError>> + Send + 'a {
        self.inner.fill(buffer)
    }
}

pub(crate) fn has_entropy_device(fdt: &Fdt<'_>) -> bool {
    crate::count_virtio_mmio_devices(fdt, helios_virtio::DeviceType::Entropy) != 0
}

/// Brings up the platform's entropy device and hands it to the kernel's
/// reseed task.
pub(crate) fn install<WatchdogImpl>(
    kernel: &helios_kernel::Kernel<crate::RiscvCpu, WatchdogImpl>,
    fdt: &Fdt<'_>,
    root: helios_kernel::RootEntropyHandle,
) -> Option<EntropyInterrupt>
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let Some((device, source)) = discover_entropy_device(fdt) else {
        tracing::warn!("virtio entropy device was not discovered on the platform bus");
        return None;
    };
    helios_kernel::install_entropy_device(kernel, root, device.clone());
    tracing::info!("virtio entropy online irq={}", source.0.get());
    Some(EntropyInterrupt { source, device })
}

fn discover_entropy_device(fdt: &Fdt<'_>) -> Option<(VirtioEntropyDevice, InterruptSourceId)> {
    let candidate = helios_virtio::mmio_candidates(fdt).find(|candidate| {
        crate::matches_virtio_mmio_device(candidate.base, helios_virtio::DeviceType::Entropy)
    })?;
    let base = candidate.base;
    let header = core::ptr::NonNull::new(base as *mut u8)
        .unwrap_or_else(|| panic!("virtio MMIO base {base:#x} was unexpectedly null"));
    let source = candidate
        .interrupt
        .and_then(|interrupt| NonZeroU32::new(interrupt.number))
        .map(InterruptSourceId)
        .unwrap_or_else(|| panic!("virtio-rng node at {base:#x} has no valid interrupt source"));
    let device =
        unsafe { helios_virtio::rng_from_mmio(header, candidate.size) }.unwrap_or_else(|error| {
            panic!("failed to initialize virtio-rng device at {base:#x}: {error}")
        });
    Some((
        VirtioEntropyDevice {
            inner: Arc::new(device),
        },
        source,
    ))
}
