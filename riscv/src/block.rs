//! virtio-blk over the platform MMIO bus for the RISC-V backend.
//!
//! Every block device the device tree names is brought up here; which of
//! them the kernel ends up owning is the kernel's decision, taken from
//! the serials the devices report.
//!
//! Concurrency contract: the devices are discovered and programmed on
//! the bootstrap hart before external interrupts are unmasked, and their
//! completions arrive on the PLIC sources the device tree names, so
//! every reader parks on a notification rather than polling a used ring.

extern crate alloc;

use alloc::vec::Vec;
use core::num::NonZeroU32;

use fdt::Fdt;
use helios_hal::fs::BlockDeviceRights;
use helios_kernel::ExternalInterruptHandler;

use crate::RiscvCpu;
use crate::debug_state::RuntimeState;
use crate::net::InterruptSourceId;

type RiscvVirtioBlockDevice = helios_virtio::VirtioMmioBlockDevice<RiscvCpu>;

#[derive(Clone)]
pub(crate) struct VirtioBlockDevice {
    inner: RiscvVirtioBlockDevice,
}

/// One block device the bootstrap hart brought up, together with the
/// PLIC source the device tree routes it to.
pub(crate) struct BlockInterrupt {
    pub(crate) source: InterruptSourceId,
    pub(crate) device: VirtioBlockDevice,
}

impl ExternalInterruptHandler for VirtioBlockDevice {
    fn handle_interrupt(&self) {
        self.inner.handle_interrupt();
    }
}

pub(crate) fn count_block_devices(fdt: &Fdt<'_>) -> usize {
    crate::count_virtio_mmio_devices(fdt, helios_virtio::DeviceType::Block)
}

/// Brings up every block device on the platform bus and hands them to
/// the kernel, which keeps the one the platform named as its own.
pub(crate) fn install<WatchdogImpl>(
    cpu: &RiscvCpu,
    kernel: &helios_kernel::Kernel<RiscvCpu, WatchdogImpl>,
    fdt: &Fdt<'_>,
    debug_state: &RuntimeState,
    root: helios_kernel::RootEntropyHandle,
) -> Vec<BlockInterrupt>
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let discovered = discover_block_devices(cpu, fdt);
    let devices = discovered
        .iter()
        .map(|interrupt| interrupt.device.inner.clone())
        .collect();
    let state = debug_state.clone();
    helios_kernel::install_block_devices(kernel, root, devices, move |service| {
        state.install_block_service(service);
    });
    for interrupt in &discovered {
        tracing::info!(irq = interrupt.source.0.get(), "virtio block online");
    }
    discovered
}

fn discover_block_devices(cpu: &RiscvCpu, fdt: &Fdt<'_>) -> Vec<BlockInterrupt> {
    helios_virtio::mmio_candidates(fdt)
        .filter(|candidate| {
            crate::matches_virtio_mmio_device(candidate.base, helios_virtio::DeviceType::Block)
        })
        .map(|candidate| {
            let base = candidate.base;
            let header = core::ptr::NonNull::new(base as *mut u8)
                .unwrap_or_else(|| panic!("virtio MMIO base {base:#x} was unexpectedly null"));
            let source = candidate
                .interrupt
                .and_then(|interrupt| NonZeroU32::new(interrupt.number))
                .map(InterruptSourceId)
                .unwrap_or_else(|| {
                    panic!("virtio-blk node at {base:#x} has no valid interrupt source")
                });
            let device = unsafe {
                helios_virtio::block_from_mmio(
                    header,
                    candidate.size,
                    cpu.clone(),
                    BlockDeviceRights::READ | BlockDeviceRights::WRITE,
                )
            }
            .unwrap_or_else(|error| {
                panic!("failed to initialize virtio-blk device at {base:#x}: {error}")
            });
            BlockInterrupt {
                source,
                device: VirtioBlockDevice { inner: device },
            }
        })
        .collect()
}
