use async_lock::Mutex;

use helios_hal::io::{IoError, IoResult};

use crate::features::{NegotiatedFeatures, RING_FEATURES, negotiate};
use crate::inflight::{InFlight, await_completion, submit_chain};
use crate::notify::Notify;
use crate::queue::VirtQueue;
use crate::transport::{DeviceStatus, DeviceType, VirtioTransport};

const REQUEST_QUEUE_INDEX: u16 = 0;
const REQUEST_QUEUE_SIZE: u16 = 16;
/// The entropy queue only ever carries a single writable buffer.
const REQUEST_CHAIN_LIMIT: u16 = 1;

pub struct VirtioRngDevice<T: VirtioTransport> {
    transport: T,
    queue: Mutex<VirtQueue<T>>,
    inflight: InFlight<{ REQUEST_QUEUE_SIZE as usize }>,
    interrupts: Notify,
    features: NegotiatedFeatures,
}

impl<T: VirtioTransport> VirtioRngDevice<T> {
    pub fn new(transport: T) -> IoResult<Self> {
        if transport.device_type() != DeviceType::Entropy {
            return Err(IoError::Unsupported);
        }

        let features = negotiate(&transport, RING_FEATURES)?;

        let queue_size = transport
            .queue_max_size(REQUEST_QUEUE_INDEX)
            .min(REQUEST_QUEUE_SIZE);
        if queue_size == 0 || !queue_size.is_power_of_two() {
            return Err(IoError::Unsupported);
        }

        let queue = VirtQueue::new(
            &transport,
            REQUEST_QUEUE_INDEX,
            queue_size,
            REQUEST_CHAIN_LIMIT,
            features,
        )?;
        transport.set_status(
            DeviceStatus::ACKNOWLEDGE
                | DeviceStatus::DRIVER
                | DeviceStatus::FEATURES_OK
                | DeviceStatus::DRIVER_OK,
        );

        Ok(Self {
            transport,
            queue: Mutex::new(queue),
            inflight: InFlight::new(),
            interrupts: Notify::new(),
            features,
        })
    }

    /// The feature set this device negotiated.
    pub fn features(&self) -> NegotiatedFeatures {
        self.features
    }

    /// Interrupt handlers should only acknowledge the device and wake waiters.
    pub fn handle_interrupt(&self) {
        self.transport.ack_interrupt();
        self.interrupts.notify_all();
    }

    pub async fn fill(&self, buffer: &mut [u8]) -> IoResult<()> {
        if buffer.is_empty() {
            return Ok(());
        }

        let mut filled = 0usize;
        while filled < buffer.len() {
            let len = self.fill_chunk(&mut buffer[filled..]).await?;
            if len == 0 {
                return Err(IoError::DeviceFault);
            }
            filled = filled.checked_add(len).ok_or(IoError::DeviceFault)?;
        }
        Ok(())
    }

    async fn fill_chunk(&self, buffer: &mut [u8]) -> IoResult<usize> {
        let capacity = buffer.len();
        let token = submit_chain(
            &self.inflight,
            &self.queue,
            &self.interrupts,
            &self.transport,
            &[],
            &mut [buffer],
        )
        .await?;

        let len = await_completion(&self.inflight, &self.queue, &self.interrupts, token, || {
            self.interrupts.notified()
        })
        .await;
        let len = usize::try_from(len).map_err(|_| IoError::DeviceFault)?;
        if len > capacity {
            return Err(IoError::DeviceFault);
        }
        Ok(len)
    }
}

impl<T: VirtioTransport> Drop for VirtioRngDevice<T> {
    fn drop(&mut self) {
        self.queue.get_mut().shutdown(&self.transport);
    }
}
