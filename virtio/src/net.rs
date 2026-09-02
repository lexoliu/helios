use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use async_lock::Mutex as AsyncMutex;
use bytes::Bytes;
use core::cell::UnsafeCell;
use core::future::Future;
use core::mem::size_of;
use core::ops::Range;

use helios_hal::io::{IoError, IoResult};
use spin::Mutex as SpinMutex;

use crate::features::{NegotiatedFeatures, RING_FEATURES, negotiate_with};
use crate::notify::Notify;
use crate::queue::VirtQueue;
use crate::transport::{DeviceStatus, DeviceType, VirtioTransport};

const NET_QUEUE_SIZE: u16 = 256;
/// Receive buffers are always a single writable descriptor.
const RX_CHAIN_LIMIT: u16 = 1;
/// A transmit frame is either one slot-resident buffer or a slot-resident
/// header prefix chained to an external payload.
const TX_CHAIN_LIMIT: u16 = 2;
/// A control command is a read-only command buffer plus a writable ack.
const CONTROL_CHAIN_LIMIT: u16 = 2;
const DESCRIPTOR_BITSET_WORDS: usize =
    (NET_QUEUE_SIZE as usize + usize::BITS as usize - 1) / usize::BITS as usize;
const ETH_HEADER_LEN: usize = 14;
const DEFAULT_IP_MTU: usize = 1500;
/// VIRTIO_NET_F_CSUM: the driver may submit frames with partial
/// checksums that the device completes from csum_start/csum_offset.
const NET_FEATURE_CSUM: u64 = 1 << 0;
/// VIRTIO_NET_F_HOST_TSO4/6: the device performs TCP segmentation, so
/// the driver may submit one oversized segment with GSO metadata.
const NET_FEATURE_HOST_TSO4: u64 = 1 << 11;
const NET_FEATURE_HOST_TSO6: u64 = 1 << 12;
const NET_FEATURE_MAC: u64 = 1 << 5;
const NET_FEATURE_STATUS: u64 = 1 << 16;
const NET_FEATURE_MTU: u64 = 1 << 3;
/// VIRTIO_NET_F_MQ: device exposes multiple TX/RX queue pairs and
/// the driver may activate up to `max_virtqueue_pairs` of them via
/// the control queue command class 4 (`VIRTIO_NET_CTRL_MQ`,
/// command 0 = `VQ_PAIRS_SET`). Required for SMP TX scaling so
/// each CPU can write to its own ring without contending on the
/// other CPUs' submissions.
const NET_FEATURE_MQ: u64 = 1 << 22;
/// VIRTIO_NET_F_CTRL_VQ: device exposes a control queue that the
/// driver uses to issue runtime configuration commands such as
/// `VQ_PAIRS_SET`. Required when negotiating `NET_FEATURE_MQ`.
const NET_FEATURE_CTRL_VQ: u64 = 1 << 17;
/// Maximum queue pairs the kernel is willing to bring up. Sized so
/// the per-CPU shard count on Apple Silicon (up to 12 cores) fits;
/// devices advertising more pairs are simply capped at this value.
const NET_MAX_QUEUE_PAIRS: u16 = 16;
/// Control-queue command class for `VIRTIO_NET_CTRL_MQ`.
const CTRL_CLASS_MQ: u8 = 4;
/// Control-queue command id for `VQ_PAIRS_SET` under class MQ.
const CTRL_CMD_MQ_VQ_PAIRS_SET: u8 = 0;
/// Pre-submission ack sentinel; the device replaces this with
/// `CTRL_ACK_OK` (0) on success or `CTRL_ACK_FAIL` (1) on error.
const CTRL_ACK_PENDING: u8 = 0xff;
/// Device-side success ack on the control queue.
const CTRL_ACK_OK: u8 = 0;
/// Bytes the `VQ_PAIRS_SET` command payload occupies (class +
/// command + le16 pairs).
const CTRL_MQ_PAIRS_CMD_BYTES: usize = 4;
/// Maximum command payload size the control queue scratch buffer
/// is sized for. Currently only `VQ_PAIRS_SET` is sent.
const CTRL_CMD_MAX_BYTES: usize = CTRL_MQ_PAIRS_CMD_BYTES;

/// Checksum-offload metadata for one transmit frame: the device
/// finishes the one's-complement sum from byte `start` of the frame and
/// stores the complemented result at `start + offset`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxChecksumMeta {
    pub start: u16,
    pub offset: u16,
}

/// Borrowed transmit frame plus checksum-offload metadata.
#[derive(Clone, Copy, Debug)]
pub struct TxFrameDescriptor<'a> {
    pub bytes: &'a [u8],
    pub checksum: Option<TxChecksumMeta>,
}

/// A transmit frame split into a small header prefix (copied into the
/// device slot behind the virtio-net header) and an external payload
/// the device reads in place. The payload memory must stay valid until
/// a token-reporting reclaim returns this frame's token.
#[derive(Clone, Copy, Debug)]
pub struct TxScatterFrame<'a> {
    pub headers: &'a [u8],
    pub payload: &'a [u8],
    pub checksum: Option<TxChecksumMeta>,
}

/// A frame acceptable to the transmit entry points. Plain byte slices
/// transmit with no offload; [`TxFrameDescriptor`] carries checksum
/// metadata into the virtio-net header.
pub trait TxFrame {
    fn frame_bytes(&self) -> &[u8];
    fn tx_checksum(&self) -> Option<TxChecksumMeta> {
        None
    }
}

impl TxFrame for &[u8] {
    fn frame_bytes(&self) -> &[u8] {
        self
    }
}

impl TxFrame for TxFrameDescriptor<'_> {
    fn frame_bytes(&self) -> &[u8] {
        self.bytes
    }

    fn tx_checksum(&self) -> Option<TxChecksumMeta> {
        self.checksum
    }
}

impl TxFrame for helios_netstack::TxFrameRef<'_> {
    fn frame_bytes(&self) -> &[u8] {
        // The contiguous entry points cannot carry a scatter payload; a
        // frame that reaches them with one attached would go on the wire
        // truncated with a matching-length IP header. Scatter frames must
        // go through the scatter transmit entry points.
        assert!(
            self.payload.is_none(),
            "scatter TCP frame routed through a contiguous transmit path"
        );
        self.bytes
    }

    fn tx_checksum(&self) -> Option<TxChecksumMeta> {
        self.checksum.map(|checksum| TxChecksumMeta {
            start: checksum.start,
            offset: checksum.offset,
        })
    }
}

impl TxFrame for helios_netstack::PacketBuffer {
    fn frame_bytes(&self) -> &[u8] {
        assert!(
            self.payload().is_none(),
            "scatter TCP frame routed through a contiguous transmit path"
        );
        self.as_slice()
    }

    fn tx_checksum(&self) -> Option<TxChecksumMeta> {
        helios_netstack::PacketBuffer::tx_checksum(self).map(|checksum| TxChecksumMeta {
            start: checksum.start,
            offset: checksum.offset,
        })
    }
}

/// VIRTIO_NET_HDR_F_NEEDS_CSUM: csum_start/csum_offset are valid.
const VIRTIO_NET_HDR_F_NEEDS_CSUM: u8 = 1;
const VIRTIO_NET_HDR_GSO_NONE: u8 = 0;
const VIRTIO_NET_HDR_GSO_TCPV4: u8 = 1;
const VIRTIO_NET_HDR_GSO_TCPV6: u8 = 4;

/// TCP segmentation-offload metadata for one oversized transmit frame:
/// the device splits it into `mss`-payload segments, replicating the
/// `hdr_len`-byte header prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxGsoMeta {
    pub ipv6: bool,
    pub hdr_len: u16,
    pub mss: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VirtioNetHeader {
    flags: u8,
    gso_type: u8,
    hdr_len: u16,
    gso_size: u16,
    csum_start: u16,
    csum_offset: u16,
    num_buffers: u16,
}

struct NetRxState<T: VirtioTransport> {
    rx_queue: VirtQueue<T>,
    /// Receive slots currently owned by the device, indexed by slot.
    rx_in_device: DescriptorBitSet,
    /// Which receive slot each in-flight descriptor identifier carries.
    ///
    /// Descriptor identifiers are owned by the ring, which hands them out
    /// in its own order, so a slot keeps no permanent identifier: the
    /// mapping is recorded when a buffer is posted and read back when the
    /// device completes it.
    rx_slot_for_token: Box<[u16]>,
}

struct RxReturnedSlots {
    slots: SpinMutex<Vec<u16>>,
}

struct RxBufferSlot {
    slot: u16,
    returned: Arc<RxReturnedSlots>,
    buffer: UnsafeCell<Box<[u8]>>,
}

struct RxFrameOwner {
    slot: Arc<RxBufferSlot>,
    range: Range<usize>,
}

struct NetTxState<T: VirtioTransport> {
    tx_queue: VirtQueue<T>,
    tx_buffers: Box<[u8]>,
    tx_buffer_len: usize,
    tx_in_flight: DescriptorBitSet,
}

/// One TX/RX queue pair as exposed by VIRTIO_NET_F_MQ. Each pair
/// owns its own pair of `VirtQueue`s, RX buffer slab, and TX buffer
/// slab so submissions on different CPUs do not contend on the same
/// SpinMutex / async lock.
struct NetQueuePair<T: VirtioTransport> {
    rx_state: AsyncMutex<NetRxState<T>>,
    rx_returned: Arc<RxReturnedSlots>,
    rx_slots: Box<[Arc<RxBufferSlot>]>,
    tx_state: SpinMutex<NetTxState<T>>,
}

unsafe impl Send for RxBufferSlot {}
unsafe impl Sync for RxBufferSlot {}
unsafe impl<T: VirtioTransport> Send for NetQueuePair<T> {}
unsafe impl<T: VirtioTransport> Sync for NetQueuePair<T> {}

struct DescriptorBitSet {
    words: [usize; DESCRIPTOR_BITSET_WORDS],
    bits: usize,
}

pub struct VirtioNetDevice<T: VirtioTransport> {
    transport: T,
    /// One entry per `VIRTIO_NET_F_MQ` queue pair. Single-queue
    /// devices (and devices that did not negotiate MQ) carry a
    /// single pair so the rest of the implementation does not need
    /// a `Option` branch on every code path.
    queue_pairs: Box<[NetQueuePair<T>]>,
    /// Control queue used to issue runtime commands such as
    /// `VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET`. `None` when the device
    /// did not negotiate `VIRTIO_NET_F_CTRL_VQ` (unconditionally
    /// the case for single-queue paths).
    control: Option<SpinMutex<NetControlState<T>>>,
    rx_buffer_len: usize,
    interrupts: Notify,
    features: NegotiatedFeatures,
    mac_address: [u8; 6],
    max_frame_len: usize,
    header_len: usize,
    /// VIRTIO_NET_F_CSUM was negotiated: frames may carry partial
    /// checksums for the device to finish.
    tx_checksum_negotiated: bool,
    /// VIRTIO_NET_F_HOST_TSO4/6 negotiated for the given families: the
    /// driver may submit oversized TCP segments for device segmentation.
    tso_v4_negotiated: bool,
    tso_v6_negotiated: bool,
}

/// Control queue state: a single descriptor pair (header bytes
/// in, ack byte out). Each command serialises through `SpinMutex`
/// so the rare control-plane traffic does not race with the
/// per-CPU TX submission paths.
struct NetControlState<T: VirtioTransport> {
    queue: VirtQueue<T>,
    /// Scratch buffer for the next command's header + body bytes.
    /// Sized once at init to fit the largest fixed-shape command
    /// helios sends (currently `VQ_PAIRS_SET`, 4 bytes).
    cmd_buffer: Box<[u8]>,
    ack_buffer: Box<[u8]>,
}

pub type RxFrame = Bytes;

// SAFETY: each `NetQueuePair`'s RX/TX state is independently
// synchronised by its own async / spin lock; `control` follows the
// same single-mutex discipline as the legacy single-queue
// `tx_state`. Crossing pair boundaries always happens via Box-slice
// indexing (no aliasing) so the device as a whole is `Sync`.
unsafe impl<T: VirtioTransport> Sync for VirtioNetDevice<T> {}

impl AsRef<[u8]> for RxFrameOwner {
    fn as_ref(&self) -> &[u8] {
        &self.slot.buffer()[self.range.clone()]
    }
}

impl Drop for RxFrameOwner {
    fn drop(&mut self) {
        self.slot.returned.slots.lock().push(self.slot.slot);
    }
}

impl RxBufferSlot {
    fn buffer(&self) -> &[u8] {
        unsafe { &*self.buffer.get() }
    }

    fn buffer_mut(&self) -> &mut [u8] {
        unsafe { &mut *self.buffer.get() }
    }
}

impl<T: VirtioTransport> VirtioNetDevice<T> {
    pub fn new(transport: T) -> IoResult<Self> {
        if transport.device_type() != DeviceType::Network {
            return Err(IoError::Unsupported);
        }

        let features = negotiate_with(&transport, |offered| {
            // Only ask for MQ together with CTRL_VQ — without the
            // control queue there is no way to enable additional pairs
            // beyond the default single pair.
            let mq_supported = offered & NET_FEATURE_MQ != 0 && offered & NET_FEATURE_CTRL_VQ != 0;
            let mq_mask = if mq_supported {
                NET_FEATURE_MQ | NET_FEATURE_CTRL_VQ
            } else {
                0
            };
            // HOST_TSO requires CSUM; only ask for segmentation when both
            // the device offers TSO and checksum is available.
            let tso_supported = offered & NET_FEATURE_CSUM != 0
                && offered & (NET_FEATURE_HOST_TSO4 | NET_FEATURE_HOST_TSO6) != 0;
            let tso_mask = if tso_supported {
                NET_FEATURE_HOST_TSO4 | NET_FEATURE_HOST_TSO6
            } else {
                0
            };
            RING_FEATURES
                | NET_FEATURE_CSUM
                | NET_FEATURE_MAC
                | NET_FEATURE_STATUS
                | NET_FEATURE_MTU
                | mq_mask
                | tso_mask
        })?;

        let mac_address = read_mac_address(&transport);
        let ip_mtu = read_mtu(&transport, features);
        let max_frame_len = ip_mtu
            .checked_add(ETH_HEADER_LEN)
            .ok_or(IoError::DeviceFault)?;
        let header_len = size_of::<VirtioNetHeader>();
        let rx_buffer_len = header_len
            .checked_add(max_frame_len)
            .ok_or(IoError::DeviceFault)?;
        let tx_buffer_len = header_len
            .checked_add(max_frame_len)
            .ok_or(IoError::DeviceFault)?;

        let pair_count = if features.device(NET_FEATURE_MQ) {
            let device_max = read_max_virtqueue_pairs(&transport);
            device_max.clamp(1, NET_MAX_QUEUE_PAIRS)
        } else {
            1
        };
        if pair_count == 0 {
            return Err(IoError::Unsupported);
        }

        let mut queue_pairs: Vec<NetQueuePair<T>> = Vec::with_capacity(usize::from(pair_count));
        for pair_idx in 0..pair_count {
            let rx_queue_index = pair_idx * 2;
            let tx_queue_index = pair_idx * 2 + 1;
            let rx_queue_size = transport.queue_max_size(rx_queue_index).min(NET_QUEUE_SIZE);
            let tx_queue_size = transport.queue_max_size(tx_queue_index).min(NET_QUEUE_SIZE);
            if rx_queue_size == 0
                || tx_queue_size == 0
                || !rx_queue_size.is_power_of_two()
                || !tx_queue_size.is_power_of_two()
            {
                return Err(IoError::Unsupported);
            }

            let mut rx_queue = VirtQueue::new(
                &transport,
                rx_queue_index,
                rx_queue_size,
                RX_CHAIN_LIMIT,
                features,
            )?;
            let tx_queue = VirtQueue::new(
                &transport,
                tx_queue_index,
                tx_queue_size,
                TX_CHAIN_LIMIT,
                features,
            )?;
            let rx_buffer_count = usize::from(rx_queue_size);
            let rx_returned = Arc::new(RxReturnedSlots {
                slots: SpinMutex::new(Vec::with_capacity(rx_buffer_count)),
            });
            let mut rx_slots: Vec<Arc<RxBufferSlot>> = Vec::with_capacity(rx_buffer_count);
            for slot_index in 0..rx_buffer_count {
                let slot = u16::try_from(slot_index).map_err(|_| IoError::DeviceFault)?;
                rx_slots.push(Arc::new(RxBufferSlot {
                    slot,
                    returned: rx_returned.clone(),
                    buffer: UnsafeCell::new(vec![0_u8; rx_buffer_len].into_boxed_slice()),
                }));
            }
            let mut rx_in_device = DescriptorBitSet::new(rx_buffer_count);
            let mut rx_slot_for_token = vec![0_u16; usize::from(rx_queue_size)].into_boxed_slice();
            for (slot_index, slot) in rx_slots.iter().enumerate() {
                let token = rx_queue.submit_output_deferred(&transport, slot.buffer_mut())?;
                rx_slot_for_token[usize::from(token)] = slot.slot;
                assert!(
                    !rx_in_device.get(slot_index),
                    "virtio net RX slot was posted twice during initialization"
                );
                rx_in_device.set(slot_index);
            }
            rx_queue.publish();
            let tx_buffer_count = usize::from(tx_queue_size);
            let tx_buffers = vec![
                0_u8;
                tx_buffer_len
                    .checked_mul(tx_buffer_count)
                    .ok_or(IoError::DeviceFault)?
            ]
            .into_boxed_slice();
            let tx_in_flight = DescriptorBitSet::new(tx_buffer_count);

            queue_pairs.push(NetQueuePair {
                rx_state: AsyncMutex::new(NetRxState {
                    rx_queue,
                    rx_in_device,
                    rx_slot_for_token,
                }),
                rx_returned,
                rx_slots: rx_slots.into_boxed_slice(),
                tx_state: SpinMutex::new(NetTxState {
                    tx_queue,
                    tx_buffers,
                    tx_buffer_len,
                    tx_in_flight,
                }),
            });
        }

        // Optional control queue. Allocated AFTER all RX/TX queues
        // because virtio places it at index 2*max_pairs, not
        // immediately after the queues we activated.
        let control = if features.device(NET_FEATURE_CTRL_VQ) {
            let device_max = if features.device(NET_FEATURE_MQ) {
                read_max_virtqueue_pairs(&transport).max(1)
            } else {
                1
            };
            let ctrl_index = device_max * 2;
            let ctrl_size = transport.queue_max_size(ctrl_index).min(NET_QUEUE_SIZE);
            if ctrl_size != 0 && ctrl_size.is_power_of_two() {
                let queue = VirtQueue::new(
                    &transport,
                    ctrl_index,
                    ctrl_size,
                    CONTROL_CHAIN_LIMIT,
                    features,
                )?;
                let cmd_buffer = vec![0u8; CTRL_CMD_MAX_BYTES].into_boxed_slice();
                let ack_buffer = vec![0u8; 1].into_boxed_slice();
                Some(SpinMutex::new(NetControlState {
                    queue,
                    cmd_buffer,
                    ack_buffer,
                }))
            } else {
                None
            }
        } else {
            None
        };

        transport.set_status(
            DeviceStatus::ACKNOWLEDGE
                | DeviceStatus::DRIVER
                | DeviceStatus::FEATURES_OK
                | DeviceStatus::DRIVER_OK,
        );
        // Kick each RX queue so the device knows descriptors are
        // available for delivery on every pair.
        for (pair_idx, pair) in queue_pairs.iter().enumerate() {
            let _ = pair_idx;
            pair.rx_state
                .try_lock()
                .expect("freshly constructed RX queue cannot be contended")
                .rx_queue
                .notify(&transport);
        }

        let device = Self {
            transport,
            queue_pairs: queue_pairs.into_boxed_slice(),
            control,
            rx_buffer_len,
            interrupts: Notify::new(),
            features,
            mac_address,
            max_frame_len,
            header_len,
            tx_checksum_negotiated: features.device(NET_FEATURE_CSUM),
            tso_v4_negotiated: features.device(NET_FEATURE_HOST_TSO4),
            tso_v6_negotiated: features.device(NET_FEATURE_HOST_TSO6),
        };

        // Activate every queue pair on the device side. The device
        // ships with a single pair active by default; without this
        // command extra RX queues we just allocated would not
        // receive traffic.
        if features.device(NET_FEATURE_MQ) && device.queue_pair_count() > 1 {
            device.send_set_vq_pairs(device.queue_pair_count())?;
        }
        Ok(device)
    }

    /// Number of TX/RX queue pairs the device exposes. Always at
    /// least 1; greater than 1 only when both `VIRTIO_NET_F_MQ` and
    /// `VIRTIO_NET_F_CTRL_VQ` were negotiated and the device
    /// advertised multiple pairs.
    /// Descriptors in each TX queue: bounds the completion tokens a
    /// zero-copy caller must be able to pin payloads for.
    pub fn tx_queue_depth(&self) -> usize {
        usize::from(NET_QUEUE_SIZE)
    }

    /// Queue topology as the netstack capability struct.
    pub fn queue_topology(&self) -> helios_netstack::QueueTopology {
        helios_netstack::QueueTopology {
            rx_queues: self.queue_pair_count(),
            tx_queues: self.queue_pair_count(),
            tx_queue_depth: self.tx_queue_depth(),
            rss: false,
        }
    }

    pub fn queue_pair_count(&self) -> usize {
        self.queue_pairs.len()
    }

    /// The feature set this device negotiated.
    pub fn features(&self) -> NegotiatedFeatures {
        self.features
    }

    fn send_set_vq_pairs(&self, pairs: usize) -> IoResult<()> {
        let Some(control) = self.control.as_ref() else {
            return Err(IoError::Unsupported);
        };
        let pairs_u16 = u16::try_from(pairs).map_err(|_| IoError::DeviceFault)?;
        let mut state = control.lock();
        let pairs_bytes = pairs_u16.to_le_bytes();
        // Destructure under `&mut` so the split borrows of the
        // queue, cmd buffer, and ack buffer are independent for the
        // duration of the submission.
        let NetControlState {
            queue,
            cmd_buffer,
            ack_buffer,
        } = &mut *state;
        cmd_buffer[0] = CTRL_CLASS_MQ;
        cmd_buffer[1] = CTRL_CMD_MQ_VQ_PAIRS_SET;
        cmd_buffer[2] = pairs_bytes[0];
        cmd_buffer[3] = pairs_bytes[1];
        ack_buffer[0] = CTRL_ACK_PENDING;
        let cmd_slice: &[u8] = &cmd_buffer[..CTRL_MQ_PAIRS_CMD_BYTES];
        let ack_slice: &mut [u8] = &mut ack_buffer[..];
        let _token = queue.submit(&self.transport, &[cmd_slice], &mut [ack_slice])?;
        queue.notify(&self.transport);
        // Spin-poll the used ring; the device handles the command
        // synchronously and `pop_used` becomes ready as soon as the
        // descriptor returns. Holding the spin mutex here is fine
        // because the control queue is consulted only at startup
        // and infrequent runtime tweaks.
        loop {
            if queue.pop_used().is_some() {
                break;
            }
            core::hint::spin_loop();
        }
        if ack_buffer[0] != CTRL_ACK_OK {
            return Err(IoError::DeviceFault);
        }
        Ok(())
    }

    pub fn handle_interrupt(&self) {
        self.transport.ack_interrupt();
        self.interrupts.notify_all();
    }

    /// Whether VIRTIO_NET_F_CSUM was negotiated, allowing frames with
    /// partial checksums the device finishes on transmit.
    pub fn tx_checksum_negotiated(&self) -> bool {
        self.tx_checksum_negotiated
    }

    /// Whether the device performs TCP segmentation for the given
    /// address family, allowing oversized TX segments.
    pub fn tso_negotiated(&self, ipv6: bool) -> bool {
        if ipv6 {
            self.tso_v6_negotiated
        } else {
            self.tso_v4_negotiated
        }
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac_address
    }

    pub fn max_frame_len(&self) -> usize {
        self.max_frame_len
    }

    pub async fn try_receive_into(&self, output: &mut [u8]) -> IoResult<Option<usize>> {
        const PAIR: usize = 0;
        let mut state = self.queue_pairs[PAIR].rx_state.lock().await;
        self.drain_returned_rx_buffers(PAIR, &mut state)?;
        let Some((token, used_len)) = state.rx_queue.pop_used_with_len() else {
            return Ok(None);
        };
        let slot_index = Self::complete_rx_slot(&mut state, token);
        let used_len = used_len as usize;
        if used_len < self.header_len || used_len > self.rx_buffer_len {
            self.repost_rx_buffer(PAIR, &mut state, slot_index)?;
            return Err(IoError::DeviceFault);
        }

        let frame_len = used_len - self.header_len;
        if frame_len > output.len() {
            self.repost_rx_buffer(PAIR, &mut state, slot_index)?;
            return Err(IoError::OutOfBounds);
        }
        let slot = &self.queue_pairs[PAIR].rx_slots[usize::from(slot_index)];
        output[..frame_len].copy_from_slice(&slot.buffer()[self.header_len..used_len]);
        self.repost_rx_buffer(PAIR, &mut state, slot_index)?;
        Ok(Some(frame_len))
    }

    pub async fn try_receive_frame(&self) -> IoResult<Option<RxFrame>> {
        const PAIR: usize = 0;
        let mut state = self.queue_pairs[PAIR].rx_state.lock().await;
        self.drain_returned_rx_buffers(PAIR, &mut state)?;
        let Some((token, used_len)) = state.rx_queue.pop_used_with_len() else {
            return Ok(None);
        };
        let slot_index = Self::complete_rx_slot(&mut state, token);
        let used_len = used_len as usize;
        if used_len < self.header_len || used_len > self.rx_buffer_len {
            self.repost_rx_buffer(PAIR, &mut state, slot_index)?;
            return Err(IoError::DeviceFault);
        }

        Ok(Some(self.rx_frame_from_slot(PAIR, slot_index, used_len)))
    }

    pub fn try_receive_frames_immediate(
        &self,
        frames: &mut [Option<RxFrame>],
    ) -> IoResult<Option<usize>> {
        let mut received = 0usize;
        let mut any_locked = false;
        for pair_idx in 0..self.queue_pairs.len() {
            if received >= frames.len() {
                break;
            }
            let Some(mut state) = self.queue_pairs[pair_idx].rx_state.try_lock() else {
                continue;
            };
            any_locked = true;
            received += self.drain_rx_pair_locked(pair_idx, &mut state, &mut frames[received..])?;
        }
        if !any_locked {
            return Ok(None);
        }
        Ok(Some(received))
    }

    /// Variant of `try_receive_frames_immediate` that drains a single
    /// RX queue pair, so per-processor pollers do not contend on the
    /// other pairs' locks. The index is normalized like the TX pair
    /// entry points.
    pub fn try_receive_frames_immediate_on_pair(
        &self,
        pair_idx: usize,
        frames: &mut [Option<RxFrame>],
    ) -> IoResult<Option<usize>> {
        let pair_idx = self.normalize_pair_idx(pair_idx);
        let Some(mut state) = self.queue_pairs[pair_idx].rx_state.try_lock() else {
            return Ok(None);
        };
        let received = self.drain_rx_pair_locked(pair_idx, &mut state, frames)?;
        Ok(Some(received))
    }

    fn drain_rx_pair_locked(
        &self,
        pair_idx: usize,
        state: &mut NetRxState<T>,
        frames: &mut [Option<RxFrame>],
    ) -> IoResult<usize> {
        self.drain_returned_rx_buffers(pair_idx, state)?;
        let mut received = 0usize;
        while received < frames.len() {
            let Some((token, used_len)) = state.rx_queue.pop_used_with_len() else {
                break;
            };
            let slot_index = Self::complete_rx_slot(state, token);
            let used_len = used_len as usize;
            if used_len < self.header_len || used_len > self.rx_buffer_len {
                self.repost_rx_buffer(pair_idx, state, slot_index)?;
                return Err(IoError::DeviceFault);
            }

            let slot = &mut frames[received];
            assert!(slot.is_none(), "virtio net RX batch slot was not empty");
            *slot = Some(self.rx_frame_from_slot(pair_idx, slot_index, used_len));
            received += 1;
        }
        Ok(received)
    }

    pub async fn repost_rx_frame(&self, frame: RxFrame) -> IoResult<()> {
        drop(frame);
        for pair_idx in 0..self.queue_pairs.len() {
            let mut state = self.queue_pairs[pair_idx].rx_state.lock().await;
            self.drain_returned_rx_buffers(pair_idx, &mut state)?;
        }
        Ok(())
    }

    pub fn repost_rx_frames_immediate(
        &self,
        frames: &mut [Option<RxFrame>],
    ) -> IoResult<Option<()>> {
        for frame in frames.iter_mut() {
            drop(frame.take());
        }
        let mut any_locked = false;
        for pair_idx in 0..self.queue_pairs.len() {
            let Some(mut state) = self.queue_pairs[pair_idx].rx_state.try_lock() else {
                continue;
            };
            any_locked = true;
            self.drain_returned_rx_buffers(pair_idx, &mut state)?;
        }
        if !any_locked {
            return Ok(None);
        }
        Ok(Some(()))
    }

    pub async fn transmit(&self, frame: &[u8]) -> IoResult<()> {
        self.transmit_with_wait(frame, || self.interrupts.notified())
            .await
    }

    pub async fn transmit_with_wait<Wait, Fut>(&self, frame: &[u8], wait: Wait) -> IoResult<()>
    where
        Wait: FnMut() -> Fut,
        Fut: Future<Output = ()>,
    {
        self.transmit_batch_with_wait(&[frame], wait).await
    }

    pub async fn transmit_batch(&self, frames: &[&[u8]]) -> IoResult<()> {
        self.transmit_batch_with_wait(frames, || self.interrupts.notified())
            .await
    }

    pub async fn transmit_batch_with_wait<Wait, Fut>(
        &self,
        frames: &[&[u8]],
        wait: Wait,
    ) -> IoResult<()>
    where
        Wait: FnMut() -> Fut,
        Fut: Future<Output = ()>,
    {
        self.transmit_frames_with_wait(frames, wait).await
    }

    pub async fn try_transmit_frames<Frame>(&self, frames: &[Frame]) -> IoResult<usize>
    where
        Frame: TxFrame,
    {
        self.try_transmit_frames_on_pair(0, frames).await
    }

    /// Per-pair variant of [`try_transmit_frames`]. The pair index
    /// is clamped to the actual queue pair count so callers that
    /// pass a stale CPU index never panic and instead route to the
    /// last live queue.
    pub async fn try_transmit_frames_on_pair<Frame>(
        &self,
        pair_idx: usize,
        frames: &[Frame],
    ) -> IoResult<usize>
    where
        Frame: TxFrame,
    {
        self.validate_tx_frames(frames)?;
        let pair_idx = self.normalize_pair_idx(pair_idx);
        let mut state = self.queue_pairs[pair_idx].tx_state.lock();
        Self::drain_tx_completions_when_full(&mut state, frames.len());
        let mut next_frame = 0usize;
        self.submit_available_tx_frames(&mut state, frames, &mut next_frame)
    }

    pub fn try_transmit_frames_immediate<Frame>(&self, frames: &[Frame]) -> IoResult<Option<usize>>
    where
        Frame: TxFrame,
    {
        self.try_transmit_frames_immediate_on_pair(0, frames)
    }

    pub fn try_transmit_frames_immediate_on_pair<Frame>(
        &self,
        pair_idx: usize,
        frames: &[Frame],
    ) -> IoResult<Option<usize>>
    where
        Frame: TxFrame,
    {
        self.validate_tx_frames(frames)?;
        self.try_transmit_valid_frames_immediate(self.normalize_pair_idx(pair_idx), frames)
    }

    /// Immediate TX path for frames already produced by the Helios netstack.
    ///
    /// The stack's `PacketBuffer` capacity and frame encoders enforce Ethernet
    /// frame bounds before queueing; skipping the second validation pass keeps
    /// the profile-visible `network;tx-submit-immediate-device-*` phase focused
    /// on descriptor ownership and payload copy. Slot capacity is still checked
    /// by `write_tx_payload` immediately before DMA publication.
    pub fn try_transmit_trusted_frames_immediate<Frame>(
        &self,
        frames: &[Frame],
    ) -> IoResult<Option<usize>>
    where
        Frame: TxFrame,
    {
        self.try_transmit_trusted_frames_immediate_on_pair(0, frames)
    }

    pub fn try_transmit_trusted_frames_immediate_on_pair<Frame>(
        &self,
        pair_idx: usize,
        frames: &[Frame],
    ) -> IoResult<Option<usize>>
    where
        Frame: TxFrame,
    {
        self.try_transmit_valid_frames_immediate(self.normalize_pair_idx(pair_idx), frames)
    }

    fn try_transmit_valid_frames_immediate<Frame>(
        &self,
        pair_idx: usize,
        frames: &[Frame],
    ) -> IoResult<Option<usize>>
    where
        Frame: TxFrame,
    {
        let Some(mut state) = self.queue_pairs[pair_idx].tx_state.try_lock() else {
            return Ok(None);
        };
        Self::drain_tx_completions_when_full(&mut state, frames.len());
        let mut next_frame = 0usize;
        self.submit_available_tx_frames(&mut state, frames, &mut next_frame)
            .map(Some)
    }

    /// Maps a caller-provided queue pair index onto the live
    /// queue-pair ring. This lets CPU/shard indices exceed the
    /// device's negotiated queue-pair count without adding a legacy
    /// single-queue code path.
    fn normalize_pair_idx(&self, pair_idx: usize) -> usize {
        if self.queue_pairs.is_empty() {
            return 0;
        }
        pair_idx % self.queue_pairs.len()
    }

    pub async fn transmit_frames_with_wait<Frame, Wait, Fut>(
        &self,
        frames: &[Frame],
        wait: Wait,
    ) -> IoResult<()>
    where
        Frame: TxFrame,
        Wait: FnMut() -> Fut,
        Fut: Future<Output = ()>,
    {
        self.transmit_frames_with_wait_on_pair(0, frames, wait)
            .await
    }

    pub async fn transmit_frames_with_wait_on_pair<Frame, Wait, Fut>(
        &self,
        pair_idx: usize,
        frames: &[Frame],
        mut wait: Wait,
    ) -> IoResult<()>
    where
        Frame: TxFrame,
        Wait: FnMut() -> Fut,
        Fut: Future<Output = ()>,
    {
        self.validate_tx_frames(frames)?;
        let pair_idx = self.normalize_pair_idx(pair_idx);

        let mut next_frame = 0usize;
        while next_frame < frames.len() {
            let submitted = {
                let mut state = self.queue_pairs[pair_idx].tx_state.lock();
                Self::drain_tx_completions_when_full(&mut state, frames.len() - next_frame);
                self.submit_available_tx_frames(&mut state, frames, &mut next_frame)?
            };

            if submitted != 0 {
                continue;
            }

            let should_wait = {
                let mut state = self.queue_pairs[pair_idx].tx_state.lock();
                Self::drain_tx_completions(&mut state, usize::MAX);
                state.tx_queue.available_descriptors() == 0
            };
            if should_wait {
                wait().await;
            }
        }
        Ok(())
    }

    fn validate_tx_frames<Frame>(&self, frames: &[Frame]) -> IoResult<()>
    where
        Frame: TxFrame,
    {
        for frame in frames {
            let frame = frame.frame_bytes();
            if frame.is_empty() || frame.len() > self.max_frame_len {
                return Err(IoError::InvalidBufferLength {
                    required_multiple: 1,
                    actual: frame.len(),
                });
            }
        }
        Ok(())
    }

    fn submit_available_tx_frames<Frame>(
        &self,
        state: &mut NetTxState<T>,
        frames: &[Frame],
        next_frame: &mut usize,
    ) -> IoResult<usize>
    where
        Frame: TxFrame,
    {
        let NetTxState {
            tx_queue,
            tx_buffers,
            tx_buffer_len,
            tx_in_flight,
        } = state;
        let mut submitted = 0usize;
        let available_frames = tx_queue
            .available_descriptors()
            .min(frames.len().saturating_sub(*next_frame));
        while submitted < available_frames {
            let frame = frames[*next_frame].frame_bytes();
            let checksum = frames[*next_frame].tx_checksum();
            if checksum.is_some() {
                assert!(
                    self.tx_checksum_negotiated,
                    "checksum-offload frame submitted without negotiated VIRTIO_NET_F_CSUM"
                );
            }
            let token = tx_queue.next_free_descriptor();
            let token_index = usize::from(token);
            assert!(
                !tx_in_flight.get(token_index),
                "virtio net TX descriptor {token} is still in flight"
            );
            let payload_len = write_tx_payload(
                slot_buffer_mut(tx_buffers, *tx_buffer_len, token_index, "TX"),
                self.header_len,
                frame,
                checksum,
            )?;
            let payload = slot_buffer(tx_buffers, *tx_buffer_len, token_index, payload_len, "TX");
            let submitted_token = tx_queue.submit_read_only_deferred(&self.transport, payload)?;
            assert_eq!(
                submitted_token, token,
                "virtio net TX descriptor allocation moved while payload was prepared"
            );
            tx_in_flight.set(token_index);
            submitted += 1;
            *next_frame += 1;
        }
        if submitted != 0 {
            tx_queue.publish();
            tx_queue.notify(&self.transport);
        }
        Ok(submitted)
    }

    pub async fn reclaim_transmit_completions(&self, budget: usize) -> IoResult<usize> {
        self.reclaim_transmit_completions_on_pair(0, budget).await
    }

    pub async fn reclaim_transmit_completions_on_pair(
        &self,
        pair_idx: usize,
        budget: usize,
    ) -> IoResult<usize> {
        let pair_idx = self.normalize_pair_idx(pair_idx);
        let mut state = self.queue_pairs[pair_idx].tx_state.lock();
        Ok(Self::drain_tx_completions(&mut state, budget))
    }

    pub fn reclaim_transmit_completions_immediate(&self, budget: usize) -> IoResult<Option<usize>> {
        self.reclaim_transmit_completions_immediate_on_pair(0, budget)
    }

    pub fn reclaim_transmit_completions_immediate_on_pair(
        &self,
        pair_idx: usize,
        budget: usize,
    ) -> IoResult<Option<usize>> {
        let pair_idx = self.normalize_pair_idx(pair_idx);
        let Some(mut state) = self.queue_pairs[pair_idx].tx_state.try_lock() else {
            return Ok(None);
        };
        Ok(Some(Self::drain_tx_completions(&mut state, budget)))
    }

    /// Immediate zero-copy TX: headers are copied into the descriptor
    /// slot behind the virtio-net header while the payload is chained
    /// as an external read-only descriptor. Accepted frames report
    /// their head token through `tokens`; the caller must keep each
    /// payload alive until `reclaim_transmit_tokens_immediate_on_pair`
    /// returns that token.
    pub fn try_transmit_scatter_immediate_on_pair(
        &self,
        pair_idx: usize,
        frames: &[TxScatterFrame<'_>],
        tokens: &mut [Option<u16>],
    ) -> IoResult<Option<usize>> {
        assert!(
            tokens.len() >= frames.len(),
            "scatter transmit token slots must cover the frame batch"
        );
        let pair_idx = self.normalize_pair_idx(pair_idx);
        let Some(mut state) = self.queue_pairs[pair_idx].tx_state.try_lock() else {
            return Ok(None);
        };
        Self::drain_tx_completions_when_full(&mut state, frames.len());
        let NetTxState {
            tx_queue,
            tx_buffers,
            tx_buffer_len,
            tx_in_flight,
        } = &mut *state;
        let mut submitted = 0usize;
        for frame in frames {
            if frame.checksum.is_some() {
                assert!(
                    self.tx_checksum_negotiated,
                    "checksum-offload frame submitted without negotiated VIRTIO_NET_F_CSUM"
                );
            }
            // A chain needs two descriptors (slot prefix + payload).
            if tx_queue.available_descriptors() < 2 {
                break;
            }
            let token = tx_queue.next_free_descriptor();
            let token_index = usize::from(token);
            assert!(
                !tx_in_flight.get(token_index),
                "virtio net TX descriptor {token} is still in flight"
            );
            let slot = slot_buffer_mut(tx_buffers, *tx_buffer_len, token_index, "TX");
            let prefix_len =
                write_tx_payload(slot, self.header_len, frame.headers, frame.checksum)?;
            let prefix = slot_buffer(tx_buffers, *tx_buffer_len, token_index, prefix_len, "TX");
            let submitted_token = if frame.payload.is_empty() {
                tx_queue.submit_read_only_deferred(&self.transport, prefix)?
            } else {
                tx_queue
                    .submit_read_only_chain_deferred(&self.transport, &[prefix, frame.payload])?
            };
            assert_eq!(
                submitted_token, token,
                "virtio net TX descriptor allocation moved while scatter frame was prepared"
            );
            tx_in_flight.set(token_index);
            tokens[submitted] = Some(token);
            submitted += 1;
        }
        if submitted != 0 {
            tx_queue.publish();
            tx_queue.notify(&self.transport);
        }
        Ok(Some(submitted))
    }

    /// Immediate TX completion drain that reports which head tokens
    /// finished, releasing zero-copy payload pins held by the caller.
    pub fn reclaim_transmit_tokens_immediate_on_pair(
        &self,
        pair_idx: usize,
        tokens: &mut [Option<u16>],
    ) -> IoResult<Option<usize>> {
        let pair_idx = self.normalize_pair_idx(pair_idx);
        let Some(mut state) = self.queue_pairs[pair_idx].tx_state.try_lock() else {
            return Ok(None);
        };
        let mut completed = 0usize;
        while completed < tokens.len() {
            let Some(token) = state.tx_queue.pop_used() else {
                break;
            };
            let token_index = usize::from(token);
            assert!(
                state.tx_in_flight.get(token_index),
                "virtio net TX completion referenced idle descriptor {token}"
            );
            state.tx_in_flight.clear(token_index);
            tokens[completed] = Some(token);
            completed += 1;
        }
        Ok(Some(completed))
    }

    pub async fn wait_for_interrupt(&self) {
        self.interrupts.notified().await;
    }

    fn rx_frame_from_slot(&self, pair_idx: usize, slot_index: u16, used_len: usize) -> RxFrame {
        Bytes::from_owner(RxFrameOwner {
            slot: self.queue_pairs[pair_idx].rx_slots[usize::from(slot_index)].clone(),
            range: self.header_len..used_len,
        })
    }

    fn drain_returned_rx_buffers(
        &self,
        pair_idx: usize,
        state: &mut NetRxState<T>,
    ) -> IoResult<()> {
        let returned = &self.queue_pairs[pair_idx].rx_returned;
        let mut slots = returned.slots.lock();
        if slots.is_empty() {
            return Ok(());
        }
        while let Some(slot_index) = slots.pop() {
            self.repost_rx_buffer_deferred(pair_idx, state, slot_index)?;
        }
        state.rx_queue.publish();
        // Kick the device: after the RX ring runs dry QEMU re-enables
        // notification and parks arrived packets until the guest signals
        // fresh buffers, so an unkicked repost leaves the receive path
        // stalled until the peer retransmits.
        state.rx_queue.notify(&self.transport);
        Ok(())
    }

    fn repost_rx_buffer(
        &self,
        pair_idx: usize,
        state: &mut NetRxState<T>,
        slot_index: u16,
    ) -> IoResult<()> {
        self.repost_rx_buffer_deferred(pair_idx, state, slot_index)?;
        state.rx_queue.publish();
        state.rx_queue.notify(&self.transport);
        Ok(())
    }

    fn repost_rx_buffer_deferred(
        &self,
        pair_idx: usize,
        state: &mut NetRxState<T>,
        slot_index: u16,
    ) -> IoResult<()> {
        let slot = usize::from(slot_index);
        assert!(
            !state.rx_in_device.get(slot),
            "virtio net RX buffer was reposted while still owned by the device"
        );
        let token = state.rx_queue.submit_output_deferred(
            &self.transport,
            self.queue_pairs[pair_idx].rx_slots[slot].buffer_mut(),
        )?;
        state.rx_slot_for_token[usize::from(token)] = slot_index;
        state.rx_in_device.set(slot);
        Ok(())
    }

    /// Resolves a completed descriptor identifier back to the receive
    /// slot it was carrying and marks that slot as driver-owned again.
    fn complete_rx_slot(state: &mut NetRxState<T>, token: u16) -> u16 {
        let slot_index = *state
            .rx_slot_for_token
            .get(usize::from(token))
            .unwrap_or_else(|| panic!("virtio net RX completion named unknown descriptor {token}"));
        let slot = usize::from(slot_index);
        assert!(
            state.rx_in_device.get(slot),
            "virtio net RX completion referenced an idle slot {slot_index}"
        );
        state.rx_in_device.clear(slot);
        slot_index
    }

    fn drain_tx_completions(state: &mut NetTxState<T>, budget: usize) -> usize {
        let mut completed = 0usize;
        while completed < budget {
            let Some(token) = state.tx_queue.pop_used() else {
                break;
            };
            let token_index = usize::from(token);
            assert!(
                state.tx_in_flight.get(token_index),
                "virtio net TX completion referenced idle descriptor {token}"
            );
            state.tx_in_flight.clear(token_index);
            completed += 1;
        }
        completed
    }

    fn drain_tx_completions_when_full(state: &mut NetTxState<T>, budget: usize) -> usize {
        if state.tx_queue.available_descriptors() != 0 {
            return 0;
        }
        Self::drain_tx_completions(state, budget.max(1))
    }
}

impl<T: VirtioTransport> Drop for VirtioNetDevice<T> {
    fn drop(&mut self) {
        // The driver owns the transport, so releasing the device-side
        // queues belongs here rather than inside the queue: a queue has
        // no way to reach the register that resets it.
        for pair in self.queue_pairs.iter_mut() {
            pair.rx_state.get_mut().rx_queue.shutdown(&self.transport);
            pair.tx_state.get_mut().tx_queue.shutdown(&self.transport);
        }
        if let Some(control) = self.control.as_mut() {
            control.get_mut().queue.shutdown(&self.transport);
        }
    }
}

impl DescriptorBitSet {
    fn new(bits: usize) -> Self {
        assert!(
            bits <= usize::from(NET_QUEUE_SIZE),
            "virtio descriptor bitset exceeds net queue capacity"
        );
        Self {
            words: [0; DESCRIPTOR_BITSET_WORDS],
            bits,
        }
    }

    fn get(&self, bit: usize) -> bool {
        let (word, mask) = self.word_mask(bit);
        self.words[word] & mask != 0
    }

    fn set(&mut self, bit: usize) {
        let (word, mask) = self.word_mask(bit);
        self.words[word] |= mask;
    }

    fn clear(&mut self, bit: usize) {
        let (word, mask) = self.word_mask(bit);
        self.words[word] &= !mask;
    }

    fn word_mask(&self, bit: usize) -> (usize, usize) {
        assert!(
            bit < self.bits,
            "virtio descriptor bit {bit} is outside bitset"
        );
        let word = bit / usize::BITS as usize;
        let shift = bit % usize::BITS as usize;
        (word, 1usize << shift)
    }
}

fn read_max_virtqueue_pairs<T: VirtioTransport>(transport: &T) -> u16 {
    // virtio-net config layout (when F_MQ negotiated): mac (6B),
    // status (2B), max_virtqueue_pairs (2B at offset 8).
    let config = transport.read_config_u32(8).to_le_bytes();
    u16::from_le_bytes([config[0], config[1]])
}

fn read_mac_address<T: VirtioTransport>(transport: &T) -> [u8; 6] {
    let low = transport.read_config_u32(0).to_le_bytes();
    let high = transport.read_config_u32(4).to_le_bytes();
    [low[0], low[1], low[2], low[3], high[0], high[1]]
}

fn read_mtu<T: VirtioTransport>(transport: &T, features: NegotiatedFeatures) -> usize {
    if !features.device(NET_FEATURE_MTU) {
        return DEFAULT_IP_MTU;
    }

    let config = transport.read_config_u32(8).to_le_bytes();
    let mtu = u16::from_le_bytes([config[2], config[3]]) as usize;
    if mtu == 0 {
        return DEFAULT_IP_MTU;
    }
    mtu
}

fn slot_buffer_mut<'a>(
    buffers: &'a mut [u8],
    buffer_len: usize,
    token_index: usize,
    queue: &str,
) -> &'a mut [u8] {
    let start = token_index.checked_mul(buffer_len).unwrap_or_else(|| {
        panic!("virtio net {queue} token {token_index} buffer offset overflowed")
    });
    let end = start
        .checked_add(buffer_len)
        .unwrap_or_else(|| panic!("virtio net {queue} token {token_index} buffer end overflowed"));
    buffers.get_mut(start..end).unwrap_or_else(|| {
        panic!("virtio net {queue} token {token_index} is outside the buffer slab")
    })
}

fn slot_buffer<'a>(
    buffers: &'a [u8],
    buffer_len: usize,
    token_index: usize,
    payload_len: usize,
    queue: &str,
) -> &'a [u8] {
    assert!(
        payload_len <= buffer_len,
        "virtio net payload length exceeds slot capacity"
    );
    let start = token_index.checked_mul(buffer_len).unwrap_or_else(|| {
        panic!("virtio net {queue} token {token_index} buffer offset overflowed")
    });
    let end = start
        .checked_add(payload_len)
        .unwrap_or_else(|| panic!("virtio net {queue} token {token_index} payload end overflowed"));
    buffers.get(start..end).unwrap_or_else(|| {
        panic!("virtio net {queue} token {token_index} is outside the buffer slab")
    })
}

fn write_tx_payload(
    buffer: &mut [u8],
    header_len: usize,
    frame: &[u8],
    checksum: Option<TxChecksumMeta>,
) -> IoResult<usize> {
    write_tx_payload_gso(buffer, header_len, frame, checksum, None)
}

fn write_tx_payload_gso(
    buffer: &mut [u8],
    header_len: usize,
    frame: &[u8],
    checksum: Option<TxChecksumMeta>,
    gso: Option<TxGsoMeta>,
) -> IoResult<usize> {
    let payload_len = header_len
        .checked_add(frame.len())
        .ok_or(IoError::DeviceFault)?;
    if payload_len > buffer.len() {
        return Err(IoError::InvalidBufferLength {
            required_multiple: 1,
            actual: frame.len(),
        });
    }

    // Slots are reused across frames with different offload needs, so the
    // header is rewritten every submission: NEEDS_CSUM with the frame's
    // csum_start/csum_offset (and GSO fields when segmenting), or all-zero
    // "no offload".
    buffer[..header_len].fill(0);
    if let Some(meta) = checksum {
        buffer[0] = VIRTIO_NET_HDR_F_NEEDS_CSUM;
        buffer[6..8].copy_from_slice(&meta.start.to_le_bytes());
        buffer[8..10].copy_from_slice(&meta.offset.to_le_bytes());
    }
    buffer[1] = match gso {
        None => VIRTIO_NET_HDR_GSO_NONE,
        Some(meta) => {
            assert!(
                checksum.is_some(),
                "GSO transmit requires transmit checksum offload"
            );
            buffer[2..4].copy_from_slice(&meta.hdr_len.to_le_bytes());
            buffer[4..6].copy_from_slice(&meta.mss.to_le_bytes());
            if meta.ipv6 {
                VIRTIO_NET_HDR_GSO_TCPV6
            } else {
                VIRTIO_NET_HDR_GSO_TCPV4
            }
        }
    };
    buffer[header_len..payload_len].copy_from_slice(frame);
    Ok(payload_len)
}

#[cfg(test)]
mod tests {
    use core::mem::size_of;

    use super::{
        DescriptorBitSet, TxChecksumMeta, TxGsoMeta, VirtioNetHeader, write_tx_payload,
        write_tx_payload_gso,
    };

    #[test]
    fn descriptor_bitset_tracks_sparse_tokens() {
        let mut bits = DescriptorBitSet::new(130);

        bits.set(0);
        bits.set(65);
        bits.set(129);

        assert!(bits.get(0));
        assert!(bits.get(65));
        assert!(bits.get(129));
        assert!(!bits.get(64));

        bits.clear(65);
        assert!(bits.get(0));
        assert!(!bits.get(65));
        assert!(bits.get(129));
    }

    #[test]
    fn tx_payload_write_populates_gso_header() {
        let header_len = size_of::<VirtioNetHeader>();
        let mut buffer = [0u8; 256];
        let frame = [0u8; 60];
        write_tx_payload_gso(
            &mut buffer,
            header_len,
            &frame,
            Some(TxChecksumMeta {
                start: 34,
                offset: 16,
            }),
            Some(TxGsoMeta {
                ipv6: false,
                hdr_len: 54,
                mss: 1460,
            }),
        )
        .expect("gso payload fits");
        assert_eq!(buffer[0], super::VIRTIO_NET_HDR_F_NEEDS_CSUM);
        assert_eq!(buffer[1], super::VIRTIO_NET_HDR_GSO_TCPV4);
        assert_eq!(&buffer[2..4], &54u16.to_le_bytes());
        assert_eq!(&buffer[4..6], &1460u16.to_le_bytes());
        assert_eq!(&buffer[6..8], &34u16.to_le_bytes());

        // A non-GSO frame reusing the slot scrubs gso_type back to NONE.
        write_tx_payload(&mut buffer, header_len, &frame, None).expect("plain payload fits");
        assert_eq!(buffer[1], super::VIRTIO_NET_HDR_GSO_NONE);
        assert_eq!(&buffer[2..6], &[0u8; 4]);
    }

    #[test]
    fn tx_payload_write_rewrites_net_header_per_frame() {
        let zero_header = [0; size_of::<VirtioNetHeader>()];
        let header_len = zero_header.len();
        let mut buffer = [0u8; 64];
        let first = [1u8; 8];
        let second = [2u8; 4];

        assert_eq!(
            write_tx_payload(
                &mut buffer,
                header_len,
                &first,
                Some(TxChecksumMeta {
                    start: 34,
                    offset: 16,
                }),
            )
            .expect("first payload fits"),
            header_len + first.len()
        );
        assert_eq!(buffer[0], super::VIRTIO_NET_HDR_F_NEEDS_CSUM);
        assert_eq!(&buffer[6..8], &34u16.to_le_bytes());
        assert_eq!(&buffer[8..10], &16u16.to_le_bytes());
        assert_eq!(&buffer[header_len..header_len + first.len()], first);

        // A non-offload frame reusing the slot must scrub the stale
        // NEEDS_CSUM header back to all-zero.
        assert_eq!(
            write_tx_payload(&mut buffer, header_len, &second, None).expect("second payload fits"),
            header_len + second.len()
        );
        assert_eq!(&buffer[..header_len], zero_header);
        assert_eq!(&buffer[header_len..header_len + second.len()], second);
    }
}
