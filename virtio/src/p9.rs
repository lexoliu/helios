use alloc::string::String;
use alloc::vec;

use async_lock::Mutex;
use helios_hal::io::{IoError, IoResult};

use crate::notify::Notify;
use crate::queue::VirtQueue;
use crate::transport::{DeviceStatus, DeviceType, VirtioFeatures, VirtioTransport};

const REQUEST_QUEUE_INDEX: u16 = 0;
const REQUEST_QUEUE_SIZE: u16 = 16;
const HEADER_SIZE: usize = 7;

/// Thin async wrapper around a virtio-9p request queue.
///
/// Protocol encoding stays outside this type. The device layer only handles
/// feature negotiation, mount-tag discovery, queue submission and interrupt
/// wakeups.
pub struct Virtio9pDevice<T: VirtioTransport> {
    transport: T,
    queue: Mutex<VirtQueue<T>>,
    interrupts: Notify,
    mount_tag: String,
}

impl<T: VirtioTransport> Virtio9pDevice<T> {
    pub fn new(transport: T) -> IoResult<Self> {
        if transport.device_type() != DeviceType::_9P {
            return Err(IoError::Unsupported);
        }

        transport.reset();
        transport.set_status(DeviceStatus::ACKNOWLEDGE);
        transport.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER);

        let offered = transport.device_features();
        let accepted = offered
            & (VirtioFeatures::VERSION_1.bits() | VirtioFeatures::RING_INDIRECT_DESC.bits());
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
            interrupts: Notify::new(),
            mount_tag,
        })
    }

    pub fn mount_tag(&self) -> &str {
        &self.mount_tag
    }

    /// Interrupt handlers should only acknowledge the device and wake waiters.
    pub fn handle_interrupt(&self) {
        self.transport.ack_interrupt();
        self.interrupts.notify_one();
    }

    pub async fn request(&self, request: &[u8], response: &mut [u8]) -> IoResult<u32> {
        if request.is_empty() || response.len() < HEADER_SIZE {
            return Err(IoError::InvalidBufferLength {
                required_multiple: HEADER_SIZE,
                actual: response.len(),
            });
        }

        let mut queue = self.queue.lock().await;
        let token = queue.submit(&[request], &mut [response])?;
        queue.notify(&self.transport);

        let used_len = loop {
            if let Some((used, len)) = queue.pop_used_with_len() {
                assert_eq!(used, token, "virtio 9p completion token mismatch");
                break len;
            }

            self.interrupts.notified().await;
        };

        let header_len = u32::from_le_bytes([response[0], response[1], response[2], response[3]]);
        if header_len != used_len {
            return Err(IoError::InvalidDeviceConfig(
                "virtio-9p response length header did not match the used ring length",
            ));
        }

        Ok(used_len)
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
