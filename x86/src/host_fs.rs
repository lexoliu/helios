//! virtio-9p host share over PCI for the x86 backend.
//!
//! Concurrency contract: the device is constructed on the bootstrap
//! processor before interrupts are unmasked. Afterwards every caller
//! submits straight to the device, which pipelines requests and routes
//! each completion back to the task that submitted it; completions
//! arrive on the device's MSI-X vector, so callers await a notification
//! instead of polling the used ring.

extern crate alloc;

use alloc::sync::Arc;

use bytes::BytesMut;
use helios_hal::io::IoError;
use helios_kernel::{ExternalInterruptHandler, HostFsTransport};
use helios_virtio::{DeviceType, Virtio9pDevice, VirtioPciTransport};
use pci_types::PciAddress;

use crate::debug_state::RuntimeState;
use crate::iommu::X86DmaPool;
use crate::pci::PciRoot;

type X86Virtio9pDevice = Virtio9pDevice<VirtioPciTransport<X86DmaPool>>;

pub(crate) type HostFileSystemService =
    helios_kernel::HostFsClient<HostFsTransportService, crate::X86Cpu>;

#[derive(Clone)]
pub(crate) struct HostFsTransportService {
    device: Arc<X86Virtio9pDevice>,
}

/// The PCI function that carries the platform's virtio-9p host share.
pub(crate) fn discover(pci: &PciRoot) -> Option<PciAddress> {
    pci.find_virtio_function(DeviceType::_9P)
}

/// Brings up the virtio-9p function at `address` and installs the host
/// file system service on top of it.
pub(crate) fn install(
    cpu: &crate::X86Cpu,
    pci: &PciRoot,
    address: PciAddress,
    dma: X86DmaPool,
    vector: u8,
    destination_apic_id: u32,
    debug_state: &RuntimeState,
) -> HostFsTransportService {
    let msix_vector = pci.bind_msix_vector(address, vector, destination_apic_id);
    let device = helios_virtio::p9_from_pci(&pci.access(), address, pci, dma, Some(msix_vector))
        .unwrap_or_else(|error| {
            panic!("failed to initialize the virtio-9p function at {address}: {error}")
        });
    let transport = HostFsTransportService {
        device: Arc::new(device),
    };
    debug_state.install_host_fs_service(HostFileSystemService::new(transport.clone(), cpu.clone()));
    tracing::info!(
        "virtio 9p online transport=pci function={address} msix_vector={vector:#x} mount_tag={}",
        transport.device.mount_tag()
    );
    transport
}

impl ExternalInterruptHandler for HostFsTransportService {
    fn handle_interrupt(&self) {
        self.device.handle_interrupt();
    }
}

impl HostFsTransport for HostFsTransportService {
    fn mount_tag(&self) -> &str {
        self.device.mount_tag()
    }

    fn pipeline_depth(&self) -> usize {
        self.device.pipeline_depth()
    }

    fn request<'a>(
        &'a self,
        bytes: &'a [u8],
        response: &'a mut BytesMut,
        response_len: usize,
    ) -> impl core::future::Future<Output = Result<(), IoError>> + Send + 'a {
        async move {
            response.clear();
            response.resize(response_len, 0);
            let used = self.device.request(bytes, response).await?;
            let used = usize::try_from(used).map_err(|_| IoError::DeviceFault)?;
            response.truncate(used);
            Ok(())
        }
    }
}
