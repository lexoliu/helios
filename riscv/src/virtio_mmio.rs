use helios_virtio::DeviceType;

const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976;
const VIRTIO_MMIO_MODERN_VERSION: u32 = 2;
const VIRTIO_MMIO_MAGIC_OFFSET: usize = 0x000;
const VIRTIO_MMIO_VERSION_OFFSET: usize = 0x004;
const VIRTIO_MMIO_DEVICE_ID_OFFSET: usize = 0x008;

pub(crate) fn matches_device(base: usize, expected: DeviceType) -> bool {
    unsafe {
        read_u32(base + VIRTIO_MMIO_MAGIC_OFFSET) == VIRTIO_MMIO_MAGIC
            && read_u32(base + VIRTIO_MMIO_VERSION_OFFSET) == VIRTIO_MMIO_MODERN_VERSION
            && read_u32(base + VIRTIO_MMIO_DEVICE_ID_OFFSET) == expected as u32
    }
}

unsafe fn read_u32(addr: usize) -> u32 {
    unsafe { (addr as *const u32).read_volatile() }
}
