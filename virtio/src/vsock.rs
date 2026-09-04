//! virtio-vsock driver.
//!
//! The device carries whole vsock packets: a fixed 44-byte header
//! followed by at most [`VSOCK_MAX_PAYLOAD_BYTES`] payload bytes. Three queues
//! serve it — receive, transmit, and an event queue the device uses to
//! announce that the host end was replaced underneath a live connection.
//!
//! Receive memory rule, as for virtio-net: every buffer the device
//! writes into is allocated once, at bring-up, and recycled for the
//! lifetime of the device. A receive slot is one page handed to the
//! device as a single writable descriptor; a delivered packet is copied
//! into the caller's buffer and the slot goes straight back to the ring.
//! Copying rather than lending the slot out is deliberate here: a vsock
//! payload is bounded by the connection's credit window and is consumed
//! by the connection table immediately, so an owning receive frame would
//! buy nothing and would let one stalled connection pin receive slots.
//!
//! Concurrency contract: `send` is a multi-flight entry point — the
//! transmit queue lock is taken only long enough to publish a chain and
//! claim its completion slot, so several tasks transmit concurrently and
//! completions are routed back by descriptor identifier. `receive_into`
//! serialises on the receive queue's own async mutex and parks on the
//! device interrupt when the ring is empty; it never holds the lock
//! across an await. Both are safe to call from any processor.

use alloc::boxed::Box;
use alloc::vec;
use async_lock::Mutex as AsyncMutex;
use core::sync::atomic::{AtomicBool, Ordering};

use helios_hal::io::{IoError, IoResult};
use helios_hal::vsock::{
    VsockAddress, VsockDelivery, VsockDevice, VsockOp, VsockPacketHeader, VsockReceived,
    VsockShutdown,
};

use crate::features::{NegotiatedFeatures, RING_FEATURES, negotiate};
use crate::inflight::{InFlight, await_completion, submit_chain};
use crate::notify::Notify;
use crate::queue::VirtQueue;
use crate::transport::{DeviceStatus, DeviceType, VirtioTransport};

const RX_QUEUE_INDEX: u16 = 0;
const TX_QUEUE_INDEX: u16 = 1;
const EVENT_QUEUE_INDEX: u16 = 2;

/// Depth the driver asks for on the receive and transmit queues.
///
/// The receive ring is also the driver's whole receive buffer pool — one
/// page per descriptor — so its depth is what bounds how much the host
/// may have in flight towards this machine before it has to wait for the
/// guest to repost.
const DATA_QUEUE_SIZE: u16 = 128;
/// The event queue only ever carries a handful of four-byte notices.
const EVENT_QUEUE_SIZE: u16 = 4;

/// A receive buffer is always a single writable descriptor.
const RX_CHAIN_LIMIT: u16 = 1;
/// A transmit chain is the packet header followed by its payload.
const TX_CHAIN_LIMIT: u16 = 2;
/// An event buffer is a single writable descriptor.
const EVENT_CHAIN_LIMIT: u16 = 1;

/// `struct virtio_vsock_hdr` (virtio 1.2 §5.10.6): ten little-endian
/// fields, two of them 16-bit.
pub const HEADER_BYTES: usize = 44;
/// Receive slot granularity. One page per descriptor keeps the pool's
/// arithmetic trivial and leaves the payload capacity a round number the
/// connection layer can advertise as its credit window.
const RX_SLOT_BYTES: usize = 4096;
/// Largest payload one packet carries in either direction.
pub const VSOCK_MAX_PAYLOAD_BYTES: usize = RX_SLOT_BYTES - HEADER_BYTES;
/// `struct virtio_vsock_event`: a single little-endian id.
const EVENT_BYTES: usize = 4;
/// `VIRTIO_VSOCK_EVENT_TRANSPORT_RESET`: the host end was replaced, so
/// every open connection is gone.
const EVENT_TRANSPORT_RESET: u32 = 0;

/// `VIRTIO_VSOCK_TYPE_STREAM`: the only socket type this driver carries.
const PACKET_TYPE_STREAM: u16 = 1;

/// VIRTIO_VSOCK_F_STREAM: the device confirms it carries stream sockets.
///
/// The bit was only introduced in virtio 1.2; a device that predates it
/// carries stream sockets unconditionally, so the driver asks for the
/// bit and treats its absence as "this device is older", not as a
/// refusal. Seqpacket is deliberately never requested: the connection
/// table implements stream semantics only.
const VSOCK_FEATURE_STREAM: u64 = 1 << 0;

/// Byte offset of `guest_cid` in the device configuration space.
const CONFIG_GUEST_CID_OFFSET: usize = 0;

/// Encodes a packet header into its 44 wire bytes.
fn encode_header(header: &VsockPacketHeader) -> [u8; HEADER_BYTES] {
    let mut bytes = [0_u8; HEADER_BYTES];
    bytes[0..8].copy_from_slice(&header.source.cid.to_le_bytes());
    bytes[8..16].copy_from_slice(&header.destination.cid.to_le_bytes());
    bytes[16..20].copy_from_slice(&header.source.port.to_le_bytes());
    bytes[20..24].copy_from_slice(&header.destination.port.to_le_bytes());
    bytes[24..28].copy_from_slice(&header.payload_len.to_le_bytes());
    bytes[28..30].copy_from_slice(&PACKET_TYPE_STREAM.to_le_bytes());
    bytes[30..32].copy_from_slice(&header.op.as_id().to_le_bytes());
    bytes[32..36].copy_from_slice(&header.flags.to_le_bytes());
    bytes[36..40].copy_from_slice(&header.buf_alloc.to_le_bytes());
    bytes[40..44].copy_from_slice(&header.fwd_cnt.to_le_bytes());
    bytes
}

/// Decodes the 44 wire bytes of a packet header.
///
/// A header naming a socket type this driver does not carry, or an
/// operation the specification does not define, is a device fault: vsock
/// has no "unknown packet" the driver could skip past, and a connection
/// table fed a half-decoded header would answer the wrong peer.
fn decode_header(bytes: &[u8]) -> IoResult<VsockPacketHeader> {
    let bytes: &[u8; HEADER_BYTES] = bytes
        .get(..HEADER_BYTES)
        .and_then(|slice| slice.try_into().ok())
        .ok_or(IoError::DeviceFault)?;
    let packet_type = u16::from_le_bytes([bytes[28], bytes[29]]);
    if packet_type != PACKET_TYPE_STREAM {
        return Err(IoError::DeviceFault);
    }
    let op =
        VsockOp::from_id(u16::from_le_bytes([bytes[30], bytes[31]])).ok_or(IoError::DeviceFault)?;
    Ok(VsockPacketHeader {
        source: VsockAddress {
            cid: u64::from_le_bytes(bytes[0..8].try_into().expect("eight bytes")),
            port: u32::from_le_bytes(bytes[16..20].try_into().expect("four bytes")),
        },
        destination: VsockAddress {
            cid: u64::from_le_bytes(bytes[8..16].try_into().expect("eight bytes")),
            port: u32::from_le_bytes(bytes[20..24].try_into().expect("four bytes")),
        },
        op,
        flags: u32::from_le_bytes(bytes[32..36].try_into().expect("four bytes")),
        payload_len: u32::from_le_bytes(bytes[24..28].try_into().expect("four bytes")),
        buf_alloc: u32::from_le_bytes(bytes[36..40].try_into().expect("four bytes")),
        fwd_cnt: u32::from_le_bytes(bytes[40..44].try_into().expect("four bytes")),
    })
}

/// The receive ring together with the buffer pool that backs it.
///
/// Slot bookkeeping lives beside the queue rather than behind its own
/// lock because every path that touches it already holds the receive
/// queue: a completion is drained, its slot copied out, and the slot
/// reposted before the lock is released.
struct VsockRxState<T: VirtioTransport> {
    queue: VirtQueue<T>,
    /// One page per descriptor, allocated at bring-up and recycled.
    slots: Box<[Box<[u8]>]>,
    /// Which slot each outstanding descriptor identifier carries.
    slot_for_token: Box<[u16]>,
}

/// The event ring and its buffers.
struct VsockEventState<T: VirtioTransport> {
    queue: VirtQueue<T>,
    buffers: Box<[Box<[u8]>]>,
    buffer_for_token: Box<[u16]>,
}

pub struct VirtioVsockDevice<T: VirtioTransport> {
    transport: T,
    guest_cid: u64,
    rx: AsyncMutex<VsockRxState<T>>,
    tx_queue: AsyncMutex<VirtQueue<T>>,
    tx_inflight: InFlight<{ DATA_QUEUE_SIZE as usize }>,
    event: AsyncMutex<VsockEventState<T>>,
    interrupts: Notify,
    features: NegotiatedFeatures,
    /// Set when the device announced `VIRTIO_VSOCK_EVENT_TRANSPORT_RESET`
    /// and not yet collected by the connection table.
    transport_reset: AtomicBool,
}

impl<T: VirtioTransport> VirtioVsockDevice<T> {
    pub fn new(transport: T) -> IoResult<Self> {
        if transport.device_type() != DeviceType::Vsock {
            return Err(IoError::Unsupported);
        }

        let features = negotiate(&transport, RING_FEATURES | VSOCK_FEATURE_STREAM)?;

        let rx_size = queue_size(&transport, RX_QUEUE_INDEX, DATA_QUEUE_SIZE)?;
        let tx_size = queue_size(&transport, TX_QUEUE_INDEX, DATA_QUEUE_SIZE)?;
        let event_size = queue_size(&transport, EVENT_QUEUE_INDEX, EVENT_QUEUE_SIZE)?;

        let mut rx_queue = VirtQueue::new(
            &transport,
            RX_QUEUE_INDEX,
            rx_size,
            RX_CHAIN_LIMIT,
            features,
        )?;
        let tx_queue = VirtQueue::new(
            &transport,
            TX_QUEUE_INDEX,
            tx_size,
            TX_CHAIN_LIMIT,
            features,
        )?;
        let mut event_queue = VirtQueue::new(
            &transport,
            EVENT_QUEUE_INDEX,
            event_size,
            EVENT_CHAIN_LIMIT,
            features,
        )?;

        let mut slots: Box<[Box<[u8]>]> = (0..usize::from(rx_size))
            .map(|_| vec![0_u8; RX_SLOT_BYTES].into_boxed_slice())
            .collect();
        let mut slot_for_token = vec![0_u16; usize::from(rx_size)].into_boxed_slice();
        for index in 0..slots.len() {
            let token = rx_queue.submit_output_deferred(&transport, &mut slots[index])?;
            slot_for_token[usize::from(token)] =
                u16::try_from(index).map_err(|_| IoError::DeviceFault)?;
        }
        rx_queue.publish();

        let mut buffers: Box<[Box<[u8]>]> = (0..usize::from(event_size))
            .map(|_| vec![0_u8; EVENT_BYTES].into_boxed_slice())
            .collect();
        let mut buffer_for_token = vec![0_u16; usize::from(event_size)].into_boxed_slice();
        for index in 0..buffers.len() {
            let token = event_queue.submit_output_deferred(&transport, &mut buffers[index])?;
            buffer_for_token[usize::from(token)] =
                u16::try_from(index).map_err(|_| IoError::DeviceFault)?;
        }
        event_queue.publish();

        let guest_cid = read_guest_cid(&transport);

        transport.set_status(
            DeviceStatus::ACKNOWLEDGE
                | DeviceStatus::DRIVER
                | DeviceStatus::FEATURES_OK
                | DeviceStatus::DRIVER_OK,
        );
        rx_queue.notify(&transport);
        event_queue.notify(&transport);

        tracing::info!(
            guest_cid,
            rx_queue_size = rx_size,
            tx_queue_size = tx_size,
            stream_feature = features.device(VSOCK_FEATURE_STREAM),
            "virtio-vsock device online"
        );

        Ok(Self {
            transport,
            guest_cid,
            rx: AsyncMutex::new(VsockRxState {
                queue: rx_queue,
                slots,
                slot_for_token,
            }),
            tx_queue: AsyncMutex::new(tx_queue),
            tx_inflight: InFlight::new(),
            event: AsyncMutex::new(VsockEventState {
                queue: event_queue,
                buffers,
                buffer_for_token,
            }),
            interrupts: Notify::new(),
            features,
            transport_reset: AtomicBool::new(false),
        })
    }

    /// The feature set this device negotiated.
    pub fn features(&self) -> NegotiatedFeatures {
        self.features
    }

    /// Interrupt handlers should only acknowledge the device and wake
    /// waiters.
    pub fn handle_interrupt(&self) {
        self.transport.ack_interrupt();
        self.interrupts.notify_all();
    }

    /// Takes the pending transport-reset announcement, if any.
    ///
    /// The device raises it when the host end of the link was replaced,
    /// which invalidates every connection at once. Reporting it as a
    /// one-shot flag rather than as a packet keeps it out of the receive
    /// path's ordering: there is no connection left for it to be
    /// ordered against.
    pub fn take_transport_reset(&self) -> bool {
        self.transport_reset.swap(false, Ordering::AcqRel)
    }

    /// Drains one packet out of the receive ring, if the device has
    /// published one.
    fn try_receive(
        state: &mut VsockRxState<T>,
        transport: &T,
        payload: &mut [u8],
    ) -> IoResult<Option<VsockReceived>> {
        let Some((token, used_len)) = state.queue.pop_used_with_len() else {
            return Ok(None);
        };
        let slot_index = usize::from(
            *state
                .slot_for_token
                .get(usize::from(token))
                .ok_or(IoError::DeviceFault)?,
        );
        let used_len = usize::try_from(used_len).map_err(|_| IoError::DeviceFault)?;
        // The slot returns to the device whatever the packet turns out
        // to be: a fault that also leaked a receive buffer would starve
        // the ring on top of dropping the packet.
        let decoded = Self::decode_slot(&state.slots[slot_index], used_len, payload);
        let token = state
            .queue
            .submit_output_deferred(transport, &mut state.slots[slot_index])?;
        state.slot_for_token[usize::from(token)] =
            u16::try_from(slot_index).map_err(|_| IoError::DeviceFault)?;
        state.queue.publish();
        state.queue.notify(transport);
        decoded.map(Some)
    }

    /// Decodes one used receive slot into `payload`.
    fn decode_slot(slot: &[u8], used_len: usize, payload: &mut [u8]) -> IoResult<VsockReceived> {
        if used_len < HEADER_BYTES || used_len > slot.len() {
            return Err(IoError::DeviceFault);
        }
        let header = decode_header(&slot[..HEADER_BYTES])?;
        let announced = usize::try_from(header.payload_len).map_err(|_| IoError::DeviceFault)?;
        if announced != used_len - HEADER_BYTES || announced > payload.len() {
            return Err(IoError::DeviceFault);
        }
        payload[..announced].copy_from_slice(&slot[HEADER_BYTES..used_len]);
        Ok(VsockReceived {
            header,
            payload_len: announced,
        })
    }

    /// Collects whatever the event queue published, reposting every
    /// buffer it drained.
    fn drain_events(state: &mut VsockEventState<T>, transport: &T) -> IoResult<bool> {
        let mut reset = false;
        let mut drained = false;
        while let Some((token, used_len)) = state.queue.pop_used_with_len() {
            drained = true;
            let index = usize::from(
                *state
                    .buffer_for_token
                    .get(usize::from(token))
                    .ok_or(IoError::DeviceFault)?,
            );
            if used_len as usize >= EVENT_BYTES {
                let buffer = &state.buffers[index];
                let id = u32::from_le_bytes(buffer[..EVENT_BYTES].try_into().expect("four bytes"));
                if id == EVENT_TRANSPORT_RESET {
                    reset = true;
                }
            }
            let token = state
                .queue
                .submit_output_deferred(transport, &mut state.buffers[index])?;
            state.buffer_for_token[usize::from(token)] =
                u16::try_from(index).map_err(|_| IoError::DeviceFault)?;
        }
        if drained {
            state.queue.publish();
            state.queue.notify(transport);
        }
        Ok(reset)
    }
}

fn queue_size<T: VirtioTransport>(transport: &T, index: u16, wanted: u16) -> IoResult<u16> {
    let size = transport.queue_max_size(index).min(wanted);
    if size == 0 || !size.is_power_of_two() {
        return Err(IoError::Unsupported);
    }
    Ok(size)
}

/// Reads the 64-bit `guest_cid` out of the device configuration space.
fn read_guest_cid<T: VirtioTransport>(transport: &T) -> u64 {
    let low = u64::from(transport.read_config_u32(CONFIG_GUEST_CID_OFFSET));
    let high = u64::from(transport.read_config_u32(CONFIG_GUEST_CID_OFFSET + 4));
    low | (high << 32)
}

impl<T: VirtioTransport> VsockDevice for VirtioVsockDevice<T> {
    fn guest_cid(&self) -> u64 {
        self.guest_cid
    }

    fn max_payload_bytes(&self) -> usize {
        VSOCK_MAX_PAYLOAD_BYTES
    }

    async fn send(&self, header: VsockPacketHeader, payload: &[u8]) -> IoResult<()> {
        if payload.len() > VSOCK_MAX_PAYLOAD_BYTES {
            return Err(IoError::OutOfBounds);
        }
        if usize::try_from(header.payload_len).map_err(|_| IoError::DeviceFault)? != payload.len() {
            return Err(IoError::InvalidDeviceConfig(
                "vsock packet header announces a payload length its buffer does not match",
            ));
        }
        let encoded = encode_header(&header);
        let inputs: [&[u8]; 2] = [&encoded, payload];
        let inputs = if payload.is_empty() {
            &inputs[..1]
        } else {
            &inputs[..]
        };
        let token = submit_chain(
            &self.tx_inflight,
            &self.tx_queue,
            &self.transport,
            inputs,
            &mut [],
        )
        .await?;
        await_completion(&self.tx_inflight, &self.tx_queue, token, || {
            self.interrupts.notified()
        })
        .await;
        Ok(())
    }

    async fn receive_into(&self, payload: &mut [u8]) -> IoResult<VsockDelivery> {
        if payload.len() < VSOCK_MAX_PAYLOAD_BYTES {
            return Err(IoError::OutOfBounds);
        }
        loop {
            // Armed before the rings are drained: a packet that the
            // device publishes in between belongs to this wait, not to
            // the next interrupt.
            let notified = self.interrupts.notified();
            if let Some(mut event) = self.event.try_lock()
                && Self::drain_events(&mut event, &self.transport)?
            {
                self.transport_reset.store(true, Ordering::Release);
            }
            if self.take_transport_reset() {
                return Ok(VsockDelivery::TransportReset);
            }
            {
                let mut state = self.rx.lock().await;
                if let Some(received) = Self::try_receive(&mut state, &self.transport, payload)? {
                    return Ok(VsockDelivery::Packet(received));
                }
            }
            notified.await;
        }
    }
}

impl<T: VirtioTransport> Drop for VirtioVsockDevice<T> {
    fn drop(&mut self) {
        self.rx.get_mut().queue.shutdown(&self.transport);
        self.tx_queue.get_mut().shutdown(&self.transport);
        self.event.get_mut().queue.shutdown(&self.transport);
    }
}

/// A shutdown announcement addressed at `destination`.
///
/// Building the header here rather than at each call site keeps the
/// flag encoding in one place; the connection table only decides which
/// directions it is closing.
pub fn vsock_shutdown_header(
    source: VsockAddress,
    destination: VsockAddress,
    shutdown: VsockShutdown,
    buf_alloc: u32,
    fwd_cnt: u32,
) -> VsockPacketHeader {
    VsockPacketHeader {
        source,
        destination,
        op: VsockOp::Shutdown,
        flags: shutdown.as_flags(),
        payload_len: 0,
        buf_alloc,
        fwd_cnt,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DeviceType, HEADER_BYTES, IoError, PACKET_TYPE_STREAM, VSOCK_MAX_PAYLOAD_BYTES,
        VirtioVsockDevice, decode_header, encode_header,
    };
    use crate::testing::{FakeTransport, FakeTransportConfig};
    use crate::transport::VirtioFeatures;
    use alloc::vec;
    use core::pin::pin;
    use futures_lite::future::{block_on, poll_once};
    use helios_hal::vsock::{
        VsockAddress, VsockDelivery, VsockDevice, VsockOp, VsockPacketHeader, VsockShutdown,
    };

    const GUEST_CID: u64 = 42;

    fn device() -> VirtioVsockDevice<FakeTransport> {
        let transport = FakeTransport::new(FakeTransportConfig {
            device_type: DeviceType::Vsock,
            offered_features: VirtioFeatures::VERSION_1.bits(),
            queue_size: 8,
            supports_queue_reset: false,
            absent_queues: &[],
        });
        transport.set_config_u32(0, GUEST_CID as u32);
        transport.set_config_u32(4, (GUEST_CID >> 32) as u32);
        VirtioVsockDevice::new(transport).expect("vsock device should initialize")
    }

    fn header(op: VsockOp, payload_len: u32) -> VsockPacketHeader {
        VsockPacketHeader {
            source: VsockAddress::new(GUEST_CID, 9000),
            destination: VsockAddress::host(1024),
            op,
            flags: 0,
            payload_len,
            buf_alloc: 4096,
            fwd_cnt: 17,
        }
    }

    /// Plays the device: writes `packet` into the receive slot the
    /// driver posted under `token` and raises the interrupt.
    fn deliver(device: &VirtioVsockDevice<FakeTransport>, token: u16, packet: &[u8]) {
        let state = device
            .rx
            .try_lock()
            .expect("a parked driver does not hold the receive lock");
        let len = state.queue.device_respond(token, packet);
        state.queue.device_complete(token, len);
        drop(state);
        device.handle_interrupt();
    }

    #[test]
    fn a_wrong_device_type_is_rejected() {
        let rejected = VirtioVsockDevice::new(FakeTransport::new(FakeTransportConfig {
            device_type: DeviceType::Block,
            ..FakeTransportConfig::default()
        }))
        .err();
        assert_eq!(rejected, Some(IoError::Unsupported));
    }

    #[test]
    fn the_guest_context_id_comes_from_the_device_configuration_space() {
        assert_eq!(device().guest_cid(), GUEST_CID);
    }

    #[test]
    fn a_header_round_trips_through_its_wire_encoding() {
        let original = VsockPacketHeader {
            source: VsockAddress::new(3, 0xdead_beef),
            destination: VsockAddress::new(2, 1024),
            op: VsockOp::Shutdown,
            flags: VsockShutdown::both().as_flags(),
            payload_len: 0,
            buf_alloc: 65_536,
            fwd_cnt: 4_294_967_000,
        };
        let encoded = encode_header(&original);
        assert_eq!(encoded.len(), HEADER_BYTES);
        assert_eq!(
            u16::from_le_bytes([encoded[28], encoded[29]]),
            PACKET_TYPE_STREAM,
            "the driver only ever emits stream packets"
        );
        assert_eq!(decode_header(&encoded), Ok(original));
    }

    #[test]
    fn a_packet_of_an_unknown_socket_type_is_a_device_fault() {
        let mut encoded = encode_header(&header(VsockOp::Data, 0));
        // VIRTIO_VSOCK_TYPE_SEQPACKET, which this driver does not carry.
        encoded[28..30].copy_from_slice(&2_u16.to_le_bytes());
        assert_eq!(decode_header(&encoded), Err(IoError::DeviceFault));
    }

    #[test]
    fn a_packet_of_an_undefined_operation_is_a_device_fault() {
        let mut encoded = encode_header(&header(VsockOp::Data, 0));
        encoded[30..32].copy_from_slice(&99_u16.to_le_bytes());
        assert_eq!(decode_header(&encoded), Err(IoError::DeviceFault));
    }

    #[test]
    fn every_receive_slot_is_posted_before_the_device_is_started() {
        let device = device();
        let state = device.rx.try_lock().expect("the driver is idle");
        assert_eq!(
            state.queue.available_descriptors(),
            0,
            "the whole receive ring is handed to the device at bring-up"
        );
    }

    #[test]
    fn a_delivered_packet_is_decoded_and_its_slot_returns_to_the_device() {
        let device = device();
        let mut payload = vec![0_u8; VSOCK_MAX_PAYLOAD_BYTES];
        let received = {
            let mut receive = pin!(device.receive_into(&mut payload));
            assert!(
                block_on(poll_once(receive.as_mut())).is_none(),
                "nothing has arrived yet"
            );

            let mut packet = vec![0_u8; HEADER_BYTES + 5];
            packet[..HEADER_BYTES].copy_from_slice(&encode_header(&VsockPacketHeader {
                source: VsockAddress::host(1024),
                destination: VsockAddress::new(GUEST_CID, 9000),
                op: VsockOp::Data,
                flags: 0,
                payload_len: 5,
                buf_alloc: 8192,
                fwd_cnt: 3,
            }));
            packet[HEADER_BYTES..].copy_from_slice(b"hello");
            deliver(&device, 0, &packet);

            match block_on(poll_once(receive.as_mut()))
                .expect("the packet is ready")
                .expect("the packet decodes")
            {
                VsockDelivery::Packet(received) => received,
                VsockDelivery::TransportReset => panic!("no transport reset was announced"),
            }
        };
        assert_eq!(received.payload_len, 5);
        assert_eq!(&payload[..5], b"hello");
        assert_eq!(received.header.op, VsockOp::Data);
        assert_eq!(received.header.buf_alloc, 8192);
        assert_eq!(received.header.fwd_cnt, 3);

        let state = device.rx.try_lock().expect("the driver is idle");
        assert_eq!(
            state.queue.available_descriptors(),
            0,
            "the drained slot went straight back to the device"
        );
    }

    #[test]
    fn a_credit_update_carries_the_peers_window_without_a_payload() {
        let device = device();
        let mut payload = vec![0_u8; VSOCK_MAX_PAYLOAD_BYTES];
        let mut receive = pin!(device.receive_into(&mut payload));
        assert!(block_on(poll_once(receive.as_mut())).is_none());

        let packet = encode_header(&VsockPacketHeader {
            source: VsockAddress::host(1024),
            destination: VsockAddress::new(GUEST_CID, 9000),
            op: VsockOp::CreditUpdate,
            flags: 0,
            payload_len: 0,
            buf_alloc: 262_144,
            fwd_cnt: 1_000,
        });
        deliver(&device, 0, &packet);

        let received = match block_on(poll_once(receive.as_mut()))
            .expect("the packet is ready")
            .expect("the packet decodes")
        {
            VsockDelivery::Packet(received) => received,
            VsockDelivery::TransportReset => panic!("no transport reset was announced"),
        };
        assert_eq!(received.payload_len, 0);
        assert_eq!(received.header.op, VsockOp::CreditUpdate);
        assert_eq!(received.header.buf_alloc, 262_144);
        assert_eq!(received.header.fwd_cnt, 1_000);
    }

    #[test]
    fn a_payload_longer_than_the_header_announced_is_a_device_fault() {
        let device = device();
        let mut payload = vec![0_u8; VSOCK_MAX_PAYLOAD_BYTES];
        let mut receive = pin!(device.receive_into(&mut payload));
        assert!(block_on(poll_once(receive.as_mut())).is_none());

        let mut packet = vec![0_u8; HEADER_BYTES + 8];
        packet[..HEADER_BYTES].copy_from_slice(&encode_header(&VsockPacketHeader {
            payload_len: 4,
            ..header(VsockOp::Data, 4)
        }));
        deliver(&device, 0, &packet);

        assert_eq!(
            block_on(poll_once(receive.as_mut())),
            Some(Err(IoError::DeviceFault))
        );
    }

    #[test]
    fn a_transmitted_packet_carries_its_header_and_payload_as_one_chain() {
        let device = device();
        let mut send = pin!(device.send(header(VsockOp::Data, 4), b"ping"));
        assert!(
            block_on(poll_once(send.as_mut())).is_none(),
            "the chain is still with the device"
        );

        let queue = device
            .tx_queue
            .try_lock()
            .expect("a parked sender does not hold the transmit lock");
        let request = queue.device_request(0);
        assert_eq!(request.len(), HEADER_BYTES + 4);
        assert_eq!(&request[HEADER_BYTES..], b"ping");
        let decoded = decode_header(&request).expect("the driver emits a decodable header");
        assert_eq!(decoded.op, VsockOp::Data);
        assert_eq!(decoded.payload_len, 4);
        assert_eq!(decoded.source, VsockAddress::new(GUEST_CID, 9000));
        assert_eq!(decoded.destination, VsockAddress::host(1024));

        queue.device_complete(0, 0);
        drop(queue);
        device.handle_interrupt();
        assert_eq!(block_on(poll_once(send.as_mut())), Some(Ok(())));
    }

    #[test]
    fn a_control_packet_is_transmitted_as_a_header_alone() {
        let device = device();
        let mut send = pin!(device.send(header(VsockOp::Reset, 0), &[]));
        assert!(block_on(poll_once(send.as_mut())).is_none());

        let queue = device.tx_queue.try_lock().expect("the sender is parked");
        assert_eq!(
            queue.device_chain(0).len(),
            1,
            "a packet with no payload occupies a single descriptor"
        );
        assert_eq!(queue.device_request(0).len(), HEADER_BYTES);
    }

    #[test]
    fn a_payload_that_disagrees_with_its_header_is_refused_before_the_device_sees_it() {
        let device = device();
        // Bring-up already kicked the receive and event rings; nothing
        // the refused packet does may add to that.
        let kicks = device.transport.kick_count();
        let refused = block_on(device.send(header(VsockOp::Data, 4), b"longer"));
        assert!(matches!(refused, Err(IoError::InvalidDeviceConfig(_))));
        assert_eq!(device.transport.kick_count(), kicks);
    }

    #[test]
    fn a_payload_larger_than_one_packet_is_refused() {
        let device = device();
        let oversized = vec![0_u8; VSOCK_MAX_PAYLOAD_BYTES + 1];
        let refused = block_on(device.send(
            header(VsockOp::Data, VSOCK_MAX_PAYLOAD_BYTES as u32 + 1),
            &oversized,
        ));
        assert_eq!(refused, Err(IoError::OutOfBounds));
    }

    #[test]
    fn a_receive_buffer_too_small_for_a_packet_is_refused() {
        let device = device();
        let mut payload = vec![0_u8; VSOCK_MAX_PAYLOAD_BYTES - 1];
        assert_eq!(
            block_on(device.receive_into(&mut payload)),
            Err(IoError::OutOfBounds)
        );
    }

    #[test]
    fn a_transport_reset_event_is_collected_once() {
        let device = device();
        let mut payload = vec![0_u8; VSOCK_MAX_PAYLOAD_BYTES];
        {
            let state = device.event.try_lock().expect("the driver is idle");
            let len = state.queue.device_respond(0, &0_u32.to_le_bytes());
            state.queue.device_complete(0, len);
        }
        assert_eq!(
            block_on(device.receive_into(&mut payload)),
            Ok(VsockDelivery::TransportReset),
            "the announcement reaches a caller parked for a packet"
        );
        assert!(
            !device.take_transport_reset(),
            "an announcement is delivered once"
        );
    }
}
