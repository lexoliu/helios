//! virtio-entropy over the platform MMIO bus for the AArch64 backend.
//!
//! Concurrency contract: the device is discovered and programmed on the
//! bootstrap processor before IRQs are unmasked. Afterwards the kernel's
//! reseed task is its only reader, and completions arrive on the SPI the
//! device tree names, so the reader parks on a notification rather than
//! polling the used ring.

extern crate alloc;

use core::future::Future;

use crate::platform::PlatformDescription;
use arm_gic::{IntId, Trigger};
use helios_hal::io::IoError;
use helios_kernel::{ExternalInterruptHandler, HardwareEntropySource, RootEntropyHandle};
use triomphe::Arc;

type Aarch64VirtioRngDevice = helios_virtio::VirtioRngDevice<
    helios_virtio::VirtioMmioTransport<helios_virtio::MmioBus<helios_virtio::OffsetDmaPool>>,
>;

#[derive(Clone)]
pub(crate) struct VirtioEntropyDevice {
    device: Arc<Aarch64VirtioRngDevice>,
}

/// The entropy device the bootstrap processor brought up, together with
/// the interrupt the device tree routes it to.
pub(crate) struct EntropyInterrupt {
    pub(crate) interrupt: IntId,
    pub(crate) trigger: Trigger,
    pub(crate) device: VirtioEntropyDevice,
}

impl ExternalInterruptHandler for VirtioEntropyDevice {
    fn handle_interrupt(&self) {
        self.device.handle_interrupt();
    }
}

impl HardwareEntropySource for VirtioEntropyDevice {
    fn fill<'a>(
        &'a self,
        buffer: &'a mut [u8],
    ) -> impl Future<Output = Result<(), IoError>> + Send + 'a {
        self.device.fill(buffer)
    }
}

pub(crate) fn has_entropy_device(platform: &PlatformDescription) -> bool {
    crate::count_virtio_mmio_devices(platform, helios_virtio::DeviceType::Entropy) != 0
}

pub(crate) fn install<WatchdogImpl>(
    kernel: &helios_kernel::Kernel<crate::Aarch64Cpu, WatchdogImpl>,
    platform: &PlatformDescription,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
    root: RootEntropyHandle,
) -> Option<EntropyInterrupt>
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let Some(entropy) = discover_entropy_device(platform, physical_memory_offset, handoff) else {
        tracing::warn!("virtio entropy device was not discovered on the platform bus");
        return None;
    };
    helios_kernel::install_entropy_device(kernel, root, entropy.device.clone());
    tracing::info!("virtio entropy online interrupt={:?}", entropy.interrupt);
    Some(entropy)
}

fn discover_entropy_device(
    platform: &PlatformDescription,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
) -> Option<EntropyInterrupt> {
    let candidate = crate::virtio_slots(
        platform,
        physical_memory_offset,
        handoff,
        helios_virtio::DeviceType::Entropy,
    )
    .next()?;
    let (interrupt, trigger) = (candidate.interrupt.intid(), candidate.interrupt.trigger);
    assert!(
        candidate.region.size != 0,
        "AArch64 virtio-rng node has zero MMIO size"
    );
    crate::map_mmio_page(candidate.region.base, physical_memory_offset, handoff);
    let virtual_base = crate::mmio_virtual_base(candidate.region.base, physical_memory_offset);
    let header = core::ptr::NonNull::new(virtual_base as *mut u8)
        .unwrap_or_else(|| panic!("virtio MMIO base {virtual_base:#x} was unexpectedly null"));
    let dma = helios_virtio::OffsetDmaPool::new(physical_memory_offset);
    let device =
        unsafe { helios_virtio::rng_from_mmio_with_dma(header, candidate.region.size, dma) }
            .unwrap_or_else(|error| {
                panic!(
                    "failed to initialize virtio-rng device at {:#x}: {error}",
                    candidate.region.base
                )
            });
    Some(EntropyInterrupt {
        interrupt,
        trigger,
        device: VirtioEntropyDevice {
            device: Arc::new(device),
        },
    })
}
