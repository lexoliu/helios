//! virtio-vsock over the platform MMIO bus for the AArch64 backend.
//!
//! Concurrency contract: the device is discovered and programmed on the
//! bootstrap processor before IRQs are unmasked. Afterwards the kernel's
//! vsock service owns it — one receive pump plus whichever tasks are
//! transmitting — and completions arrive on the SPI the device tree
//! names, so every waiter parks on a notification rather than polling.

extern crate alloc;

use alloc::sync::Arc;

use crate::platform::PlatformDescription;
use arm_gic::{IntId, Trigger};
use helios_kernel::ExternalInterruptHandler;

type Aarch64VirtioVsockDevice = helios_virtio::VirtioVsockDevice<
    helios_virtio::VirtioMmioTransport<helios_virtio::MmioBus<helios_virtio::OffsetDmaPool>>,
>;

/// The interrupt route's view of the platform's vsock device.
///
/// The kernel's vsock service holds the same `Arc`, which satisfies the
/// device contract through hal's shared-handle impl; this newtype exists
/// only to give the route an interrupt handler to dispatch to.
#[derive(Clone)]
pub(crate) struct VirtioVsockDevice {
    device: Arc<Aarch64VirtioVsockDevice>,
}

/// The vsock device the bootstrap processor brought up, together with
/// the interrupt the device tree routes it to.
pub(crate) struct VsockInterrupt {
    pub(crate) interrupt: IntId,
    pub(crate) trigger: Trigger,
    pub(crate) device: VirtioVsockDevice,
}

impl ExternalInterruptHandler for VirtioVsockDevice {
    fn handle_interrupt(&self) {
        self.device.handle_interrupt();
    }
}

pub(crate) fn has_vsock_device(platform: &PlatformDescription) -> bool {
    crate::count_virtio_mmio_devices(platform, helios_virtio::DeviceType::Vsock) != 0
}

/// Brings up the vsock device and publishes it as the machine's host
/// link.
pub(crate) fn install<WatchdogImpl>(
    kernel: &helios_kernel::Kernel<crate::Aarch64Cpu, WatchdogImpl>,
    cpu: &crate::Aarch64Cpu,
    platform: &PlatformDescription,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
    debug_state: &crate::debug_state::RuntimeState,
) -> Option<VsockInterrupt>
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let vsock = discover_vsock_device(platform, physical_memory_offset, handoff)?;
    let service = helios_kernel::install_vsock_device(kernel, cpu, vsock.device.device.clone());
    let guest_cid = service.guest_cid();
    debug_state.install_vsock_service(helios_kernel::ComponentHostVsockService::from_service(
        service,
    ));
    tracing::info!(
        guest_cid,
        interrupt = ?vsock.interrupt,
        "virtio vsock online"
    );
    Some(vsock)
}

fn discover_vsock_device(
    platform: &PlatformDescription,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
) -> Option<VsockInterrupt> {
    let candidate = crate::virtio_slots(
        platform,
        physical_memory_offset,
        handoff,
        helios_virtio::DeviceType::Vsock,
    )
    .next()?;
    let (interrupt, trigger) = (candidate.interrupt.intid(), candidate.interrupt.trigger);
    assert!(
        candidate.region.size != 0,
        "AArch64 virtio-vsock node has zero MMIO size"
    );
    crate::map_mmio_page(candidate.region.base, physical_memory_offset, handoff);
    let virtual_base = crate::mmio_virtual_base(candidate.region.base, physical_memory_offset);
    let header = core::ptr::NonNull::new(virtual_base as *mut u8)
        .unwrap_or_else(|| panic!("virtio MMIO base {virtual_base:#x} was unexpectedly null"));
    let dma = helios_virtio::OffsetDmaPool::new(physical_memory_offset);
    let device =
        unsafe { helios_virtio::vsock_from_mmio_with_dma(header, candidate.region.size, dma) }
            .unwrap_or_else(|error| {
                panic!(
                    "failed to initialize virtio-vsock device at {:#x}: {error}",
                    candidate.region.base
                )
            });
    Some(VsockInterrupt {
        interrupt,
        trigger,
        device: VirtioVsockDevice {
            device: Arc::new(device),
        },
    })
}
