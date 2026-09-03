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

    fn drain_completions(&self) {
        self.inner.handle_interrupt();
    }
}

pub(crate) fn has_entropy_device(fdt: &Fdt<'_>) -> bool {
    crate::count_virtio_mmio_devices(fdt, helios_virtio::DeviceType::Entropy) != 0
}

/// Brings the platform's entropy device up.
///
/// Separate from [`install`] and called earlier, because the root DRBG
/// is seeded from a read of this device: riscv64 has no entropy
/// instruction the kernel can rely on, so a platform whose firmware
/// leaves no `/chosen/rng-seed` has this device and nothing else. The
/// reseed task that keeps reading it needs the seeded root and so comes
/// after.
pub(crate) fn bring_up(fdt: &Fdt<'_>) -> Option<EntropyInterrupt> {
    let Some((device, source)) = discover_entropy_device(fdt) else {
        tracing::warn!("virtio entropy device was not discovered on the platform bus");
        return None;
    };
    Some(EntropyInterrupt { source, device })
}

/// Hands the device to the kernel's reseed task.
pub(crate) fn install<WatchdogImpl>(
    kernel: &helios_kernel::Kernel<crate::RiscvCpu, WatchdogImpl>,
    entropy: &EntropyInterrupt,
    root: helios_kernel::RootEntropyHandle,
) where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    helios_kernel::install_entropy_device(kernel, root, entropy.device.clone());
    tracing::info!("virtio entropy online irq={}", entropy.source.0.get());
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
