use core::marker::PhantomData;

use helios_hal::fs::{BlockDevice, BlockDeviceRights};
use helios_hal::io::{IoError, IoResult};
use helios_hal::resource::KernelResource;
use spinning_top::Spinlock;
use virtio_drivers::Hal;
use virtio_drivers::device::blk::{SECTOR_SIZE, VirtIOBlk};
use virtio_drivers::transport::Transport;

use crate::error::map_virtio_error;

pub struct VirtioBlockDevice<H: Hal, T: Transport> {
    driver: Spinlock<VirtIOBlk<H, T>>,
    capacity_blocks: usize,
    _hal: PhantomData<H>,
}

pub struct VirtioBlockResource<H: Hal, T: Transport> {
    resource: KernelResource<VirtioBlockDevice<H, T>, BlockDeviceRights>,
}

impl<H: Hal, T: Transport> VirtioBlockDevice<H, T> {
    pub fn new(transport: T) -> IoResult<Self> {
        let driver = VirtIOBlk::<H, T>::new(transport).map_err(map_virtio_error)?;
        let capacity_blocks =
            usize::try_from(driver.capacity()).map_err(|_| IoError::DeviceFault)?;

        Ok(Self {
            driver: Spinlock::new(driver),
            capacity_blocks,
            _hal: PhantomData,
        })
    }

    pub fn into_resource(self, rights: BlockDeviceRights) -> VirtioBlockResource<H, T> {
        VirtioBlockResource {
            resource: KernelResource::new(self, rights),
        }
    }

    pub fn new_resource(
        transport: T,
        rights: BlockDeviceRights,
    ) -> IoResult<VirtioBlockResource<H, T>> {
        Self::new(transport).map(|device| device.into_resource(rights))
    }
}

impl<H: Hal, T: Transport> VirtioBlockResource<H, T> {
    pub fn rights(&self) -> BlockDeviceRights {
        self.resource.rights()
    }

    pub fn derive(&self, rights: BlockDeviceRights) -> Option<Self> {
        self.resource
            .derive(rights)
            .map(|resource| Self { resource })
    }

    fn object(&self) -> &VirtioBlockDevice<H, T> {
        self.resource.object()
    }
}

impl<H: Hal, T: Transport> Clone for VirtioBlockResource<H, T> {
    fn clone(&self) -> Self {
        Self {
            resource: self.resource.clone(),
        }
    }
}

impl<H, T> BlockDevice for VirtioBlockResource<H, T>
where
    H: Hal + Send + Sync,
    T: Transport + Send,
{
    async fn read_block(&self, block_id: usize, buf: &mut [u8]) -> IoResult<()> {
        validate_request(
            self.rights(),
            self.object().capacity_blocks,
            block_id,
            buf.len(),
            BlockDeviceRights::READ,
        )?;

        self.object()
            .driver
            .lock()
            .read_blocks(block_id, buf)
            .map_err(map_virtio_error)
    }

    async fn write_block(&self, block_id: usize, buf: &[u8]) -> IoResult<()> {
        validate_request(
            self.rights(),
            self.object().capacity_blocks,
            block_id,
            buf.len(),
            BlockDeviceRights::WRITE,
        )?;

        let mut driver = self.object().driver.lock();
        if driver.readonly() {
            return Err(IoError::ReadOnly);
        }

        driver.write_blocks(block_id, buf).map_err(map_virtio_error)
    }

    fn block_size(&self) -> usize {
        SECTOR_SIZE
    }
}

fn validate_request(
    current_rights: BlockDeviceRights,
    capacity_blocks: usize,
    block_id: usize,
    len: usize,
    required_right: BlockDeviceRights,
) -> IoResult<()> {
    if !current_rights.contains(required_right) {
        return Err(IoError::PermissionDenied);
    }

    if len == 0 || len % SECTOR_SIZE != 0 {
        return Err(IoError::InvalidBufferLength {
            required_multiple: SECTOR_SIZE,
            actual: len,
        });
    }

    let requested_blocks = len / SECTOR_SIZE;
    let end_block = block_id
        .checked_add(requested_blocks)
        .ok_or(IoError::OutOfBounds)?;
    if end_block > capacity_blocks {
        return Err(IoError::OutOfBounds);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_request;

    use helios_hal::fs::BlockDeviceRights;
    use helios_hal::io::IoError;

    #[test]
    fn rejects_missing_rights() {
        let error = validate_request(BlockDeviceRights::READ, 8, 0, 512, BlockDeviceRights::WRITE)
            .expect_err("write access without WRITE right must fail");

        assert_eq!(error, IoError::PermissionDenied);
    }

    #[test]
    fn rejects_invalid_buffer_length() {
        let error = validate_request(
            BlockDeviceRights::READ | BlockDeviceRights::WRITE,
            8,
            0,
            511,
            BlockDeviceRights::READ,
        )
        .expect_err("request must reject invalid buffer length");

        assert_eq!(
            error,
            IoError::InvalidBufferLength {
                required_multiple: 512,
                actual: 511,
            }
        );
    }

    #[test]
    fn rejects_out_of_bounds_requests() {
        let error = validate_request(BlockDeviceRights::READ, 1, 1, 512, BlockDeviceRights::READ)
            .expect_err("out-of-bounds request must fail");

        assert_eq!(error, IoError::OutOfBounds);
    }

    #[test]
    fn accepts_well_formed_request() {
        validate_request(
            BlockDeviceRights::READ | BlockDeviceRights::WRITE,
            8,
            2,
            1024,
            BlockDeviceRights::READ,
        )
        .expect("request should be accepted");
    }
}
