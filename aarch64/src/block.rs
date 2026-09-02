//! virtio-blk over the platform MMIO bus for the AArch64 backend.
//!
//! Every block device the device tree names is brought up here; which of
//! them the kernel ends up owning is the kernel's decision, taken from
//! the serials the devices report.
//!
//! Concurrency contract: the devices are discovered and programmed on
//! the bootstrap processor before IRQs are unmasked, and their
//! completions arrive on the SPIs the device tree names, so every reader
//! parks on a notification rather than polling a used ring.

extern crate alloc;

use alloc::vec::Vec;

use arm_gic::{IntId, Trigger};
use fdt::Fdt;
use helios_hal::fs::BlockDeviceRights;
use helios_kernel::ExternalInterruptHandler;

use crate::Aarch64Cpu;
use crate::debug_state::RuntimeState;

type Aarch64VirtioBlockDevice = helios_virtio::VirtioBlockResource<
    helios_virtio::VirtioMmioTransport<helios_virtio::MmioBus<helios_virtio::OffsetDmaPool>>,
    Aarch64Cpu,
>;

#[derive(Clone)]
pub(crate) struct VirtioBlockDevice {
    inner: Aarch64VirtioBlockDevice,
}

/// One block device the bootstrap processor brought up, together with
/// the interrupt the device tree routes it to.
pub(crate) struct BlockInterrupt {
    pub(crate) interrupt: IntId,
    pub(crate) trigger: Trigger,
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
    cpu: &Aarch64Cpu,
    kernel: &helios_kernel::Kernel<Aarch64Cpu, WatchdogImpl>,
    fdt: &Fdt<'_>,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
    debug_state: &RuntimeState,
    root: helios_kernel::RootEntropyHandle,
) -> Vec<BlockInterrupt>
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let discovered = discover_block_devices(cpu, fdt, physical_memory_offset, handoff);
    let devices = discovered
        .iter()
        .map(|interrupt| interrupt.device.inner.clone())
        .collect();
    let state = debug_state.clone();
    helios_kernel::install_block_devices(kernel, root, devices, move |service| {
        state.install_block_service(service);
    });
    for interrupt in &discovered {
        tracing::info!(interrupt = ?interrupt.interrupt, "virtio block online");
    }
    discovered
}

fn discover_block_devices(
    cpu: &Aarch64Cpu,
    fdt: &Fdt<'_>,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
) -> Vec<BlockInterrupt> {
    helios_virtio::mmio_candidates(fdt)
        .filter(|candidate| {
            crate::matches_virtio_mmio_device(
                candidate.base,
                physical_memory_offset,
                handoff,
                helios_virtio::DeviceType::Block,
            )
        })
        .map(|candidate| {
            let (interrupt, trigger) =
                crate::gic::device_interrupt(candidate.interrupt, candidate.base);
            assert!(
                candidate.size != 0,
                "AArch64 virtio-blk node has zero MMIO size"
            );
            crate::map_mmio_page(candidate.base, physical_memory_offset, handoff);
            let virtual_base = crate::mmio_virtual_base(candidate.base, physical_memory_offset);
            let header = core::ptr::NonNull::new(virtual_base as *mut u8).unwrap_or_else(|| {
                panic!("virtio MMIO base {virtual_base:#x} was unexpectedly null")
            });
            let dma = helios_virtio::OffsetDmaPool::new(physical_memory_offset);
            let device = unsafe {
                helios_virtio::block_from_mmio_with_dma(
                    header,
                    candidate.size,
                    dma,
                    *cpu,
                    BlockDeviceRights::READ | BlockDeviceRights::WRITE,
                )
            }
            .unwrap_or_else(|error| {
                panic!(
                    "failed to initialize virtio-blk device at {:#x}: {error}",
                    candidate.base
                )
            });
            BlockInterrupt {
                interrupt,
                trigger,
                device: VirtioBlockDevice { inner: device },
            }
        })
        .collect()
}
