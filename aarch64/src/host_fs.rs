extern crate alloc;

use crate::platform::PlatformDescription;
use arm_gic::{IntId, Trigger};
use bytes::BytesMut;
use helios_hal::io::IoError;
use helios_kernel::{ExternalInterruptHandler, HostFsTransport};
use triomphe::Arc;

type Aarch64Virtio9pDevice = helios_virtio::Virtio9pDevice<
    helios_virtio::VirtioMmioTransport<helios_virtio::MmioBus<helios_virtio::OffsetDmaPool>>,
>;

pub(crate) type HostFileSystemService =
    helios_kernel::HostFsClient<HostFsTransportService, crate::Aarch64Cpu>;

#[derive(Clone)]
pub(crate) struct HostFsTransportService {
    device: Arc<Aarch64Virtio9pDevice>,
}

impl ExternalInterruptHandler for HostFsTransportService {
    fn handle_interrupt(&self) {
        self.device.handle_interrupt();
    }
}

/// The 9p transport the bootstrap processor brought up, together with
/// the interrupt the device tree routes it to.
pub(crate) struct HostFsInterrupt {
    pub(crate) interrupt: IntId,
    pub(crate) trigger: Trigger,
    pub(crate) transport: HostFsTransportService,
}

pub(crate) fn install(
    cpu: &crate::Aarch64Cpu,
    platform: &PlatformDescription,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
    debug_state: &crate::debug_state::RuntimeState,
) -> Option<HostFsInterrupt> {
    let Some(probe) = discover_9p_device(platform, physical_memory_offset, handoff) else {
        tracing::warn!("virtio 9p device was not discovered on the platform bus");
        return None;
    };
    let service = HostFileSystemService::new(probe.transport.clone(), *cpu);
    debug_state.install_host_fs_service(service);
    tracing::info!(
        "virtio 9p online mount_tag={} interrupt={:?}",
        helios_kernel::HOST_SHARE_MOUNT_TAG,
        probe.interrupt
    );
    Some(probe)
}

impl HostFsTransport for HostFsTransportService {
    fn mount_tag(&self) -> &str {
        self.device.mount_tag()
    }

    fn pipeline_depth(&self) -> usize {
        self.device.pipeline_depth()
    }

    async fn request<'a>(
        &'a self,
        bytes: &'a [u8],
        response: &'a mut BytesMut,
        response_len: usize,
    ) -> Result<(), IoError> {
        response.clear();
        response.resize(response_len, 0);
        let used = self.device.request(bytes, response).await?;
        let used = usize::try_from(used).map_err(|_| IoError::DeviceFault)?;
        response.truncate(used);
        Ok(())
    }
}

pub(crate) fn has_9p_device(platform: &PlatformDescription) -> bool {
    crate::count_virtio_mmio_devices(platform, helios_virtio::DeviceType::_9P) != 0
}

fn discover_9p_device(
    platform: &PlatformDescription,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
) -> Option<HostFsInterrupt> {
    let candidate = crate::virtio_slots(
        platform,
        physical_memory_offset,
        handoff,
        helios_virtio::DeviceType::_9P,
    )
    .next()?;
    let (interrupt, trigger) = (candidate.interrupt.intid(), candidate.interrupt.trigger);
    Some(HostFsInterrupt {
        interrupt,
        trigger,
        transport: HostFsTransportService {
            device: init_9p_device(
                candidate.region.base,
                candidate.region.size,
                physical_memory_offset,
                handoff,
            ),
        },
    })
}

fn init_9p_device(
    physical_base: usize,
    size: usize,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
) -> Arc<Aarch64Virtio9pDevice> {
    assert!(size != 0, "AArch64 virtio-9p node has zero MMIO size");
    crate::map_mmio_page(physical_base, physical_memory_offset, handoff);
    let virtual_base = crate::mmio_virtual_base(physical_base, physical_memory_offset);
    let header = core::ptr::NonNull::new(virtual_base as *mut u8)
        .unwrap_or_else(|| panic!("virtio MMIO base {virtual_base:#x} was unexpectedly null"));
    let dma = helios_virtio::OffsetDmaPool::new(physical_memory_offset);
    let device = unsafe { helios_virtio::p9_from_mmio_with_dma(header, size, dma) }.unwrap_or_else(
        |error| panic!("failed to initialize virtio-9p device at {physical_base:#x}: {error}"),
    );
    Arc::new(device)
}
