//! virtio-blk over the platform MMIO bus for the RISC-V backend.
//!
//! Every block device the device tree names is brought up here, up to
//! the block interrupt routes the kernel's table holds; what each of
//! them is for is the kernel's decision, taken from the serials they
//! report.
//!
//! Concurrency contract: the devices are discovered and programmed on
//! the bootstrap hart before external interrupts are unmasked, and their
//! completions arrive on the PLIC lines the device tree names, so
//! every reader parks on a notification rather than polling a used ring.

extern crate alloc;

use alloc::vec::Vec;

use fdt::Fdt;
use helios_hal::fs::BlockDeviceRights;
use helios_kernel::ExternalInterruptHandler;

use crate::RiscvCpu;
use crate::debug_state::RuntimeState;
use crate::net::PlicInterrupt;

type RiscvVirtioBlockDevice =
    helios_virtio::VirtioMmioBlockDevice<RiscvCpu, helios_kernel::BlockPages>;

#[derive(Clone)]
pub(crate) struct VirtioBlockDevice {
    inner: RiscvVirtioBlockDevice,
}

/// One block device the bootstrap hart brought up, together with the
/// PLIC line the device tree routes it to.
pub(crate) struct BlockInterrupt {
    pub(crate) line: PlicInterrupt,
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
/// the kernel, which gives each the role its serial names.
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
    // Swap is not wired on this backend, so the scratch disk is proved
    // and counted but nothing writes to it yet. Nothing mounts a disk
    // yet either, so the mount sources' cache is held by its reclaim
    // task alone.
    helios_kernel::install_block_devices(
        kernel,
        root,
        devices,
        move |service, _scratch, _mount_sources| {
            state.install_block_service(service);
        },
    );
    for interrupt in &discovered {
        tracing::info!(irq = interrupt.line.number(), "virtio block online");
    }
    discovered
}

fn discover_block_devices(cpu: &RiscvCpu, fdt: &Fdt<'_>) -> Vec<BlockInterrupt> {
    let candidates = helios_virtio::mmio_candidates(fdt)
        .filter(|candidate| {
            crate::matches_virtio_mmio_device(candidate.base, helios_virtio::DeviceType::Block)
        })
        .collect();
    helios_kernel::admit_block_devices(candidates)
        .into_iter()
        .map(|candidate| {
            let base = candidate.base;
            let header = core::ptr::NonNull::new(base as *mut u8)
                .unwrap_or_else(|| panic!("virtio MMIO base {base:#x} was unexpectedly null"));
            let line = candidate
                .interrupt
                .and_then(PlicInterrupt::from_specifier)
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
                line,
                device: VirtioBlockDevice { inner: device },
            }
        })
        .collect()
}
