use alloc::vec::Vec;
use async_lock::Mutex;
use core::cmp;
use core::mem::size_of;

use helios_hal::fs::{BlockDevice, BlockDeviceRights};
use helios_hal::io::{IoError, IoResult};
use helios_hal::resource::KernelResource;
use helios_hal::vmm::SwapBackend;

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

pub struct VirtioBlockSwapBackend<D: BlockDevice> {
    device: D,
    state: Mutex<VirtioBlockSwapState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtioBlockSwapToken {
    start_block: usize,
    block_count: usize,
    byte_len: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VirtioBlockSwapError {
    #[error("swap payload is empty")]
    EmptyPayload,
    #[error("swap device reports zero-sized blocks")]
    InvalidBlockSize,
    #[error("swap extent is empty")]
    EmptyExtent,
    #[error("swap extent [{start_block}, +{block_count}) exceeds device block count {device_blocks}")]
    ExtentOutOfBounds {
        start_block: usize,
        block_count: usize,
        device_blocks: usize,
    },
    #[error("swap device has {available_blocks} free blocks, requested {requested_blocks}")]
    OutOfSwap {
        requested_blocks: usize,
        available_blocks: usize,
    },
    #[error("swap-in destination length {actual} does not match token byte length {expected}")]
    InvalidDestination { expected: usize, actual: usize },
    #[error("swap block I/O failed: {0}")]
    Io(#[from] IoError),
}

#[derive(Default)]
struct VirtioBlockSwapState {
    free: Vec<SwapExtent>,
}

#[derive(Clone, Copy)]
struct SwapExtent {
    start_block: usize,
    block_count: usize,
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

impl<D: BlockDevice> VirtioBlockSwapBackend<D> {
    pub fn new(
        device: D,
        start_block: usize,
        block_count: usize,
    ) -> Result<Self, VirtioBlockSwapError> {
        let device_blocks = device.block_count();
        if device.block_size() == 0 {
            return Err(VirtioBlockSwapError::InvalidBlockSize);
        }
        if block_count == 0 {
            return Err(VirtioBlockSwapError::EmptyExtent);
        }
        let end_block =
            start_block
                .checked_add(block_count)
                .ok_or(VirtioBlockSwapError::ExtentOutOfBounds {
                    start_block,
                    block_count,
                    device_blocks,
                })?;
        if end_block > device_blocks {
            return Err(VirtioBlockSwapError::ExtentOutOfBounds {
                start_block,
                block_count,
                device_blocks,
            });
        }

        Ok(Self {
            device,
            state: Mutex::new(VirtioBlockSwapState {
                free: Vec::from([SwapExtent {
                    start_block,
                    block_count,
                }]),
            }),
        })
    }

    pub fn from_entire_device(device: D) -> Result<Self, VirtioBlockSwapError> {
        let block_count = device.block_count();
        Self::new(device, 0, block_count)
    }

    async fn allocate_token(
        &self,
        byte_len: usize,
    ) -> Result<VirtioBlockSwapToken, VirtioBlockSwapError> {
        if byte_len == 0 {
            return Err(VirtioBlockSwapError::EmptyPayload);
        }
        let block_size = self.device.block_size();
        if block_size == 0 {
            return Err(VirtioBlockSwapError::InvalidBlockSize);
        }
        let block_count = byte_len.div_ceil(block_size);
        let mut state = self.state.lock().await;
        let start_block = state
            .allocate(block_count)
            .ok_or_else(|| VirtioBlockSwapError::OutOfSwap {
                requested_blocks: block_count,
                available_blocks: state.available_blocks(),
            })?;
        Ok(VirtioBlockSwapToken {
            start_block,
            block_count,
            byte_len,
        })
    }

    async fn release_token(&self, token: VirtioBlockSwapToken) {
        self.state.lock().await.release(SwapExtent {
            start_block: token.start_block,
            block_count: token.block_count,
        });
    }
}

impl VirtioBlockSwapState {
    fn allocate(&mut self, requested_blocks: usize) -> Option<usize> {
        let index = self
            .free
            .iter()
            .position(|extent| extent.block_count >= requested_blocks)?;
        let extent = &mut self.free[index];
        let start_block = extent.start_block;
        extent.start_block += requested_blocks;
        extent.block_count -= requested_blocks;
        if extent.block_count == 0 {
            self.free.swap_remove(index);
        }
        Some(start_block)
    }

    fn release(&mut self, extent: SwapExtent) {
        if extent.block_count == 0 {
            return;
        }
        self.free.push(extent);
        self.free.sort_by_key(|extent| extent.start_block);

        let mut index = 0;
        while index + 1 < self.free.len() {
            let current = self.free[index];
            let next = self.free[index + 1];
            let current_end = current.start_block + current.block_count;
            if current_end >= next.start_block {
                let next_end = next.start_block + next.block_count;
                self.free[index].block_count =
                    cmp::max(current_end, next_end) - current.start_block;
                self.free.remove(index + 1);
            } else {
                index += 1;
            }
        }
    }

    fn available_blocks(&self) -> usize {
        self.free.iter().map(|extent| extent.block_count).sum()
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

    fn block_count(&self) -> usize {
        self.object().capacity_blocks
    }
}

impl<D: BlockDevice + 'static> SwapBackend for VirtioBlockSwapBackend<D> {
    type Token = VirtioBlockSwapToken;
    type Error = VirtioBlockSwapError;

    async fn swap_out(&self, bytes: &[u8]) -> Result<Self::Token, Self::Error> {
        let token = self.allocate_token(bytes.len()).await?;
        let block_size = self.device.block_size();
        let mut padded = Vec::from(bytes);
        padded.resize(token.block_count * block_size, 0);

        if let Err(error) = self.device.write_block(token.start_block, &padded).await {
            self.release_token(token).await;
            return Err(error.into());
        }
        Ok(token)
    }

    async fn swap_in(&self, token: Self::Token, dst: &mut [u8]) -> Result<(), Self::Error> {
        if dst.len() != token.byte_len {
            return Err(VirtioBlockSwapError::InvalidDestination {
                expected: token.byte_len,
                actual: dst.len(),
            });
        }

        let block_size = self.device.block_size();
        if block_size == 0 {
            return Err(VirtioBlockSwapError::InvalidBlockSize);
        }
        let mut padded = Vec::new();
        padded.resize(token.block_count * block_size, 0);
        self.device
            .read_block(token.start_block, &mut padded)
            .await
            .map_err(VirtioBlockSwapError::Io)?;
        dst.copy_from_slice(&padded[..token.byte_len]);
        self.release_token(token).await;
        Ok(())
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

    if len == 0 || !len.is_multiple_of(SECTOR_SIZE) {
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
