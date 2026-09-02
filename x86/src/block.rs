//! virtio-blk over PCI for the x86 backend.
//!
//! q35 hands the kernel every disk on the same bus — the image the
//! firmware booted from and the scratch disk the kernel owns are two
//! functions of the same device class — so every one of them is brought
//! up here and the kernel decides which it keeps from the serials they
//! report.
//!
//! Concurrency contract: the functions are discovered and programmed on
//! the bootstrap processor before interrupts are enabled, and each of
//! them delivers its completions on its own MSI-X vector, so every
//! reader parks on a notification instead of polling a used ring.

extern crate alloc;

use alloc::vec::Vec;

use helios_hal::fs::BlockDeviceRights;
use helios_kernel::{ExternalInterruptHandler, RootEntropyHandle};
use helios_virtio::{DeviceType, OffsetDmaPool, VirtioBlockResource, VirtioPciTransport};
use pci_types::PciAddress;

use crate::X86Cpu;
use crate::exceptions::BLOCK_INTERRUPT_VECTORS;
use crate::pci::PciRoot;

type X86VirtioBlockDevice = VirtioBlockResource<VirtioPciTransport<OffsetDmaPool>, X86Cpu>;

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

/// Every PCI function that carries a block device.
pub(crate) fn discover(pci: &PciRoot) -> Vec<PciAddress> {
    let functions: Vec<PciAddress> = pci.find_virtio_functions(DeviceType::Block).collect();
    assert!(
        functions.len() <= BLOCK_INTERRUPT_VECTORS.len(),
        "platform exposes {} block functions but only {} interrupt vectors exist",
        functions.len(),
        BLOCK_INTERRUPT_VECTORS.len()
    );
    functions
}

/// Brings up every block function and hands them to the kernel, which
/// keeps the one the platform named as its own.
pub(crate) fn install<WatchdogImpl>(
    cpu: &X86Cpu,
    kernel: &helios_kernel::Kernel<X86Cpu, WatchdogImpl>,
    pci: &PciRoot,
    functions: &[PciAddress],
    physical_memory_offset: usize,
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
        .map(|(address, vector)| {
            let msix_vector = pci.bind_msix_vector(*address, vector, destination_apic_id);
            let device = helios_virtio::block_from_pci(
                &pci.access(),
                *address,
                pci,
                OffsetDmaPool::new(physical_memory_offset),
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
    helios_kernel::install_block_devices(kernel, root, devices, move |service| {
        state.install_block_service(service);
    });
    discovered
}
