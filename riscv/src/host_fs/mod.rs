//! virtio-9p host share over MMIO for the riscv backend.
//!
//! Concurrency contract: the device is constructed on the bootstrap
//! hart before interrupts are enabled. Afterwards every caller submits
//! straight to the device, which pipelines requests and routes each
//! completion back to the task that submitted it; completions arrive on
//! the PLIC line the device tree names, so callers await a notification
//! instead of polling the used ring.

extern crate alloc;

use alloc::sync::Arc;

use bytes::BytesMut;
use fdt::Fdt;
use helios_hal::io::IoError;
use helios_kernel::{ExternalInterruptHandler, HostFsTransport};

use crate::net::InterruptSourceId;

pub(crate) const HOST_MOUNT_TAG: &str = helios_kernel::HOST_SHARE_MOUNT_TAG;

pub(crate) type HostFileSystemService =
    helios_kernel::HostFsClient<HostFsTransportService, crate::RiscvCpu>;

#[derive(Clone)]
pub(crate) struct HostFsTransportService {
    device: Arc<helios_virtio::VirtioMmio9pDevice>,
}

pub(crate) struct HostFsInterrupt {
    pub(crate) source: InterruptSourceId,
    pub(crate) transport: HostFsTransportService,
}

pub(crate) fn install(
    cpu: &crate::RiscvCpu,
    fdt: &Fdt<'_>,
    debug_state: &crate::debug_state::RuntimeState,
) -> Option<HostFsInterrupt> {
    let Some((device, source)) = discover_9p_device(fdt) else {
        tracing::warn!("virtio 9p device was not discovered on the platform bus");
        return None;
    };

    let transport = HostFsTransportService { device };
    debug_state.install_host_fs_service(HostFileSystemService::new(transport.clone(), cpu.clone()));
    tracing::info!(
        "virtio 9p online mount_tag={HOST_MOUNT_TAG} irq={}",
        source.0.get()
    );

    Some(HostFsInterrupt { source, transport })
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

fn discover_9p_device(
    fdt: &Fdt<'_>,
) -> Option<(Arc<helios_virtio::VirtioMmio9pDevice>, InterruptSourceId)> {
    let candidate = helios_virtio::mmio_candidates(fdt).find(|candidate| {
        crate::matches_virtio_mmio_device(candidate.base, helios_virtio::DeviceType::_9P)
    })?;
    let base = candidate.base;
    let header = core::ptr::NonNull::new(base as *mut u8)
        .unwrap_or_else(|| panic!("virtio MMIO base {base:#x} was unexpectedly null"));
    let irq_source = candidate
        .interrupt
        .and_then(|interrupt| core::num::NonZeroU32::new(interrupt.number))
        .map(InterruptSourceId)
        .unwrap_or_else(|| panic!("virtio-9p node at {base:#x} has no valid interrupt source"));
    let device =
        unsafe { helios_virtio::p9_from_mmio(header, candidate.size) }.unwrap_or_else(|error| {
            panic!("failed to initialize virtio-9p device at {base:#x}: {error}")
        });
    Some((Arc::new(device), irq_source))
}

pub(crate) fn has_9p_device(fdt: &Fdt<'_>) -> bool {
    crate::count_virtio_mmio_devices(fdt, helios_virtio::DeviceType::_9P) != 0
}
