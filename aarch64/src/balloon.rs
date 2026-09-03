//! virtio-balloon over the platform MMIO bus for the AArch64 backend.
//!
//! Concurrency contract: the device is discovered and programmed on the
//! bootstrap processor before IRQs are unmasked. Afterwards the kernel's
//! balloon tasks are its only users, and both its completions and its
//! configuration changes arrive on the SPI the device tree names.

extern crate alloc;

use crate::platform::PlatformDescription;
use alloc::sync::Arc;
use arm_gic::{IntId, Trigger};
use helios_kernel::{BalloonHandle, ExternalInterruptHandler, Kernel};

type Aarch64VirtioBalloonDevice = helios_virtio::VirtioBalloonDevice<
    helios_virtio::VirtioMmioTransport<helios_virtio::MmioBus<helios_virtio::OffsetDmaPool>>,
>;

/// The interrupt half of the balloon: what the GIC dispatches to.
///
/// The kernel's balloon tasks hold the device itself, so this carries
/// nothing but the same handle.
#[derive(Clone)]
pub(crate) struct VirtioBalloonInterrupt {
    device: Arc<Aarch64VirtioBalloonDevice>,
}

impl ExternalInterruptHandler for VirtioBalloonInterrupt {
    fn handle_interrupt(&self) {
        self.device.handle_interrupt();
    }
}

/// The balloon the bootstrap processor brought up, together with the
/// interrupt the device tree routes it to.
pub(crate) struct BalloonInterrupt {
    pub(crate) interrupt: IntId,
    pub(crate) trigger: Trigger,
    pub(crate) handler: VirtioBalloonInterrupt,
    pub(crate) handle: BalloonHandle,
}

pub(crate) fn has_balloon_device(platform: &PlatformDescription) -> bool {
    crate::count_virtio_mmio_devices(platform, helios_virtio::DeviceType::MemoryBalloon) != 0
}

pub(crate) fn install<WatchdogImpl>(
    kernel: &Kernel<crate::Aarch64Cpu, WatchdogImpl>,
    platform: &PlatformDescription,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
) -> Option<BalloonInterrupt>
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let candidate = crate::virtio_slots(
        platform,
        physical_memory_offset,
        handoff,
        helios_virtio::DeviceType::MemoryBalloon,
    )
    .next()?;
    let (interrupt, trigger) = (candidate.interrupt.intid(), candidate.interrupt.trigger);
    assert!(
        candidate.region.size != 0,
        "AArch64 virtio-balloon node has zero MMIO size"
    );
    crate::map_mmio_page(candidate.region.base, physical_memory_offset, handoff);
    let virtual_base = crate::mmio_virtual_base(candidate.region.base, physical_memory_offset);
    let header = core::ptr::NonNull::new(virtual_base as *mut u8)
        .unwrap_or_else(|| panic!("virtio MMIO base {virtual_base:#x} was unexpectedly null"));
    let dma = helios_virtio::OffsetDmaPool::new(physical_memory_offset);
    let device =
        unsafe { helios_virtio::balloon_from_mmio_with_dma(header, candidate.region.size, dma) }
            .unwrap_or_else(|error| {
                panic!(
                    "failed to initialize virtio-balloon device at {:#x}: {error}",
                    candidate.region.base
                )
            });
    let device = Arc::new(device);
    let handle = helios_kernel::install_memory_balloon(kernel, device.clone());
    tracing::info!("virtio balloon online interrupt={interrupt:?}");
    Some(BalloonInterrupt {
        interrupt,
        trigger,
        handler: VirtioBalloonInterrupt { device },
        handle,
    })
}
