use async_lock::Mutex;
use core::mem::size_of;

use helios_hal::fs::{BlockDevice, BlockDeviceRights};
use helios_hal::io::{IoError, IoResult};
use helios_hal::resource::KernelResource;

use crate::notify::Notify;
use crate::queue::VirtQueue;
use crate::transport::{DeviceStatus, DeviceType, VirtioFeatures, VirtioTransport};

pub const SECTOR_SIZE: usize = 512;
const BLOCK_QUEUE_INDEX: u16 = 0;
const BLOCK_QUEUE_SIZE: u16 = 16;
const BLK_FEATURE_RO: u64 = 1 << 5;

pub struct VirtioBlockDevice<T: VirtioTransport> {
    transport: T,
    queue: Mutex<VirtQueue<T>>,
    interrupts: Notify,
    capacity_blocks: usize,
    readonly: bool,
}

pub struct VirtioBlockResource<T: VirtioTransport> {
    resource: KernelResource<VirtioBlockDevice<T>, BlockDeviceRights>,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BlkReq {
    type_: u32,
    reserved: u32,
    sector: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BlkResp {
    status: u8,
}

#[repr(u32)]
enum ReqType {
    In = 0,
    Out = 1,
}

impl<T: VirtioTransport> VirtioBlockDevice<T> {
    pub fn new(transport: T) -> IoResult<Self> {
        if transport.device_type() != DeviceType::Block {
            return Err(IoError::Unsupported);
        }

        transport.reset();
        transport.set_status(DeviceStatus::ACKNOWLEDGE);
        transport.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER);

        let offered = transport.device_features();
        let accepted = offered & (VirtioFeatures::VERSION_1.bits() | BLK_FEATURE_RO);
        if accepted & VirtioFeatures::VERSION_1.bits() == 0 {
            return Err(IoError::Unsupported);
        }
        transport.set_driver_features(accepted);
        transport.set_status(
            DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER | DeviceStatus::FEATURES_OK,
        );

        if !transport.status().contains(DeviceStatus::FEATURES_OK) {
            return Err(IoError::Unsupported);
        }

        let capacity_low = transport.read_config_u32(0) as u64;
        let capacity_high = transport.read_config_u32(4) as u64;
        let capacity_blocks = usize::try_from(capacity_low | (capacity_high << 32))
            .map_err(|_| IoError::DeviceFault)?;

        let queue_size = transport
            .queue_max_size(BLOCK_QUEUE_INDEX)
            .min(BLOCK_QUEUE_SIZE);
        if queue_size == 0 || !queue_size.is_power_of_two() {
            return Err(IoError::Unsupported);
        }

        let queue = VirtQueue::new(&transport, BLOCK_QUEUE_INDEX, queue_size)?;
        transport.set_status(
            DeviceStatus::ACKNOWLEDGE
                | DeviceStatus::DRIVER
                | DeviceStatus::FEATURES_OK
                | DeviceStatus::DRIVER_OK,
        );

        Ok(Self {
            transport,
            queue: Mutex::new(queue),
            interrupts: Notify::new(),
            capacity_blocks,
            readonly: accepted & BLK_FEATURE_RO != 0,
        })
    }

    pub fn into_resource(self, rights: BlockDeviceRights) -> VirtioBlockResource<T> {
        VirtioBlockResource {
            resource: KernelResource::new(self, rights),
        }
    }

    pub fn new_resource(
        transport: T,
        rights: BlockDeviceRights,
    ) -> IoResult<VirtioBlockResource<T>> {
        Self::new(transport).map(|device| device.into_resource(rights))
    }

    /// Interrupt handlers should only call this method: acknowledge the device
    /// interrupt and wake the async driver task.
    pub fn handle_interrupt(&self) {
        self.transport.ack_interrupt();
        self.interrupts.notify_one();
    }

    async fn read_block_inner(&self, block_id: usize, buf: &mut [u8]) -> IoResult<()> {
        let request = BlkReq {
            type_: ReqType::In as u32,
            reserved: 0,
            sector: block_id as u64,
        };
        let mut response = BlkResp::default();
        let request_bytes = as_bytes(&request);
        let response_bytes = as_bytes_mut(&mut response);
        let mut outputs = [buf, response_bytes];

        let mut queue = self.queue.lock().await;
        let token = queue.submit(&[request_bytes], &mut outputs)?;
        queue.notify(&self.transport);

        loop {
            if let Some(used) = queue.pop_used() {
                assert_eq!(used, token, "virtio block completion token mismatch");
                break;
            }
            self.interrupts.notified().await;
        }

        map_block_status(response.status)
    }

    async fn write_block_inner(&self, block_id: usize, buf: &[u8]) -> IoResult<()> {
        let request = BlkReq {
            type_: ReqType::Out as u32,
            reserved: 0,
            sector: block_id as u64,
        };
        let mut response = BlkResp::default();
        let request_bytes = as_bytes(&request);
        let response_bytes = as_bytes_mut(&mut response);
        let mut outputs = [response_bytes];

        let mut queue = self.queue.lock().await;
        let token = queue.submit(&[request_bytes, buf], &mut outputs)?;
        queue.notify(&self.transport);

        loop {
            if let Some(used) = queue.pop_used() {
                assert_eq!(used, token, "virtio block completion token mismatch");
                break;
            }
            self.interrupts.notified().await;
        }

        map_block_status(response.status)
    }
}

impl<T: VirtioTransport> VirtioBlockResource<T> {
    pub fn rights(&self) -> BlockDeviceRights {
        self.resource.rights()
    }

    pub fn derive(&self, rights: BlockDeviceRights) -> Option<Self> {
        self.resource
            .derive(rights)
            .map(|resource| Self { resource })
    }

    pub fn handle_interrupt(&self) {
        self.resource.object().handle_interrupt();
    }

    fn object(&self) -> &VirtioBlockDevice<T> {
        self.resource.object()
    }
}

impl<T: VirtioTransport> Clone for VirtioBlockResource<T> {
    fn clone(&self) -> Self {
        Self {
            resource: self.resource.clone(),
        }
    }
}

impl<T: VirtioTransport> BlockDevice for VirtioBlockResource<T> {
    async fn read_block(&self, block_id: usize, buf: &mut [u8]) -> IoResult<()> {
        validate_request(
            self.rights(),
            self.object().capacity_blocks,
            block_id,
            buf.len(),
            BlockDeviceRights::READ,
        )?;

        self.object().read_block_inner(block_id, buf).await
    }

    async fn write_block(&self, block_id: usize, buf: &[u8]) -> IoResult<()> {
        validate_request(
            self.rights(),
            self.object().capacity_blocks,
            block_id,
            buf.len(),
            BlockDeviceRights::WRITE,
        )?;

        if self.object().readonly {
            return Err(IoError::ReadOnly);
        }

        self.object().write_block_inner(block_id, buf).await
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

fn map_block_status(status: u8) -> IoResult<()> {
    match status {
        0 => Ok(()),
        1 => Err(IoError::DeviceFault),
        2 => Err(IoError::Unsupported),
        _ => Err(IoError::DeviceFault),
    }
}

fn as_bytes<T>(value: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

fn as_bytes_mut<T>(value: &mut T) -> &mut [u8] {
    unsafe { core::slice::from_raw_parts_mut((value as *mut T).cast::<u8>(), size_of::<T>()) }
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
