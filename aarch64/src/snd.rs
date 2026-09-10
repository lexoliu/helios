//! virtio-snd over the platform MMIO bus for the AArch64 backend.
//!
//! Concurrency contract: the device is discovered and programmed on the
//! bootstrap processor before IRQs are unmasked. Afterwards its
//! completions and its event reports arrive on the SPI the firmware
//! names, delivered to that same processor, which is why the kernel task
//! that drains its event ring is local to it.

extern crate alloc;

use alloc::sync::Arc;

use arm_gic::{IntId, Trigger};

use crate::platform::PlatformDescription;
use helios_kernel::ExternalInterruptHandler;

type Aarch64VirtioSndDevice = helios_virtio::VirtioSndDevice<
    helios_virtio::VirtioMmioTransport<helios_virtio::MmioBus<helios_virtio::OffsetDmaPool>>,
>;

/// The interrupt route's view of the machine's sound device.
///
/// The kernel's event drain holds the same `Arc`, which satisfies the
/// playback contract through hal's shared-handle impl; this newtype
/// exists only to give the route an interrupt handler to dispatch to.
#[derive(Clone)]
pub(crate) struct VirtioSoundDevice {
    device: Arc<Aarch64VirtioSndDevice>,
}

/// The sound device the bootstrap processor brought up, together with
/// the interrupt the firmware routes it to.
pub(crate) struct SoundInterrupt {
    pub(crate) interrupt: IntId,
    pub(crate) trigger: Trigger,
    pub(crate) device: VirtioSoundDevice,
}

impl ExternalInterruptHandler for VirtioSoundDevice {
    fn handle_interrupt(&self) {
        self.device.handle_interrupt();
    }
}

pub(crate) fn has_sound_device(platform: &PlatformDescription) -> bool {
    crate::count_virtio_mmio_devices(platform, helios_virtio::DeviceType::Sound) != 0
}

/// Brings the platform's sound device up and hands it to the kernel.
pub(crate) fn install<WatchdogImpl>(
    kernel: &helios_kernel::Kernel<crate::Aarch64Cpu, WatchdogImpl>,
    platform: &PlatformDescription,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
) -> Option<SoundInterrupt>
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let sound = discover_sound_device(platform, physical_memory_offset, handoff)?;
    helios_kernel::install_sound_device(kernel, sound.device.device.clone());
    Some(sound)
}

fn discover_sound_device(
    platform: &PlatformDescription,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
) -> Option<SoundInterrupt> {
    let candidate = crate::virtio_slots(
        platform,
        physical_memory_offset,
        handoff,
        helios_virtio::DeviceType::Sound,
    )
    .next()?;
    let (interrupt, trigger) = (candidate.interrupt.intid(), candidate.interrupt.trigger);
    assert!(
        candidate.region.size != 0,
        "AArch64 virtio-snd node has zero MMIO size"
    );
    crate::map_mmio_page(candidate.region.base, physical_memory_offset, handoff);
    let virtual_base = crate::mmio_virtual_base(candidate.region.base, physical_memory_offset);
    let header = core::ptr::NonNull::new(virtual_base as *mut u8)
        .unwrap_or_else(|| panic!("virtio MMIO base {virtual_base:#x} was unexpectedly null"));
    let dma = helios_virtio::OffsetDmaPool::new(physical_memory_offset);
    let device =
        unsafe { helios_virtio::snd_from_mmio_with_dma(header, candidate.region.size, dma) }
            .unwrap_or_else(|error| {
                panic!(
                    "failed to initialize virtio-snd device at {:#x}: {error}",
                    candidate.region.base
                )
            });
    Some(SoundInterrupt {
        interrupt,
        trigger,
        device: VirtioSoundDevice {
            device: Arc::new(device),
        },
    })
}
