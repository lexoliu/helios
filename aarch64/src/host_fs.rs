extern crate alloc;

use bytes::BytesMut;
use fdt::Fdt;
use helios_hal::io::IoError;
use helios_kernel::HostFsTransport;
use triomphe::Arc;

type Aarch64Virtio9pDevice = helios_virtio::Virtio9pDevice<
    helios_virtio::VirtioMmioTransport<helios_virtio::MmioBus<helios_virtio::OffsetDmaPool>>,
>;

pub(crate) type HostFileSystemService = helios_kernel::HostFsClient<HostFsTransportService>;

#[derive(Clone)]
pub(crate) struct HostFsTransportService {
    device: Arc<Aarch64Virtio9pDevice>,
}

pub(crate) fn install(
    fdt: Option<&Fdt<'_>>,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
    debug_state: &crate::debug_state::RuntimeState,
) {
    let Some(device) = discover_9p_device(fdt, physical_memory_offset, handoff) else {
        tracing::warn!("virtio 9p device was not discovered on the platform bus");
        return;
    };
    let transport = HostFsTransportService { device };
    let service = HostFileSystemService::new(transport);
    debug_state.install_host_fs_service(service);
    tracing::info!(
        "virtio 9p online mount_tag={}",
        helios_kernel::HOST_SHARE_MOUNT_TAG
    );
}

impl HostFsTransport for HostFsTransportService {
    fn mount_tag(&self) -> &str {
        self.device.mount_tag()
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
            let used = self
                .device
                .request_with_wait(bytes, response, helios_kernel::yield_now)
                .await?;
            let used = usize::try_from(used).map_err(|_| IoError::DeviceFault)?;
            response.truncate(used);
            Ok(())
        }
    }
}

pub(crate) fn has_9p_device(
    fdt: Option<&Fdt<'_>>,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
) -> bool {
    if fdt.is_some_and(|fdt| {
        crate::count_virtio_mmio_devices(fdt, helios_virtio::DeviceType::_9P) != 0
    }) {
        return true;
    }
    scan_qemu_virt_mmio(physical_memory_offset, handoff).any(|physical_base| {
        crate::matches_virtio_mmio_device(
            physical_base,
            physical_memory_offset,
            handoff,
            helios_virtio::DeviceType::_9P,
        )
    })
}

fn discover_9p_device(
    fdt: Option<&Fdt<'_>>,
    physical_memory_offset: usize,
    handoff: &crate::LimineBootHandoff,
) -> Option<Arc<Aarch64Virtio9pDevice>> {
    if let Some(fdt) = fdt {
        for node in fdt.all_nodes() {
            if !node
                .compatible()
                .is_some_and(|compatible| compatible.all().any(|entry| entry == "virtio,mmio"))
            {
                continue;
            }

            let Some(region) = node.raw_reg().and_then(|mut regs| regs.next()) else {
                continue;
            };
            let physical_base =
                crate::fdt_cells_to_usize(region.address, "AArch64 virtio MMIO base");
            if !crate::matches_virtio_mmio_device(
                physical_base,
                physical_memory_offset,
                handoff,
                helios_virtio::DeviceType::_9P,
            ) {
                continue;
            }
            let size = crate::fdt_cells_to_usize(region.size, "AArch64 virtio MMIO size");
            return Some(init_9p_device(
                physical_base,
                size,
                physical_memory_offset,
                handoff,
            ));
        }
    }

    scan_qemu_virt_mmio(physical_memory_offset, handoff)
        .find(|physical_base| {
            crate::matches_virtio_mmio_device(
                *physical_base,
                physical_memory_offset,
                handoff,
                helios_virtio::DeviceType::_9P,
            )
        })
        .map(|physical_base| {
            init_9p_device(
                physical_base,
                crate::QEMU_VIRT_MMIO_SIZE,
                physical_memory_offset,
                handoff,
            )
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

fn scan_qemu_virt_mmio(_: usize, _: &crate::LimineBootHandoff) -> impl Iterator<Item = usize> {
    (0..crate::QEMU_VIRT_MMIO_SLOTS)
        .map(|slot| crate::QEMU_VIRT_MMIO_BASE + slot * crate::QEMU_VIRT_MMIO_STRIDE)
}
