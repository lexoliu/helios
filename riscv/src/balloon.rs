//! virtio-balloon over the platform MMIO bus for the RISC-V backend.
//!
//! Concurrency contract: the device is discovered and programmed on the
//! bootstrap hart before external interrupts are unmasked. Afterwards
//! the kernel's balloon tasks are its only users, and both its
//! completions and its configuration changes arrive on the PLIC source
//! the device tree names.

extern crate alloc;

use alloc::sync::Arc;
use core::num::NonZeroU32;

use fdt::Fdt;
use helios_kernel::{BalloonHandle, ExternalInterruptHandler};

use crate::net::InterruptSourceId;

/// The interrupt half of the balloon: what the PLIC dispatches to.
///
/// The kernel's balloon tasks hold the device itself, so this carries
/// nothing but the same handle.
#[derive(Clone)]
pub(crate) struct VirtioBalloonInterrupt {
    inner: Arc<helios_virtio::VirtioMmioBalloonDevice>,
}

impl ExternalInterruptHandler for VirtioBalloonInterrupt {
    fn handle_interrupt(&self) {
        self.inner.handle_interrupt();
    }
}

/// The balloon the bootstrap hart brought up, together with the PLIC
/// source the device tree routes it to.
pub(crate) struct BalloonInterrupt {
    pub(crate) source: InterruptSourceId,
    pub(crate) handler: VirtioBalloonInterrupt,
    pub(crate) handle: BalloonHandle,
}

pub(crate) fn has_balloon_device(fdt: &Fdt<'_>) -> bool {
    crate::count_virtio_mmio_devices(fdt, helios_virtio::DeviceType::MemoryBalloon) != 0
}

/// Brings up the platform's memory balloon and hands it to the kernel's
/// balloon tasks.
pub(crate) fn install<WatchdogImpl>(
    kernel: &helios_kernel::Kernel<crate::RiscvCpu, WatchdogImpl>,
    fdt: &Fdt<'_>,
) -> Option<BalloonInterrupt>
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let candidate = helios_virtio::mmio_candidates(fdt).find(|candidate| {
        crate::matches_virtio_mmio_device(candidate.base, helios_virtio::DeviceType::MemoryBalloon)
    })?;
    let base = candidate.base;
    let header = core::ptr::NonNull::new(base as *mut u8)
        .unwrap_or_else(|| panic!("virtio MMIO base {base:#x} was unexpectedly null"));
    let source = candidate
        .interrupt
        .and_then(|interrupt| NonZeroU32::new(interrupt.number))
        .map(InterruptSourceId)
        .unwrap_or_else(|| {
            panic!("virtio-balloon node at {base:#x} has no valid interrupt source")
        });
    let device = unsafe { helios_virtio::balloon_from_mmio(header, candidate.size) }
        .unwrap_or_else(|error| {
            panic!("failed to initialize virtio-balloon device at {base:#x}: {error}")
        });
    let device = Arc::new(device);
    let handle = helios_kernel::install_memory_balloon(kernel, device.clone());
    tracing::info!("virtio balloon online irq={}", source.0.get());
    Some(BalloonInterrupt {
        source,
        handler: VirtioBalloonInterrupt { inner: device },
        handle,
    })
}
