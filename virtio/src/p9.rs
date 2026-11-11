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
//! the used ring on behalf of every waiter.
//!
//! The caller's request and response buffers move into the request and
//! the device reads and writes them in place, so nothing is copied. They
//! park with the chain's head descriptor until the device returns the
//! chain (see [`InFlight`]'s "Parked buffers"): a completed request hands
//! both back, and one whose future is dropped mid-flight leaves them with
//! the device until its completion frees them.

use alloc::string::String;
use alloc::vec;
use core::alloc::Layout;

use async_lock::Mutex;
use helios_hal::dma::{BufError, BufResult, DmaMemory};
use helios_hal::io::{IoError, IoResult};

use crate::bus::DeviceBus;
use crate::features::{NegotiatedFeatures, RING_FEATURES, negotiate};
use crate::inflight::{InFlight, await_completion, submit_lent};
use crate::notify::Notify;
use crate::queue::{ControlSlots, Part, TransportBuffer, VirtQueue, control_slots};
use crate::transport::{DeviceStatus, DeviceType, VirtioTransport};

/// Alignment of a request or reply buffer a consumer allocates: a cache
/// line of the target, so two buffers never share one.
const BUFFER_ALIGNMENT: usize = core::mem::align_of::<crossbeam_utils::CachePadded<u8>>();

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
    /// Completion slots, each parking the request and response buffer of
    /// the chain it heads.
    inflight: InFlight<{ REQUEST_QUEUE_SIZE as usize }, [TransportBuffer<T>; 2]>,
    /// A 9p chain carries no control bytes: both of its buffers are the
    /// caller's, lent. The table is empty and exists because every
    /// submission is staged through one.
    slots: ControlSlots<TransportBuffer<T>>,
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
        let slots = control_slots(&transport, &queue, 0, 0)?;
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
            slots,
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

    /// Allocates a zeroed buffer of `capacity` bytes the device can reach,
    /// for a request to be encoded into or a reply to land in.
    pub fn buffer(&self, capacity: usize) -> IoResult<TransportBuffer<T>> {
        let layout = Layout::from_size_align(capacity, BUFFER_ALIGNMENT)
            .map_err(|_| IoError::OutOfBounds)?;
        self.transport.bus().dma().allocate_zeroed(layout)
    }

    /// Sends the first `request_len` bytes of `request` and lets the
    /// device write up to `response_len` bytes of reply into the head of
    /// `response`. Resolves with the number of bytes the device wrote and
    /// both buffers, as `(request, response)`.
    ///
    /// Both buffers come from [`Virtio9pDevice::buffer`] and move into the
    /// request, so the device reads and writes them where they are and
    /// nothing is copied. They come back with every result except one: a
    /// request in flight when the queue faulted leaves them with the
    /// device, which may still be using them.
    pub async fn request(
        &self,
        request: TransportBuffer<T>,
        request_len: usize,
        response: TransportBuffer<T>,
        response_len: usize,
    ) -> BufResult<usize, (TransportBuffer<T>, TransportBuffer<T>)> {
        let returned = |error, [request, response]: [TransportBuffer<T>; 2]| {
            Err(BufError::returned(error, (request, response)))
        };
        let lent = [request, response];
        if request_len == 0 || request_len > lent[0].as_ref().len() {
            return returned(IoError::OutOfBounds, lent);
        }
        if response_len < HEADER_SIZE || response_len > lent[1].as_ref().len() {
            return returned(
                IoError::InvalidBufferLength {
                    required_multiple: HEADER_SIZE,
                    actual: response_len,
                },
                lent,
            );
        }

        let parts = [
            Part::LentRead {
                buffer: 0,
                range: 0..request_len,
            },
            Part::LentWrite {
                buffer: 1,
                range: 0..response_len,
            },
        ];
        let ticket = match submit_lent(
            &self.inflight,
            &self.queue,
            &self.transport,
            &self.slots,
            &parts,
            lent,
        )
        .await
        {
            Ok(ticket) => ticket,
            Err(error) => {
                return Err(error.map_buffers(|[request, response]| (request, response)));
            }
        };

        // Completions are routed by descriptor identifier: with EVENT_IDX
        // and IN_ORDER the device may finish requests in an order that
        // has nothing to do with which task is awake.
        let reply = match await_completion(&self.queue, ticket, || self.interrupts.notified()).await
        {
            Ok(reply) => reply,
            // The chain never came back: its buffers stay parked with it.
            Err(error) => {
                return Err(BufError {
                    error,
                    buffers: None,
                });
            }
        };
        let used_len = reply.written();
        let lent = reply.into_lent();

        let response = lent[1].as_ref();
        let header_len = u32::from_le_bytes([response[0], response[1], response[2], response[3]]);
        if header_len != used_len {
            return returned(
                IoError::InvalidDeviceConfig(
                    "virtio-9p response length header did not match the used ring length",
                ),
                lent,
            );
        }
        match usize::try_from(used_len) {
            Ok(used) => {
                let [request, response] = lent;
                Ok((used, (request, response)))
            }
            Err(_) => returned(IoError::DeviceFault, lent),
        }
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
    use crate::testing::{
        ConfinedDmaBuffer, ConfinedTransport, FakeTransport, FakeTransportConfig,
    };
    use crate::transport::VirtioFeatures;
    use core::pin::pin;
    use futures_lite::future::{block_on, poll_once};
    use helios_hal::dma::DmaBuffer;

    const MOUNT_TAG: &str = "helios";

    /// A transport whose config space already carries `MOUNT_TAG` and
    /// which offers exactly the features a 9p device must have. Its
    /// device reaches only what its own pool handed out, so a request
    /// that named caller memory instead of a buffer from
    /// [`Virtio9pDevice::buffer`] would be refused.
    fn transport(queue_size: u16) -> ConfinedTransport {
        let transport = FakeTransport::confined(FakeTransportConfig {
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
    fn reply(device: &Virtio9pDevice<ConfinedTransport>, len: usize) -> ConfinedDmaBuffer {
        let mut buffer = device.buffer(len).expect("a reply buffer");
        buffer.as_mut()[..4]
            .copy_from_slice(&u32::try_from(len).expect("reply fits in u32").to_le_bytes());
        buffer
    }

    /// A request buffer of `len` bytes of `fill`.
    fn request(
        device: &Virtio9pDevice<ConfinedTransport>,
        len: usize,
        fill: u8,
    ) -> ConfinedDmaBuffer {
        let mut buffer = device.buffer(len).expect("a request buffer");
        buffer.as_mut().fill(fill);
        buffer
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
        let [first_request, second_request, third_request] = [
            request(&device, 16, 1),
            request(&device, 16, 2),
            request(&device, 16, 3),
        ];
        let [first_response, second_response, third_response] =
            [reply(&device, 24), reply(&device, 32), reply(&device, 40)];

        let mut tokens = [0_u16; 3];
        // All three are submitted before any of them completes, so the
        // queue lock cannot have been held across an await.
        tokens[0] = device
            .queue
            .try_lock()
            .expect("the queue is free before the first request")
            .next_free_descriptor();
        let mut first = pin!(device.request(first_request, 16, first_response, 24));
        assert!(block_on(poll_once(first.as_mut())).is_none());
        tokens[1] = device
            .queue
            .try_lock()
            .expect("the queue lock is released once a request is in flight")
            .next_free_descriptor();
        let mut second = pin!(device.request(second_request, 16, second_response, 32));
        assert!(block_on(poll_once(second.as_mut())).is_none());
        tokens[2] = device
            .queue
            .try_lock()
            .expect("the queue lock is released once a request is in flight")
            .next_free_descriptor();
        let mut third = pin!(device.request(third_request, 16, third_response, 40));
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
            written(block_on(poll_once(second.as_mut()))),
            Some(32),
            "each waiter resolves with its own reply length"
        );
        assert_eq!(written(block_on(poll_once(third.as_mut()))), Some(40));
        assert_eq!(written(block_on(poll_once(first.as_mut()))), Some(24));
    }

    /// The reply length a finished request reports, `None` while it is
    /// still pending. A request that failed fails the test.
    fn written(
        outcome: Option<BufResult<usize, (ConfinedDmaBuffer, ConfinedDmaBuffer)>>,
    ) -> Option<usize> {
        outcome.map(|result| result.expect("the request succeeds").0)
    }

    #[test]
    fn a_full_ring_parks_the_submitter_until_a_chain_is_recycled() {
        let device = Virtio9pDevice::new(transport(4)).expect("the device should initialize");
        assert_eq!(device.pipeline_depth(), 2);
        let [first_request, second_request, third_request] = [
            request(&device, 8, 1),
            request(&device, 8, 2),
            request(&device, 8, 3),
        ];
        let [first_response, second_response, third_response] =
            [reply(&device, 12), reply(&device, 16), reply(&device, 20)];

        let first_token = device
            .queue
            .try_lock()
            .expect("the queue is free")
            .next_free_descriptor();
        let mut first = pin!(device.request(first_request, 8, first_response, 12));
        assert!(block_on(poll_once(first.as_mut())).is_none());
        let mut second = pin!(device.request(second_request, 8, second_response, 16));
        assert!(block_on(poll_once(second.as_mut())).is_none());

        // The ring is full: the third request must wait rather than fail.
        let mut third = pin!(device.request(third_request, 8, third_response, 20));
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
            written(block_on(poll_once(first.as_mut()))),
            Some(12),
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

    /// A guest that cancels a host-share read drops the request future
    /// while the device is still filling the reply. The buffers must stay
    /// the device's until it hands the chain back, and the queue must
    /// carry the next request.
    #[test]
    fn a_dropped_request_keeps_its_buffers_until_the_device_completes_it() {
        let device = Virtio9pDevice::new(transport(8)).expect("the device should initialize");
        let pool = device.transport.pool().clone();
        let abandoned_token = device
            .queue
            .try_lock()
            .expect("the queue is free")
            .next_free_descriptor();
        let reply_bytes = {
            let request = request(&device, 16, 1);
            let response = device.buffer(64).expect("a reply buffer");
            let reply_bytes = (response.as_ptr() as u64, 64);
            let mut abandoned = pin!(device.request(request, 16, response, 64));
            assert!(block_on(poll_once(abandoned.as_mut())).is_none());
            reply_bytes
        };
        assert!(
            !pool.is_released(reply_bytes.0, reply_bytes.1),
            "the reply buffer of a dropped request is still the device's"
        );

        // The device finishes the read it was given, after the request is
        // gone: its reply lands in memory nobody has reused.
        let mut late = vec![0_u8; 64];
        late[..4].copy_from_slice(&64_u32.to_le_bytes());
        {
            let queue = device.queue.try_lock().expect("the queue is free");
            let written = pool.respond_into_live_memory(&queue, abandoned_token, &late);
            queue.device_complete(abandoned_token, written);
        }
        device.handle_interrupt();

        // The next request drains that completion, which frees the parked
        // buffers, and goes through on the same queue.
        let next_token = device
            .queue
            .try_lock()
            .expect("the queue is free")
            .next_free_descriptor();
        let mut next = pin!(device.request(request(&device, 16, 2), 16, reply(&device, 24), 24));
        assert!(block_on(poll_once(next.as_mut())).is_none());
        assert!(
            pool.is_released(reply_bytes.0, reply_bytes.1),
            "the device's completion is what frees the dropped request's buffers"
        );
        assert!(device.inflight.is_idle(abandoned_token));

        device
            .queue
            .try_lock()
            .expect("the queue is free")
            .device_complete(next_token, 24);
        device.handle_interrupt();
        assert_eq!(
            written(block_on(poll_once(next.as_mut()))),
            Some(24),
            "the queue is not stalled by the abandoned chain"
        );
    }

    #[test]
    fn a_request_the_device_never_saw_hands_its_buffers_back() {
        let device = Virtio9pDevice::new(transport(8)).expect("the device should initialize");
        let refused = block_on(device.request(
            request(&device, 16, 1),
            16,
            device.buffer(4).expect("a reply buffer"),
            4,
        ))
        .err()
        .expect("a reply shorter than a 9p header is refused");
        assert!(matches!(refused.error, IoError::InvalidBufferLength { .. }));
        let (request, response) = refused.buffers.expect("the buffers come back");
        assert_eq!(request.as_ref(), [1_u8; 16]);
        assert_eq!(response.as_ref().len(), 4);
    }
}
