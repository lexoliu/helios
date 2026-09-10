//! virtio-input over the platform MMIO bus for the AArch64 backend.
//!
//! A machine with a desktop presents several input devices at once — a
//! keyboard, a relative pointer, an absolute tablet — so every slot the
//! platform describes that answers as one is brought up here and handed
//! to the kernel.
//!
//! Concurrency contract: the devices are discovered and programmed on
//! the bootstrap processor before IRQs are unmasked. Afterwards each
//! reports on the SPI the firmware names, delivered to that same
//! processor, which is why the kernel task that drains a device is
//! local to it.

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;

use arm_gic::{IntId, Trigger};

use crate::platform::PlatformDescription;
use helios_kernel::ExternalInterruptHandler;

type Aarch64VirtioInputDevice = helios_virtio::VirtioInputDevice<
    helios_virtio::VirtioMmioTransport<helios_virtio::MmioBus<helios_virtio::OffsetDmaPool>>,
>;

/// The interrupt route's view of one input device.
///
/// The kernel's reader holds the same `Arc`, which satisfies the input
/// contract through hal's shared-handle impl; this newtype exists only
/// to give the route an interrupt handler to dispatch to.
#[derive(Clone)]
pub(crate) struct VirtioInputDevice {
    inner: Arc<Aarch64VirtioInputDevice>,
}

/// One input device the bootstrap processor brought up, together with
/// the interrupt the firmware routes it to.
pub(crate) struct InputInterrupt {
    pub(crate) interrupt: IntId,
    pub(crate) trigger: Trigger,
    pub(crate) device: VirtioInputDevice,
}

impl ExternalInterruptHandler for VirtioInputDevice {
    fn handle_interrupt(&self) {
        self.inner.handle_interrupt();
    }
}

pub(crate) fn count_input_devices(platform: &PlatformDescription) -> usize {
    crate::count_virtio_mmio_devices(platform, helios_virtio::DeviceType::Input)
}

/// Brings up every input device on the platform bus, hands them all to
/// the kernel, and publishes the service `helios:system/input` is served
/// from.
///
/// One call for every device rather than one per device: the kernel owns
/// the machine's input devices as a set, because that is what a program
/// asking "what can I read?" is answered from.
pub(crate) fn install<WatchdogImpl>(
    kernel: &helios_kernel::Kernel<crate::Aarch64Cpu, WatchdogImpl>,
    platform: &PlatformDescription,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
    debug_state: &crate::debug_state::RuntimeState,
) -> Vec<InputInterrupt>
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let discovered = discover_input_devices(platform, physical_memory_offset, handoff);
    let service = helios_kernel::install_input_devices(
        kernel,
        discovered
            .iter()
            .map(|installed| installed.device.inner.clone()),
    );
    debug_state.install_input_service(service);
    discovered
}

fn discover_input_devices(
    platform: &PlatformDescription,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
) -> Vec<InputInterrupt> {
    crate::virtio_slots(
        platform,
        physical_memory_offset,
        handoff,
        helios_virtio::DeviceType::Input,
    )
    .map(|candidate| {
        let (interrupt, trigger) = (candidate.interrupt.intid(), candidate.interrupt.trigger);
        assert!(
            candidate.region.size != 0,
            "AArch64 virtio-input node has zero MMIO size"
        );
        crate::map_mmio_page(candidate.region.base, physical_memory_offset, handoff);
        let virtual_base = crate::mmio_virtual_base(candidate.region.base, physical_memory_offset);
        let header = core::ptr::NonNull::new(virtual_base as *mut u8)
            .unwrap_or_else(|| panic!("virtio MMIO base {virtual_base:#x} was unexpectedly null"));
        let dma = helios_virtio::OffsetDmaPool::new(physical_memory_offset);
        let device =
            unsafe { helios_virtio::input_from_mmio_with_dma(header, candidate.region.size, dma) }
                .unwrap_or_else(|error| {
                    panic!(
                        "failed to initialize virtio-input device at {:#x}: {error}",
                        candidate.region.base
                    )
                });
        InputInterrupt {
            interrupt,
            trigger,
            device: VirtioInputDevice {
                inner: Arc::new(device),
            },
        }
    })
    .collect()
}
