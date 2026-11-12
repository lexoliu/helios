//! virtio-9p transport queue.
//!
//! Concurrency contract: [`Virtio9pDevice::request`] is a multi-flight
//! entry point. The queue mutex is taken only long enough to place a
//! chain in the ring and claim its completion slot; it is never held
//! across an await, so up to [`Virtio9pDevice::pipeline_depth`] requests
//! from different tasks are in flight at once and complete in whatever
//! order the device chooses. A caller that finds no room for its chain
//! parks on the device interrupt until a completion frees descriptors
//! rather than failing. Completions are routed by descriptor identifier
//! through [`InFlight`], so whichever task wins the queue lock drains
//! the used ring on behalf of every waiter. The caller's request and
//! response buffers stay borrowed for the lifetime of the future, so
//! the device writes straight into them and nothing is copied.

use alloc::string::String;
use alloc::vec;

use async_lock::Mutex;
use helios_hal::io::{IoError, IoResult};

use crate::features::{NegotiatedFeatures, RING_FEATURES, negotiate, renegotiate};
use crate::inflight::{
    InFlight, await_completion, drain_to_idle, reinitialize_requests, submit_chain,
};
use crate::notify::Notify;
use crate::queue::VirtQueue;
use crate::resume::{DeviceReset, QuiesceError, ReinitError, conclude, driver_ok};
use crate::transport::{DeviceStatus, DeviceType, VirtioTransport};

const REQUEST_QUEUE_INDEX: u16 = 0;
const REQUEST_QUEUE_SIZE: u16 = 16;
/// One read-only request buffer plus one writable response buffer.
const REQUEST_CHAIN_BUFFERS: usize = 2;
const HEADER_SIZE: usize = 7;
/// VIRTIO_9P_MOUNT_TAG: the device exposes the mount tag its config
/// space carries. Without it the config space holds nothing the driver
/// may read, so the host share cannot be named.
const P9_FEATURE_MOUNT_TAG: u64 = 1 << 0;

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
    queue_size: u16,
    mount_tag: String,
}

impl<T: VirtioTransport> Virtio9pDevice<T> {
    pub fn new(transport: T) -> IoResult<Self> {
        if transport.device_type() != DeviceType::_9P {
            return Err(IoError::Unsupported);
        }

        let features = negotiate(&transport, RING_FEATURES | P9_FEATURE_MOUNT_TAG)?;
        if !features.device(P9_FEATURE_MOUNT_TAG) {
            return Err(IoError::InvalidDeviceConfig(
                "virtio-9p device did not accept VIRTIO_9P_MOUNT_TAG",
            ));
        }

        let queue_size = transport
            .queue_max_size(REQUEST_QUEUE_INDEX)
            .min(REQUEST_QUEUE_SIZE);
        if queue_size == 0 || !queue_size.is_power_of_two() {
            return Err(IoError::Unsupported);
        }

        let chain_limit =
            u16::try_from(REQUEST_CHAIN_BUFFERS).expect("the 9p chain limit fits in u16");
        let queue = VirtQueue::new(
            &transport,
            REQUEST_QUEUE_INDEX,
            queue_size,
            chain_limit,
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
            queue_size,
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

    /// How many requests this device carries concurrently before a
    /// submission has to wait for a completion.
    ///
    /// A 9p request is a two-buffer chain, which costs a single ring
    /// descriptor when VIRTIO_F_INDIRECT_DESC was negotiated and one
    /// descriptor per buffer otherwise. Clients size their request
    /// pipeline — and the buffer pools that feed it — from this.
    pub fn pipeline_depth(&self) -> usize {
        let per_request = if self.features.indirect() {
            1
        } else {
            REQUEST_CHAIN_BUFFERS
        };
        usize::from(self.queue_size) / per_request
    }

    /// Interrupt handlers should only acknowledge the device and wake waiters.
    pub fn handle_interrupt(&self) {
        self.transport.ack_interrupt();
        self.interrupts.notify_all();
    }

    /// Waits for every request the device owns to complete, so the device
    /// may be reset. See [`crate::resume`] for the contract; `expired`
    /// says when the caller's deadline has passed.
    pub fn quiesce(&self, mut expired: impl FnMut() -> bool) -> Result<(), QuiesceError> {
        drain_to_idle(
            &self.inflight,
            &self.queue,
            &self.transport,
            REQUEST_QUEUE_INDEX,
            &mut expired,
        )
    }

    /// Brings a reset device back with the features it had, after
    /// [`Self::quiesce`].
    ///
    /// The mount tag is read again and must be the one the share was
    /// mounted under. The returned [`DeviceReset`] is the signal the 9p
    /// client reacts to: a reset device is a server that has forgotten
    /// every session and every fid, and a request that completed before
    /// the reset names state that no longer exists.
    pub fn reinitialize(&self) -> Result<DeviceReset, ReinitError> {
        conclude(&self.transport, self.restore(), |resumed| {
            self.inflight.resume(resumed);
        })
    }

    fn restore(&self) -> Result<DeviceReset, ReinitError> {
        renegotiate(&self.transport, self.features)?;
        reinitialize_requests(&self.queue, &self.transport, REQUEST_QUEUE_INDEX)?;
        // A device is untrusted: what it reads back is compared. The tag
        // is the share's name, and a different one is a different share.
        let tag = read_mount_tag(&self.transport).map_err(|error| ReinitError::Restore {
            device: DeviceType::_9P,
            what: "the mount tag",
            error,
        })?;
        if tag != self.mount_tag {
            return Err(ReinitError::DeviceChanged {
                device: DeviceType::_9P,
                what: "the mount tag",
            });
        }
        driver_ok(&self.transport);
        Ok(DeviceReset {
            device: DeviceType::_9P,
        })
    }

    pub async fn request(&self, request: &[u8], response: &mut [u8]) -> IoResult<u32> {
        if request.is_empty() || response.len() < HEADER_SIZE {
            return Err(IoError::InvalidBufferLength {
                required_multiple: HEADER_SIZE,
                actual: response.len(),
            });
        }

        let token = submit_chain(
            &self.inflight,
            &self.queue,
            &self.transport,
            &[request],
            &mut [response],
        )
        .await?;

        // Completions are routed by descriptor identifier: with EVENT_IDX
        // and IN_ORDER the device may finish requests in an order that
        // has nothing to do with which task is awake.
        let used_len = await_completion(&self.inflight, &self.queue, token, || {
            self.interrupts.notified()
        })
        .await?;

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
    let tag_len = transport.read_config_u16(0);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::DeviceBus;
    use crate::features::RenegotiateError;
    use crate::testing::{FakeTransport, FakeTransportConfig};
    use crate::transport::VirtioFeatures;
    use core::pin::pin;
    use futures_lite::future::{block_on, poll_once};

    const MOUNT_TAG: &str = "helios";

    /// A transport whose config space already carries `MOUNT_TAG` and
    /// which offers exactly the features a 9p device must have.
    fn transport(queue_size: u16) -> FakeTransport {
        let transport = FakeTransport::new(FakeTransportConfig {
            device_type: DeviceType::_9P,
            offered_features: VirtioFeatures::VERSION_1.bits() | P9_FEATURE_MOUNT_TAG,
            queue_size,
            supports_queue_reset: true,
            absent_queues: &[],
        });
        let mut config = [0_u8; 32];
        let tag_len = u16::try_from(MOUNT_TAG.len()).expect("mount tag length fits in u16");
        config[..2].copy_from_slice(&tag_len.to_le_bytes());
        config[2..2 + MOUNT_TAG.len()].copy_from_slice(MOUNT_TAG.as_bytes());
        for (index, word) in config.chunks_exact(4).enumerate() {
            transport.bus().write_u32(
                index * 4,
                u32::from_le_bytes([word[0], word[1], word[2], word[3]]),
            );
        }
        transport
    }

    /// A 9p response buffer holding the reply header the device would
    /// have written, so the driver's length check sees a consistent
    /// message.
    fn reply(len: usize) -> alloc::vec::Vec<u8> {
        let mut bytes = vec![0_u8; len];
        bytes[..4].copy_from_slice(&u32::try_from(len).expect("reply fits in u32").to_le_bytes());
        bytes
    }

    #[test]
    fn a_device_that_refuses_the_mount_tag_feature_is_rejected() {
        let transport = FakeTransport::new(FakeTransportConfig {
            device_type: DeviceType::_9P,
            offered_features: VirtioFeatures::VERSION_1.bits(),
            ..FakeTransportConfig::default()
        });

        let error = match Virtio9pDevice::new(transport) {
            Ok(_) => panic!("a 9p device without a mount tag must be rejected"),
            Err(error) => error,
        };

        assert!(matches!(error, IoError::InvalidDeviceConfig(_)));
    }

    #[test]
    fn the_mount_tag_is_read_once_the_feature_is_negotiated() {
        let device = Virtio9pDevice::new(transport(8)).expect("the device should initialize");

        assert_eq!(device.mount_tag(), MOUNT_TAG);
        assert_eq!(
            device.pipeline_depth(),
            4,
            "two descriptors per request without indirect descriptors"
        );
    }

    #[test]
    fn three_requests_are_in_flight_together_and_complete_out_of_order() {
        let device = Virtio9pDevice::new(transport(8)).expect("the device should initialize");
        let requests = [[1_u8; 16], [2_u8; 16], [3_u8; 16]];
        let mut responses = [reply(24), reply(32), reply(40)];
        let [first_response, second_response, third_response] = &mut responses;

        let mut tokens = [0_u16; 3];
        // All three are submitted before any of them completes, so the
        // queue lock cannot have been held across an await.
        tokens[0] = device
            .queue
            .try_lock()
            .expect("the queue is free before the first request")
            .next_free_descriptor();
        let mut first = pin!(device.request(&requests[0], first_response));
        assert!(block_on(poll_once(first.as_mut())).is_none());
        tokens[1] = device
            .queue
            .try_lock()
            .expect("the queue lock is released once a request is in flight")
            .next_free_descriptor();
        let mut second = pin!(device.request(&requests[1], second_response));
        assert!(block_on(poll_once(second.as_mut())).is_none());
        tokens[2] = device
            .queue
            .try_lock()
            .expect("the queue lock is released once a request is in flight")
            .next_free_descriptor();
        let mut third = pin!(device.request(&requests[2], third_response));
        assert!(block_on(poll_once(third.as_mut())).is_none());
        assert_eq!(
            device
                .queue
                .try_lock()
                .expect("the queue is free")
                .available_descriptors(),
            2,
            "three two-descriptor chains are outstanding"
        );

        // The device finishes them in an order of its own choosing.
        {
            let queue = device.queue.try_lock().expect("the queue is free");
            queue.device_complete(tokens[2], 40);
            queue.device_complete(tokens[0], 24);
            queue.device_complete(tokens[1], 32);
        }
        device.handle_interrupt();

        assert_eq!(
            block_on(poll_once(second.as_mut())),
            Some(Ok(32)),
            "each waiter resolves with its own reply length"
        );
        assert_eq!(block_on(poll_once(third.as_mut())), Some(Ok(40)));
        assert_eq!(block_on(poll_once(first.as_mut())), Some(Ok(24)));
    }

    /// Plays the device answering `token` with a reply of `len` bytes.
    fn answer(device: &Virtio9pDevice<FakeTransport>, token: u16, len: u32) {
        device
            .queue
            .try_lock()
            .expect("a parked driver does not hold the queue lock")
            .device_complete(token, len);
        device.handle_interrupt();
    }

    #[test]
    fn quiesce_waits_for_every_request_in_flight() {
        let device = Virtio9pDevice::new(transport(8)).expect("the device should initialize");
        let requests = [[1_u8; 16], [2_u8; 16]];
        let mut responses = [reply(24), reply(32)];
        let [first_response, second_response] = &mut responses;
        let mut first = pin!(device.request(&requests[0], first_response));
        assert!(block_on(poll_once(first.as_mut())).is_none());
        let mut second = pin!(device.request(&requests[1], second_response));
        assert!(block_on(poll_once(second.as_mut())).is_none());

        // A two-buffer chain takes two descriptors, so the second request's
        // head is descriptor 2.
        let mut pending = [(2_u16, 32_u32), (0_u16, 24_u32)].into_iter();
        let mut passes = 0;
        device
            .quiesce(|| {
                passes += 1;
                if let Some((token, len)) = pending.next() {
                    answer(&device, token, len);
                }
                false
            })
            .expect("the device answers inside the deadline");
        assert_eq!(passes, 2, "one pass per answer, out of order");

        assert_eq!(block_on(poll_once(first.as_mut())), Some(Ok(24)));
        assert_eq!(block_on(poll_once(second.as_mut())), Some(Ok(32)));
    }

    #[test]
    fn quiesce_gives_up_when_the_deadline_passes() {
        let device = Virtio9pDevice::new(transport(8)).expect("the device should initialize");
        let request = [1_u8; 16];
        let mut response = reply(24);
        let mut pending = pin!(device.request(&request, &mut response));
        assert!(block_on(poll_once(pending.as_mut())).is_none());

        assert_eq!(
            device.quiesce(|| true),
            Err(QuiesceError::RequestsStillInFlight {
                device: DeviceType::_9P,
                queue: 0,
                count: 1,
            })
        );
    }

    #[test]
    fn a_reset_device_serves_requests_again_and_signals_the_reset() {
        let device = Virtio9pDevice::new(transport(8)).expect("the device should initialize");
        let request = [1_u8; 16];
        let mut response = reply(24);
        let mut pending = pin!(device.request(&request, &mut response));
        assert!(block_on(poll_once(pending.as_mut())).is_none());
        device
            .quiesce(|| {
                answer(&device, 0, 24);
                false
            })
            .expect("the device answers inside the deadline");

        device.transport.reset_behind_the_driver();
        assert_eq!(
            device.reinitialize(),
            Ok(DeviceReset {
                device: DeviceType::_9P
            })
        );

        assert_eq!(
            block_on(poll_once(pending.as_mut())),
            Some(Ok(24)),
            "the reply that arrived before the reset is still delivered"
        );
        let programmed = device.transport.programmed_queues();
        assert_eq!(programmed.len(), 2);
        assert_eq!(programmed[0], programmed[1]);
        let mut again = reply(12);
        let mut round_trip = pin!(device.request(&request, &mut again));
        assert!(block_on(poll_once(round_trip.as_mut())).is_none());
        answer(&device, 0, 12);
        assert_eq!(block_on(poll_once(round_trip.as_mut())), Some(Ok(12)));
    }

    #[test]
    fn a_device_that_offers_fewer_features_after_the_reset_is_refused() {
        let device = Virtio9pDevice::new(transport(8)).expect("the device should initialize");
        device.transport.withdraw_features(P9_FEATURE_MOUNT_TAG);

        assert_eq!(
            device.reinitialize(),
            Err(ReinitError::Renegotiate(
                RenegotiateError::FeatureWithdrawn {
                    device: DeviceType::_9P,
                    bit: 0,
                }
            ))
        );
    }

    #[test]
    fn a_device_that_serves_another_share_after_the_reset_is_refused() {
        let device = Virtio9pDevice::new(transport(8)).expect("the device should initialize");
        // The tag's first byte sits two bytes into the configuration.
        device.transport.set_config_u8(2, b'x');

        assert_eq!(
            device.reinitialize(),
            Err(ReinitError::DeviceChanged {
                device: DeviceType::_9P,
                what: "the mount tag",
            })
        );
    }

    /// A device refused after its reset is FAILED, and a request made
    /// afterwards fails at once instead of waiting for a completion that
    /// cannot come.
    #[test]
    fn a_refused_device_fails_the_requests_that_follow() {
        let device = Virtio9pDevice::new(transport(8)).expect("the device should initialize");
        device.transport.set_config_u8(2, b'x');
        assert!(matches!(
            device.reinitialize(),
            Err(ReinitError::DeviceChanged { .. })
        ));
        assert!(
            device
                .transport
                .status()
                .contains(crate::transport::DeviceStatus::FAILED)
        );

        let request = [1_u8; 16];
        let mut response = reply(24);
        assert_eq!(
            block_on(device.request(&request, &mut response)),
            Err(IoError::DeviceFault)
        );
    }

    #[test]
    fn a_full_ring_parks_the_submitter_until_a_chain_is_recycled() {
        let device = Virtio9pDevice::new(transport(4)).expect("the device should initialize");
        assert_eq!(device.pipeline_depth(), 2);
        let requests = [[1_u8; 8], [2_u8; 8], [3_u8; 8]];
        let mut responses = [reply(12), reply(16), reply(20)];
        let [first_response, second_response, third_response] = &mut responses;

        let first_token = device
            .queue
            .try_lock()
            .expect("the queue is free")
            .next_free_descriptor();
        let mut first = pin!(device.request(&requests[0], first_response));
        assert!(block_on(poll_once(first.as_mut())).is_none());
        let mut second = pin!(device.request(&requests[1], second_response));
        assert!(block_on(poll_once(second.as_mut())).is_none());

        // The ring is full: the third request must wait rather than fail.
        let mut third = pin!(device.request(&requests[2], third_response));
        assert!(block_on(poll_once(third.as_mut())).is_none());
        assert_eq!(
            device
                .queue
                .try_lock()
                .expect("the queue is free")
                .available_descriptors(),
            0
        );

        device
            .queue
            .try_lock()
            .expect("the queue is free")
            .device_complete(first_token, 12);
        device.handle_interrupt();

        assert_eq!(
            block_on(poll_once(first.as_mut())),
            Some(Ok(12)),
            "the interrupt belongs to the waiter, which drains and collects its reply"
        );
        assert!(
            block_on(poll_once(third.as_mut())).is_none(),
            "the freed chain wakes the submitter, which then waits for its own reply"
        );
        assert_eq!(
            device
                .queue
                .try_lock()
                .expect("the queue is free")
                .available_descriptors(),
            0,
            "the third request took the recycled chain"
        );
        assert!(block_on(poll_once(second.as_mut())).is_none());
    }
}
