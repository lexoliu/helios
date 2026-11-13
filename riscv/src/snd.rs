//! virtio-snd over the platform MMIO bus for the RISC-V backend.
//!
//! Concurrency contract: the device is discovered and programmed on the
//! bootstrap hart before external interrupts are unmasked. Afterwards
//! its completions and its event reports arrive on the PLIC line the
//! device tree names, on the hart that takes that PLIC's interrupts.
//! The kernel task that drains its event ring stays local to the
//! bootstrap hart, which spawned it; the handler's notification wakes
//! it from whichever hart that is.

extern crate alloc;

use alloc::sync::Arc;

use fdt::Fdt;
use helios_kernel::ExternalInterruptHandler;

use crate::net::PlicInterrupt;

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
/// PLIC line the device tree routes it to.
pub(crate) struct SoundInterrupt {
    pub(crate) line: PlicInterrupt,
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
    debug_state: &crate::debug_state::RuntimeState,
) -> Option<SoundInterrupt>
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let (device, line) = discover_sound_device(fdt)?;
    let service = helios_kernel::install_audio_device(kernel, device.inner.clone());
    debug_state.install_audio_service(service);
    Some(SoundInterrupt { line, device })
}

fn discover_sound_device(fdt: &Fdt<'_>) -> Option<(VirtioSoundDevice, PlicInterrupt)> {
    let candidate = helios_virtio::mmio_candidates(fdt).find(|candidate| {
        crate::matches_virtio_mmio_device(candidate.base, helios_virtio::DeviceType::Sound)
    })?;
    let base = candidate.base;
    let header = core::ptr::NonNull::new(base as *mut u8)
        .unwrap_or_else(|| panic!("virtio MMIO base {base:#x} was unexpectedly null"));
    let line = candidate
        .interrupt
        .and_then(PlicInterrupt::from_specifier)
        .unwrap_or_else(|| panic!("virtio-snd node at {base:#x} has no valid interrupt source"));
    let device =
        unsafe { helios_virtio::snd_from_mmio(header, candidate.size) }.unwrap_or_else(|error| {
            panic!("failed to initialize virtio-snd device at {base:#x}: {error}")
        });
    Some((
        VirtioSoundDevice {
            inner: Arc::new(device),
        },
        line,
    ))
}
