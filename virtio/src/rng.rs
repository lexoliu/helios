use spin::Mutex;

use helios_hal::io::{IoError, IoResult};

use crate::queue::VirtQueue;
use crate::transport::{DeviceStatus, DeviceType, VirtioFeatures, VirtioTransport};

const REQUEST_QUEUE_INDEX: u16 = 0;
const REQUEST_QUEUE_SIZE: u16 = 16;

pub struct VirtioRngDevice<T: VirtioTransport> {
    transport: T,
    queue: Mutex<VirtQueue<T>>,
}

impl<T: VirtioTransport> VirtioRngDevice<T> {
    pub fn new(transport: T) -> IoResult<Self> {
        if transport.device_type() != DeviceType::Entropy {
            return Err(IoError::Unsupported);
        }

        transport.reset();
        transport.set_status(DeviceStatus::ACKNOWLEDGE);
        transport.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER);

        let accepted = transport.device_features() & VirtioFeatures::VERSION_1.bits();
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

        let queue_size = transport
            .queue_max_size(REQUEST_QUEUE_INDEX)
            .min(REQUEST_QUEUE_SIZE);
        if queue_size == 0 || !queue_size.is_power_of_two() {
            return Err(IoError::Unsupported);
        }

        let queue = VirtQueue::new(&transport, REQUEST_QUEUE_INDEX, queue_size)?;
        transport.set_status(
            DeviceStatus::ACKNOWLEDGE
                | DeviceStatus::DRIVER
                | DeviceStatus::FEATURES_OK
                | DeviceStatus::DRIVER_OK,
        );

        Ok(Self {
            transport,
            queue: Mutex::new(queue),
        })
    }

    pub fn fill(&self, buffer: &mut [u8]) -> IoResult<()> {
        if buffer.is_empty() {
            return Ok(());
        }

        let mut filled = 0usize;
        while filled < buffer.len() {
            let (_, len) = self.fill_chunk(&mut buffer[filled..])?;
            if len == 0 {
                return Err(IoError::DeviceFault);
            }
            filled = filled.checked_add(len).ok_or(IoError::DeviceFault)?;
        }
        Ok(())
    }

    fn fill_chunk(&self, buffer: &mut [u8]) -> IoResult<(u16, usize)> {
        let mut queue = self.queue.lock();
        let token = queue.submit(&[], &mut [buffer])?;
        queue.notify(&self.transport);
        loop {
            if let Some((used, len)) = queue.pop_used_with_len() {
                if used != token {
                    return Err(IoError::DeviceFault);
                }
                let len = usize::try_from(len).map_err(|_| IoError::DeviceFault)?;
                if len > buffer.len() {
                    return Err(IoError::DeviceFault);
                }
                return Ok((used, len));
            }
            core::hint::spin_loop();
        }
    }
}
