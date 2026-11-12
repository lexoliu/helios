//! virtio-blk over the platform MMIO bus for the AArch64 backend.
//!
//! Every block device the platform names is brought up here, up to the
//! block interrupt routes the kernel's table holds; what each of them is
//! for is the kernel's decision, taken from the serials they report.
//!
//! Concurrency contract: the devices are discovered and programmed on
//! the bootstrap processor before IRQs are unmasked, and their
//! completions arrive on the SPIs the device tree names, so every reader
//! parks on a notification rather than polling a used ring.

extern crate alloc;

use alloc::vec::Vec;

use crate::platform::PlatformDescription;
use arm_gic::{IntId, Trigger};
use helios_hal::fs::BlockDeviceRights;
use helios_kernel::ExternalInterruptHandler;

use crate::Aarch64Cpu;
use crate::debug_state::RuntimeState;

type Aarch64VirtioBlockDevice = helios_virtio::VirtioBlockResource<
    helios_virtio::VirtioMmioTransport<helios_virtio::MmioBus<helios_virtio::OffsetDmaPool>>,
    Aarch64Cpu,
    helios_kernel::BlockPages,
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

pub(crate) fn count_block_devices(platform: &PlatformDescription) -> usize {
    crate::count_virtio_mmio_devices(platform, helios_virtio::DeviceType::Block)
}

/// Brings up every block device on the platform bus and hands them to
/// the kernel, which gives each the role its serial names and returns
/// the scratch disk for swap.
pub(crate) fn install<WatchdogImpl>(
    cpu: &Aarch64Cpu,
    kernel: &helios_kernel::Kernel<Aarch64Cpu, WatchdogImpl>,
    platform: &PlatformDescription,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
    debug_state: &RuntimeState,
    root: helios_kernel::RootEntropyHandle,
) -> Vec<BlockInterrupt>
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let discovered = discover_block_devices(cpu, platform, physical_memory_offset, handoff);
    let devices = discovered
        .iter()
        .map(|interrupt| interrupt.device.inner.clone())
        .collect();
    let state = debug_state.clone();
    let spawner = kernel.spawner();
    let timer = kernel.timer();
    let swap_cpu = *cpu;
    // Nothing mounts a disk yet, so the mount sources' cache is held by
    // its reclaim task alone.
    helios_kernel::install_block_devices(
        kernel,
        root,
        devices,
        move |service, scratch, _mount_sources| {
            state.install_block_service(service);
            if let Some(swap) = helios_kernel::install_scratch_swap(
                state.instance_registry(),
                scratch,
                spawner,
                timer,
                swap_cpu,
                "virtio-blk",
                helios_virtio::VirtioBlockSwapBackend::new,
            ) {
                state.install_swap(swap);
            }
        },
    );
    for interrupt in &discovered {
        tracing::info!(interrupt = ?interrupt.interrupt, "virtio block online");
    }
    discovered
}

fn discover_block_devices(
    cpu: &Aarch64Cpu,
    platform: &PlatformDescription,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
) -> Vec<BlockInterrupt> {
    let candidates = crate::virtio_slots(
        platform,
        physical_memory_offset,
        handoff,
        helios_virtio::DeviceType::Block,
    )
    .collect();
    helios_kernel::admit_block_devices(candidates)
        .into_iter()
        .map(|candidate| {
            let (interrupt, trigger) = (candidate.interrupt.intid(), candidate.interrupt.trigger);
            assert!(
                candidate.region.size != 0,
                "AArch64 virtio-blk node has zero MMIO size"
            );
            crate::map_mmio_page(candidate.region.base, physical_memory_offset, handoff);
            let virtual_base =
                crate::mmio_virtual_base(candidate.region.base, physical_memory_offset);
            let header = core::ptr::NonNull::new(virtual_base as *mut u8).unwrap_or_else(|| {
                panic!("virtio MMIO base {virtual_base:#x} was unexpectedly null")
            });
            let dma = helios_virtio::OffsetDmaPool::new(physical_memory_offset);
            let device = unsafe {
                helios_virtio::block_from_mmio_with_dma(
                    header,
                    candidate.region.size,
                    dma,
                    *cpu,
                    BlockDeviceRights::READ | BlockDeviceRights::WRITE,
                )
            }
            .unwrap_or_else(|error| {
                panic!(
                    "failed to initialize virtio-blk device at {:#x}: {error}",
                    candidate.region.base
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
