//! virtio-gpu over the platform MMIO bus for the AArch64 backend.
//!
//! Concurrency contract: the device is discovered and programmed on the
//! bootstrap processor before IRQs are unmasked. Afterwards its
//! completions and its display-change announcements arrive on the SPI
//! the firmware names, delivered to that same processor, which is why
//! the kernel task that follows the topology is local to it.

extern crate alloc;

use alloc::sync::Arc;

use arm_gic::{IntId, Trigger};

use crate::platform::PlatformDescription;
use helios_kernel::ExternalInterruptHandler;

type Aarch64VirtioGpuDevice = helios_virtio::VirtioGpuDevice<
    helios_virtio::VirtioMmioTransport<helios_virtio::MmioBus<helios_virtio::OffsetDmaPool>>,
>;

/// The interrupt route's view of the platform's display engine.
///
/// The kernel's topology task holds the same `Arc`, which satisfies the
/// display contract through hal's shared-handle impl; this newtype
/// exists only to give the route an interrupt handler to dispatch to.
#[derive(Clone)]
pub(crate) struct VirtioDisplayDevice {
    device: Arc<Aarch64VirtioGpuDevice>,
}

/// The display device the bootstrap processor brought up, together with
/// the interrupt the firmware routes it to.
pub(crate) struct DisplayInterrupt {
    pub(crate) interrupt: IntId,
    pub(crate) trigger: Trigger,
    pub(crate) device: VirtioDisplayDevice,
}

impl ExternalInterruptHandler for VirtioDisplayDevice {
    fn handle_interrupt(&self) {
        self.device.handle_interrupt();
    }
}

pub(crate) fn has_display_device(platform: &PlatformDescription) -> bool {
    crate::count_virtio_mmio_devices(platform, helios_virtio::DeviceType::Gpu) != 0
}

/// Brings the platform's display engine up and hands it to the kernel.
pub(crate) fn install<WatchdogImpl>(
    cpu: &crate::Aarch64Cpu,
    kernel: &helios_kernel::Kernel<crate::Aarch64Cpu, WatchdogImpl>,
    platform: &PlatformDescription,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
    debug_state: &crate::debug_state::RuntimeState,
) -> Option<DisplayInterrupt>
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let display = discover_display_device(platform, physical_memory_offset, handoff)?;
    let service = helios_kernel::install_display_device(kernel, cpu, display.device.device.clone());
    debug_state.install_display_service(service);
    Some(display)
}

fn discover_display_device(
    platform: &PlatformDescription,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
) -> Option<DisplayInterrupt> {
    let candidate = crate::virtio_slots(
        platform,
        physical_memory_offset,
        handoff,
        helios_virtio::DeviceType::Gpu,
    )
    .next()?;
    let (interrupt, trigger) = (candidate.interrupt.intid(), candidate.interrupt.trigger);
    assert!(
        candidate.region.size != 0,
        "AArch64 virtio-gpu node has zero MMIO size"
    );
    crate::map_mmio_page(candidate.region.base, physical_memory_offset, handoff);
    let virtual_base = crate::mmio_virtual_base(candidate.region.base, physical_memory_offset);
    let header = core::ptr::NonNull::new(virtual_base as *mut u8)
        .unwrap_or_else(|| panic!("virtio MMIO base {virtual_base:#x} was unexpectedly null"));
    let dma = helios_virtio::OffsetDmaPool::new(physical_memory_offset);
    let device =
        unsafe { helios_virtio::gpu_from_mmio_with_dma(header, candidate.region.size, dma) }
            .unwrap_or_else(|error| {
                panic!(
                    "failed to initialize virtio-gpu device at {:#x}: {error}",
                    candidate.region.base
                )
            });
    Some(DisplayInterrupt {
        interrupt,
        trigger,
        device: VirtioDisplayDevice {
            device: Arc::new(device),
        },
    })
}
