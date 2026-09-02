use alloc::string::String;
use alloc::vec;

use async_lock::Mutex;
use core::future::Future;
use helios_hal::io::{IoError, IoResult};

use crate::features::{NegotiatedFeatures, RING_FEATURES, negotiate};
use crate::inflight::{InFlight, await_completion};
use crate::notify::Notify;
use crate::queue::VirtQueue;
use crate::transport::{DeviceStatus, DeviceType, VirtioTransport};

const REQUEST_QUEUE_INDEX: u16 = 0;
const REQUEST_QUEUE_SIZE: u16 = 16;
/// One read-only request buffer plus one writable response buffer.
const REQUEST_CHAIN_LIMIT: u16 = 2;
const HEADER_SIZE: usize = 7;

/// Thin async wrapper around a virtio-9p request queue.
///
/// Protocol encoding stays outside this type. The device layer only handles
/// feature negotiation, mount-tag discovery, queue submission and interrupt
/// wakeups.
pub struct Virtio9pDevice<T: VirtioTransport> {
    transport: T,
    queue: Mutex<VirtQueue<T>>,
    inflight: InFlight<{ REQUEST_QUEUE_SIZE as usize }>,
    interrupts: Notify,
    features: NegotiatedFeatures,
    mount_tag: String,
}

impl<T: VirtioTransport> Virtio9pDevice<T> {
    pub fn new(transport: T) -> IoResult<Self> {
        if transport.device_type() != DeviceType::_9P {
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
        let mount_tag = read_mount_tag(&transport)?;

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
            mount_tag,
        })
    }

    pub fn mount_tag(&self) -> &str {
        &self.mount_tag
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

    pub async fn request(&self, request: &[u8], response: &mut [u8]) -> IoResult<u32> {
        self.request_with_wait(request, response, || self.interrupts.notified())
            .await
    }

    pub async fn request_with_wait<Wait, WaitFuture>(
        &self,
        request: &[u8],
        response: &mut [u8],
        wait: Wait,
    ) -> IoResult<u32>
    where
        Wait: FnMut() -> WaitFuture,
        WaitFuture: Future<Output = ()>,
    {
        if request.is_empty() || response.len() < HEADER_SIZE {
            return Err(IoError::InvalidBufferLength {
                required_multiple: HEADER_SIZE,
                actual: response.len(),
            });
        }

        let token = {
            let mut queue = self.queue.lock().await;
            let token = queue.submit(&self.transport, &[request], &mut [response])?;
            queue.notify(&self.transport);
            // Claiming the slot under the queue lock is what makes the
            // completion reachable: no other task can drain this token
            // before the waiter exists.
            self.inflight.register(token);
            token
        };

        // Completions are routed by descriptor identifier: with EVENT_IDX
        // and IN_ORDER the device may finish requests in an order that
        // has nothing to do with which task is awake.
        let used_len =
            await_completion(&self.inflight, &self.queue, &self.interrupts, token, wait).await;

        let header_len = u32::from_le_bytes([response[0], response[1], response[2], response[3]]);
        if header_len != used_len {
            return Err(IoError::InvalidDeviceConfig(
                "virtio-9p response length header did not match the used ring length",
            ));
        }

        Ok(used_len)
    }
}

impl<T: VirtioTransport> Drop for Virtio9pDevice<T> {
    fn drop(&mut self) {
        self.queue.get_mut().shutdown(&self.transport);
    }
}

fn read_mount_tag<T: VirtioTransport>(transport: &T) -> IoResult<String> {
    let tag_len = u16::from_le_bytes([transport.read_config_u8(0), transport.read_config_u8(1)]);
    if tag_len == 0 {
        return Err(IoError::InvalidDeviceConfig(
            "virtio-9p mount tag length was zero",
        ));
    }

    let mut bytes = vec![0_u8; usize::from(tag_len)];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = transport.read_config_u8(2 + index);
    }

    String::from_utf8(bytes)
        .map_err(|_| IoError::InvalidDeviceConfig("virtio-9p mount tag was not valid utf-8"))
}
