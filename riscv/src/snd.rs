//! virtio-snd over the platform MMIO bus for the RISC-V backend.
//!
//! Concurrency contract: the device is discovered and programmed on the
//! bootstrap hart before external interrupts are unmasked. Afterwards
//! its completions and its event reports arrive on the PLIC source the
//! device tree names, delivered to that same hart, which is why the
//! kernel task that drains its event ring is local to it.

extern crate alloc;

use alloc::sync::Arc;
use core::num::NonZeroU32;

use fdt::Fdt;
use helios_kernel::ExternalInterruptHandler;

use crate::net::InterruptSourceId;

/// The interrupt route's view of the machine's sound device.
///
/// The kernel's event drain holds the same `Arc`, which satisfies the
/// playback contract through hal's shared-handle impl; this newtype
/// exists only to give the route an interrupt handler to dispatch to.
#[derive(Clone)]
pub(crate) struct VirtioSoundDevice {
    inner: Arc<helios_virtio::VirtioMmioSndDevice>,
}

/// The sound device the bootstrap hart brought up, together with the
/// PLIC source the device tree routes it to.
pub(crate) struct SoundInterrupt {
    pub(crate) source: InterruptSourceId,
    pub(crate) device: VirtioSoundDevice,
}

impl ExternalInterruptHandler for VirtioSoundDevice {
    fn handle_interrupt(&self) {
        self.inner.handle_interrupt();
    }
}

pub(crate) fn has_sound_device(fdt: &Fdt<'_>) -> bool {
    crate::count_virtio_mmio_devices(fdt, helios_virtio::DeviceType::Sound) != 0
}

/// Brings the platform's sound device up and hands it to the kernel.
pub(crate) fn install<WatchdogImpl>(
    kernel: &helios_kernel::Kernel<crate::RiscvCpu, WatchdogImpl>,
    fdt: &Fdt<'_>,
) -> Option<SoundInterrupt>
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let (device, source) = discover_sound_device(fdt)?;
    helios_kernel::install_sound_device(kernel, device.inner.clone());
    Some(SoundInterrupt { source, device })
}

fn discover_sound_device(fdt: &Fdt<'_>) -> Option<(VirtioSoundDevice, InterruptSourceId)> {
    let candidate = helios_virtio::mmio_candidates(fdt).find(|candidate| {
        crate::matches_virtio_mmio_device(candidate.base, helios_virtio::DeviceType::Sound)
    })?;
    let base = candidate.base;
    let header = core::ptr::NonNull::new(base as *mut u8)
        .unwrap_or_else(|| panic!("virtio MMIO base {base:#x} was unexpectedly null"));
    let source = candidate
        .interrupt
        .and_then(|interrupt| NonZeroU32::new(interrupt.number))
        .map(InterruptSourceId)
        .unwrap_or_else(|| panic!("virtio-snd node at {base:#x} has no valid interrupt source"));
    let device =
        unsafe { helios_virtio::snd_from_mmio(header, candidate.size) }.unwrap_or_else(|error| {
            panic!("failed to initialize virtio-snd device at {base:#x}: {error}")
        });
    Some((
        VirtioSoundDevice {
            inner: Arc::new(device),
        },
        source,
    ))
}
