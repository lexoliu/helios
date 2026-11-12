//! virtio-blk over PCI for the x86 backend.
//!
//! q35 hands the kernel every disk on the same bus — the image the
//! firmware booted from, the scratch disk the kernel owns and the images
//! it mounts are functions of the same device class — so every one of
//! them is brought up here, up to the block interrupt vectors, and the
//! kernel decides what each is for from the serials they report.
//!
//! Concurrency contract: the functions are discovered and programmed on
//! the bootstrap processor before interrupts are enabled, and each of
//! them delivers its completions on its own MSI-X vector, so every
//! reader parks on a notification instead of polling a used ring.

extern crate alloc;

use alloc::vec::Vec;

use helios_hal::fs::BlockDeviceRights;
use helios_kernel::{ExternalInterruptHandler, RootEntropyHandle};
use helios_virtio::{DeviceType, VirtioBlockResource, VirtioPciTransport};
use pci_types::PciAddress;

use crate::X86Cpu;
use crate::exceptions::BLOCK_INTERRUPT_VECTORS;
use crate::iommu::X86DmaPool;
use crate::pci::PciRoot;

pub(crate) type X86VirtioBlockDevice =
    VirtioBlockResource<VirtioPciTransport<X86DmaPool>, X86Cpu, helios_kernel::BlockPages>;

#[derive(Clone)]
pub(crate) struct VirtioBlockDevice {
    device: X86VirtioBlockDevice,
}

/// One block function the bootstrap processor brought up, together with
/// the IDT vector its messages are delivered on.
pub(crate) struct BlockInterrupt {
    pub(crate) vector: u8,
    pub(crate) device: VirtioBlockDevice,
}

impl ExternalInterruptHandler for VirtioBlockDevice {
    fn handle_interrupt(&self) {
        self.device.handle_interrupt();
    }
}

/// Every PCI function that carries a block device, up to the block
/// interrupt vectors this backend has; the rest are refused by name.
pub(crate) fn discover(pci: &PciRoot) -> Vec<PciAddress> {
    helios_kernel::admit_block_devices(pci.find_virtio_functions(DeviceType::Block).collect())
}

/// Brings up every block function and hands them to the kernel, which
/// gives each the role its serial names.
pub(crate) fn install<WatchdogImpl>(
    cpu: &X86Cpu,
    kernel: &helios_kernel::Kernel<X86Cpu, WatchdogImpl>,
    pci: &PciRoot,
    functions: &[(PciAddress, X86DmaPool)],
    destination_apic_id: u32,
    debug_state: &crate::debug_state::RuntimeState,
    root: RootEntropyHandle,
) -> Vec<BlockInterrupt>
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let discovered: Vec<BlockInterrupt> = functions
        .iter()
        .zip(BLOCK_INTERRUPT_VECTORS)
        .map(|((address, dma), vector)| {
            let msix_vector = pci.bind_msix_vector(*address, vector, destination_apic_id);
            let device = helios_virtio::block_from_pci(
                &pci.access(),
                *address,
                pci,
                *dma,
                Some(msix_vector),
                cpu.clone(),
                BlockDeviceRights::READ | BlockDeviceRights::WRITE,
            )
            .unwrap_or_else(|error| {
                panic!("failed to initialize the virtio-blk function at {address}: {error}")
            });
            tracing::info!(
                "virtio block online transport=pci function={address} msix_vector={vector:#x}"
            );
            BlockInterrupt {
                vector,
                device: VirtioBlockDevice { device },
            }
        })
        .collect();

    let devices = discovered
        .iter()
        .map(|interrupt| interrupt.device.device.clone())
        .collect();
    let state = debug_state.clone();
    // Swap is not wired on this backend (see the caller), so the scratch
    // disk is proved and counted but nothing writes to it yet.
    helios_kernel::install_block_devices(
        kernel,
        root,
        devices,
        move |service, _scratch, mount_sources| {
            state.install_block_service(service);
            if let Some(cache) = mount_sources {
                state.install_mount_sources(cache);
            }
        },
    );
    discovered
}
