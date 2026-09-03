//! virtio-net driver.
//!
//! Receive memory rule: every buffer the device ever writes into is
//! allocated once, at bring-up, and recycled for the lifetime of the
//! device. Nothing on the receive path allocates per packet.
//!
//! Two pools per queue pair carry that. The receive ring is backed by
//! one page-granular slot per descriptor, handed to the device as a
//! single writable buffer; a frame that fits one slot is delivered as an
//! owning [`RxFrame`] borrowing that slot, so the common path copies
//! nothing. With `VIRTIO_NET_F_MRG_RXBUF` the device may spread one
//! frame across several consecutive slots, and since the network stack
//! parses contiguous frames those chains are assembled into a buffer
//! from the second pool: a small set of maximum-size frame buffers,
//! sized so the pool can absorb every byte the receive ring can hold.
//! The assembled frame borrows its pool buffer the same way, and the
//! buffer returns to the pool when the stack drops the frame. A drain
//! that finds the pool empty stops and leaves the used entries in the
//! ring for the next call rather than dropping frames.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use async_lock::Mutex as AsyncMutex;
use bytes::Bytes;
use core::cell::UnsafeCell;
use core::mem::size_of;
use core::ops::Range;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use helios_hal::io::{IoError, IoResult};
use helios_netstack::{
    ChecksumOffload, DEFAULT_POLL_BUDGET, EventDeliveryCapabilities, FlowHash,
    InterfaceCapabilities, LinkState, QueueTopology, RSS_INDIRECTION_ENTRIES, RSS_KEY_BYTES,
    RxChecksumReport, RxFrameOffload, STANDARD_RSS_KEY, SegmentationOffload, rss_indirection_entry,
};
use spin::Mutex as SpinMutex;

use crate::features::{NegotiatedFeatures, RING_FEATURES, negotiate_with};
use crate::notify::Notify;
use crate::queue::VirtQueue;
use crate::transport::{DeviceStatus, DeviceType, VirtioTransport};

const NET_QUEUE_SIZE: u16 = 256;
/// Receive buffers are always a single writable descriptor.
const RX_CHAIN_LIMIT: u16 = 1;
/// Receive slot granularity. A page per descriptor is what makes
/// `VIRTIO_NET_F_MRG_RXBUF` worth negotiating: MTU-sized frames leave
/// most of a slot unused, while a device-coalesced large receive frame
/// spans as many slots as it needs instead of forcing every slot to be
/// 64 KiB.
const RX_PAGE_BYTES: usize = 4096;
/// Largest Ethernet frame a device with receive segmentation offload may
/// deliver (virtio 1.2 §5.1.6.3.1 sizes its non-mergeable receive
/// buffers at this plus the 12-byte header).
const MAX_LARGE_RECEIVE_FRAME_BYTES: usize = 65_550;
/// Largest oversized frame the driver submits to a segmenting device.
/// An IP length field describes at most 65535 bytes of packet, and the
/// Ethernet header the device replicates in front of every segment
/// rides on top of it.
const MAX_SEGMENTED_TRANSMIT_FRAME_BYTES: usize = u16::MAX as usize + ETH_HEADER_LEN;
/// Upper bound on the reassembly buffers one queue pair keeps. Without
/// receive segmentation offload a frame barely outgrows a page, so the
/// byte-capacity rule alone would allocate hundreds of them for chains
/// that a well-behaved device never even produces.
const MAX_RX_REASSEMBLY_BUFFERS: usize = 64;
/// A transmit frame is either one slot-resident buffer or a slot-resident
/// header prefix chained to an external payload.
///
/// Two is the deliberate ceiling, not a placeholder. Everything the
/// network stack hands the driver is a header prefix plus at most one
/// refcounted payload buffer, and that buffer is a single kernel-heap
/// allocation, which every target's DMA pool maps linearly onto one
/// contiguous physical range. A third descriptor would have no
/// producer, and with `VIRTIO_F_INDIRECT_DESC` the limit also sizes the
/// indirect table allocated for every descriptor in the ring, so
/// widening it speculatively costs memory on every queue pair.
const TX_CHAIN_LIMIT: u16 = 2;
/// A control command is a read-only command buffer plus a writable ack.
const CONTROL_CHAIN_LIMIT: u16 = 2;
const DESCRIPTOR_BITSET_WORDS: usize = (NET_QUEUE_SIZE as usize).div_ceil(usize::BITS as usize);
const ETH_HEADER_LEN: usize = 14;
const DEFAULT_IP_MTU: usize = 1500;
/// VIRTIO_NET_F_CSUM: the driver may submit frames with partial
/// checksums that the device completes from csum_start/csum_offset.
const NET_FEATURE_CSUM: u64 = 1 << 0;
/// VIRTIO_NET_F_HOST_TSO4/6: the device performs TCP segmentation, so
/// the driver may submit one oversized segment with GSO metadata.
const NET_FEATURE_HOST_TSO4: u64 = 1 << 11;
const NET_FEATURE_HOST_TSO6: u64 = 1 << 12;
/// VIRTIO_NET_F_HOST_ECN: the device segments ECN-capable TCP, so the
/// driver may set `VIRTIO_NET_HDR_GSO_ECN` on an oversized frame whose
/// header template carries CWR. A modifier on a TSO family, never
/// requested on its own.
const NET_FEATURE_HOST_ECN: u64 = 1 << 13;
const NET_FEATURE_MAC: u64 = 1 << 5;
const NET_FEATURE_STATUS: u64 = 1 << 16;
const NET_FEATURE_MTU: u64 = 1 << 3;
/// VIRTIO_NET_F_GUEST_CSUM: the driver accepts frames whose transport
/// checksum the device either validated or left partial, so the stack
/// can skip a software verification per frame.
const NET_FEATURE_GUEST_CSUM: u64 = 1 << 1;
/// VIRTIO_NET_F_GUEST_TSO4/6, _GUEST_ECN, _GUEST_UFO: the driver accepts
/// receive frames the device coalesced out of several wire segments.
/// All four require GUEST_CSUM, and ECN additionally requires one of the
/// TSO families (virtio 1.2 §5.1.3.1).
const NET_FEATURE_GUEST_TSO4: u64 = 1 << 7;
const NET_FEATURE_GUEST_TSO6: u64 = 1 << 8;
const NET_FEATURE_GUEST_ECN: u64 = 1 << 9;
const NET_FEATURE_GUEST_UFO: u64 = 1 << 10;
/// VIRTIO_NET_F_MRG_RXBUF: the device may spread one receive frame
/// across several buffers, reporting the count in the header's
/// `num_buffers`. Required for large receive offload with buffers
/// smaller than 64 KiB.
const NET_FEATURE_MRG_RXBUF: u64 = 1 << 15;
/// Byte offset of the `status` field in the virtio-net configuration
/// space (mac[6], status[2], max_virtqueue_pairs[2], mtu[2]).
const NET_CONFIG_STATUS_OFFSET: usize = 6;
/// `duplex`, after mac[6], status[2], max_virtqueue_pairs[2], mtu[2]
/// and speed[4]. The RSS limits share its aligned dword.
const NET_CONFIG_DUPLEX_OFFSET: usize = 16;
/// `rss_max_key_size`, after mac[6], status[2], max_virtqueue_pairs[2],
/// mtu[2], speed[4] and duplex[1].
const NET_CONFIG_RSS_MAX_KEY_SIZE_OFFSET: usize = 17;
/// `rss_max_indirection_table_length`, the entry count the device can
/// hold minus one is *not* what this reports: it is the length itself.
const NET_CONFIG_RSS_MAX_TABLE_LEN_OFFSET: usize = 18;
/// `supported_hash_types`.
const NET_CONFIG_SUPPORTED_HASH_TYPES_OFFSET: usize = 20;
/// VIRTIO_NET_S_LINK_UP: the device reports carrier in `status`.
const NET_STATUS_LINK_UP: u16 = 1;
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
/// VIRTIO_NET_F_RSS: the device steers received frames across its queue
/// pairs by hashing the flow and reading a queue out of a driver-programmed
/// indirection table. What makes a per-CPU queue layout pay: without it
/// every frame arrives on queue zero and reaches its owner's processor
/// only after a cross-core hand-off.
const NET_FEATURE_RSS: u64 = 1 << 60;
/// VIRTIO_NET_F_HASH_REPORT: the device writes the hash it computed into
/// the receive header, so the driver does not have to compute the same
/// number a second time. Costs eight extra header bytes per frame in
/// both directions.
const NET_FEATURE_HASH_REPORT: u64 = 1 << 57;
/// VIRTIO_NET_F_NOTF_COAL: the device can hold a notification back
/// until a packet count or a delay is reached, so a busy queue raises
/// one interrupt per batch instead of one per descriptor. Programmed
/// through the control queue, so it needs `VIRTIO_NET_F_CTRL_VQ`.
const NET_FEATURE_NOTF_COAL: u64 = 1 << 53;
/// VIRTIO_NET_F_VQ_NOTF_COAL: coalescing can be programmed per
/// virtqueue rather than once for every receive and every transmit
/// queue. What a per-CPU queue layout wants, because each pair is
/// driven by its own processor at its own rate.
const NET_FEATURE_VQ_NOTF_COAL: u64 = 1 << 52;
/// Maximum queue pairs the kernel is willing to bring up. Sized so
/// the per-CPU shard count on Apple Silicon (up to 12 cores) fits;
/// devices advertising more pairs are simply capped at this value.
const NET_MAX_QUEUE_PAIRS: u16 = 16;
/// Control-queue command class for `VIRTIO_NET_CTRL_MQ`.
const CTRL_CLASS_MQ: u8 = 4;
/// Control-queue command id for `VQ_PAIRS_SET` under class MQ.
const CTRL_CMD_MQ_VQ_PAIRS_SET: u8 = 0;
/// Control-queue command id for `RSS_CONFIG` under class MQ. It carries
/// the active queue-pair count itself, so a device that steers is
/// configured with this instead of `VQ_PAIRS_SET`.
const CTRL_CMD_MQ_RSS_CONFIG: u8 = 1;
/// `VIRTIO_NET_HASH_TYPE_*`: the flows the device is asked to hash.
///
/// Only the four-tuple types are requested. A frame the device cannot
/// classify goes to `unclassified_queue`, which is queue zero — exactly
/// where the driver's own rule puts a frame with no flow to hash.
const NET_HASH_TYPE_TCPV4: u32 = 1 << 1;
const NET_HASH_TYPE_UDPV4: u32 = 1 << 2;
const NET_HASH_TYPE_TCPV6: u32 = 1 << 4;
const NET_HASH_TYPE_UDPV6: u32 = 1 << 5;
const NET_RSS_HASH_TYPES: u32 =
    NET_HASH_TYPE_TCPV4 | NET_HASH_TYPE_UDPV4 | NET_HASH_TYPE_TCPV6 | NET_HASH_TYPE_UDPV6;
/// `VIRTIO_NET_HASH_REPORT_NONE`: the device classified nothing, so the
/// hash field of the receive header says nothing either.
const NET_HASH_REPORT_NONE: u16 = 0;
/// Control-queue command class for `VIRTIO_NET_CTRL_NOTF_COAL`.
const CTRL_CLASS_NOTF_COAL: u8 = 6;
/// Coalescing command ids under class `VIRTIO_NET_CTRL_NOTF_COAL`.
const CTRL_CMD_NOTF_COAL_TX_SET: u8 = 0;
const CTRL_CMD_NOTF_COAL_RX_SET: u8 = 1;
const CTRL_CMD_NOTF_COAL_VQ_SET: u8 = 2;
/// Pre-submission ack sentinel; the device replaces this with
/// `CTRL_ACK_OK` (0) on success or `CTRL_ACK_FAIL` (1) on error.
const CTRL_ACK_PENDING: u8 = 0xff;
/// Device-side success ack on the control queue.
const CTRL_ACK_OK: u8 = 0;
/// Bytes the `VQ_PAIRS_SET` payload occupies: one le16 pair count.
const CTRL_MQ_PAIRS_PAYLOAD_BYTES: usize = 2;
/// Bytes a `NOTF_COAL_TX_SET` / `NOTF_COAL_RX_SET` payload occupies:
/// `le32 max_packets` then `le32 max_usecs`.
const CTRL_NOTF_COAL_PAYLOAD_BYTES: usize = 8;
/// Bytes a `NOTF_COAL_VQ_SET` payload occupies: `le16 vqn`, `le16`
/// reserved, then the same pair of le32 fields.
const CTRL_NOTF_COAL_VQ_PAYLOAD_BYTES: usize = 12;
/// Bytes a `RSS_CONFIG` payload occupies: `struct virtio_net_rss_config`
/// — hash types, table mask, unclassified queue, the table itself, the
/// transmit queue count, and the key with its length byte.
const CTRL_RSS_CONFIG_PAYLOAD_BYTES: usize =
    4 + 2 + 2 + 2 * RSS_INDIRECTION_ENTRIES + 2 + 1 + RSS_KEY_BYTES;
/// Maximum command payload size the control queue scratch buffer is
/// sized for, plus the two-byte class/command prefix every command
/// carries.
const CTRL_CMD_MAX_BYTES: usize = 2 + CTRL_RSS_CONFIG_PAYLOAD_BYTES;
/// The scratch buffer is sized once at bring-up and shared by every
/// control command, so a command that outgrew it would assert at
/// runtime on a path only real hardware reaches. Checking it here means
/// a new command with a longer payload fails the build instead.
const _: () = {
    assert!(CTRL_CMD_MAX_BYTES >= 2 + CTRL_MQ_PAIRS_PAYLOAD_BYTES);
    assert!(CTRL_CMD_MAX_BYTES >= 2 + CTRL_NOTF_COAL_PAYLOAD_BYTES);
    assert!(CTRL_CMD_MAX_BYTES >= 2 + CTRL_NOTF_COAL_VQ_PAYLOAD_BYTES);
    assert!(CTRL_CMD_MAX_BYTES >= 2 + CTRL_RSS_CONFIG_PAYLOAD_BYTES);
    // The device masks the hash to an indirection-table slot, so the
    // table length it is told about has to be a power of two.
    assert!(RSS_INDIRECTION_ENTRIES.is_power_of_two());
};

/// Frames a queue may accumulate before the device raises its
/// notification.
///
/// Small enough that a latency-sensitive exchange still gets its
/// interrupt from the delay bound below rather than waiting for a
/// batch that will never fill, and large enough that a saturated queue
/// raises one interrupt per receive batch instead of one per frame.
const NET_NOTF_COAL_MAX_PACKETS: u32 = 8;
/// Microseconds a queue may hold a notification back before raising it
/// even though the packet count was never reached.
const NET_NOTF_COAL_MAX_USECS: u32 = 50;

/// Bytes of `struct virtio_net_hdr_v1_hash`: the ordinary header plus
/// the reported hash, its report type, and two bytes of padding. Both
/// directions carry it once `VIRTIO_NET_F_HASH_REPORT` is negotiated.
const HASH_REPORT_HEADER_BYTES: usize = size_of::<VirtioNetHeader>() + 8;

/// virtqueue index of a queue pair's receive ring.
///
/// virtio-net lays its queues out as RX0, TX0, RX1, TX1, ... followed by
/// the control queue, and three call sites depend on that ordering, so
/// it is spelled once here.
const fn rx_queue_index(pair_idx: u16) -> u16 {
    pair_idx * 2
}

/// virtqueue index of a queue pair's transmit ring.
const fn tx_queue_index(pair_idx: u16) -> u16 {
    pair_idx * 2 + 1
}

/// virtqueue index of the control queue, which follows every pair the
/// *device* advertises rather than every pair the driver brought up.
const fn control_queue_index(device_max_pairs: u16) -> u16 {
    device_max_pairs * 2
}

/// How long a queue may hold a notification back.
///
/// Both bounds apply at once: whichever of the packet count and the
/// delay is reached first releases the notification, so a saturated
/// queue coalesces by count and an idle one is still bounded by time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CoalescingBudget {
    max_packets: u32,
    max_usecs: u32,
}

impl CoalescingBudget {
    /// `struct virtio_net_ctrl_coal`: the payload of a device-wide
    /// `NOTF_COAL_RX_SET` / `NOTF_COAL_TX_SET`.
    fn encode(self) -> [u8; CTRL_NOTF_COAL_PAYLOAD_BYTES] {
        let mut payload = [0; CTRL_NOTF_COAL_PAYLOAD_BYTES];
        payload[..4].copy_from_slice(&self.max_packets.to_le_bytes());
        payload[4..].copy_from_slice(&self.max_usecs.to_le_bytes());
        payload
    }

    /// `struct virtio_net_ctrl_coal_vq`: the same budget addressed to
    /// one virtqueue, which is what a per-CPU queue layout programs.
    fn encode_for_queue(self, queue: u16) -> [u8; CTRL_NOTF_COAL_VQ_PAYLOAD_BYTES] {
        let mut payload = [0; CTRL_NOTF_COAL_VQ_PAYLOAD_BYTES];
        payload[..2].copy_from_slice(&queue.to_le_bytes());
        // Bytes 2..4 are the reserved half of the header and stay zero.
        payload[4..].copy_from_slice(&self.encode());
        payload
    }
}

/// The queue pairs an interrupt found completions on.
///
/// A bitmask rather than a list because the caller is an interrupt
/// handler: no allocation, no iteration over pairs that did nothing,
/// and `NET_MAX_QUEUE_PAIRS` pairs fit a `u16` exactly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueuePairProgress(u16);

impl QueuePairProgress {
    pub const fn none() -> Self {
        Self(0)
    }

    fn record(&mut self, pair_idx: usize) {
        assert!(
            pair_idx < usize::from(NET_MAX_QUEUE_PAIRS),
            "queue pair {pair_idx} is outside the maximum this driver brings up"
        );
        self.0 |= 1 << pair_idx;
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The pairs with completions waiting, in index order.
    pub fn iter(self) -> impl Iterator<Item = usize> {
        (0..usize::from(NET_MAX_QUEUE_PAIRS)).filter(move |idx| self.0 & (1 << idx) != 0)
    }
}

/// What a device does about notification coalescing, as negotiated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NotificationCoalescing {
    /// `VIRTIO_NET_F_NOTF_COAL`: one budget for every receive queue and
    /// one for every transmit queue.
    pub device_wide: bool,
    /// `VIRTIO_NET_F_VQ_NOTF_COAL`: a budget per virtqueue.
    pub per_queue: bool,
}

impl NotificationCoalescing {
    /// Whether the device coalesces at all.
    pub const fn enabled(self) -> bool {
        self.device_wide
    }
}

/// Checksum-offload metadata for one transmit frame: the device
/// finishes the one's-complement sum from byte `start` of the frame and
/// stores the complemented result at `start + offset`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxChecksumMeta {
    pub start: u16,
    pub offset: u16,
}

impl From<helios_netstack::TxChecksum> for TxChecksumMeta {
    fn from(checksum: helios_netstack::TxChecksum) -> Self {
        Self {
            start: checksum.start,
            offset: checksum.offset,
        }
    }
}

impl From<helios_netstack::TxSegmentation> for TxGsoMeta {
    fn from(segmentation: helios_netstack::TxSegmentation) -> Self {
        Self {
            ipv6: segmentation.ipv6,
            hdr_len: segmentation.header_len,
            mss: segmentation.segment_bytes,
            ecn: segmentation.ecn,
        }
    }
}

/// VIRTIO_NET_HDR_F_NEEDS_CSUM: csum_start/csum_offset are valid.
const VIRTIO_NET_HDR_F_NEEDS_CSUM: u8 = 1;
/// VIRTIO_NET_HDR_F_DATA_VALID: the device validated the frame's
/// transport checksum. Never set together with NEEDS_CSUM.
const VIRTIO_NET_HDR_F_DATA_VALID: u8 = 2;
const VIRTIO_NET_HDR_GSO_NONE: u8 = 0;
const VIRTIO_NET_HDR_GSO_TCPV4: u8 = 1;
const VIRTIO_NET_HDR_GSO_UDP: u8 = 3;
const VIRTIO_NET_HDR_GSO_TCPV6: u8 = 4;
/// VIRTIO_NET_HDR_GSO_ECN rides on the segmentation type as a flag.
const VIRTIO_NET_HDR_GSO_ECN: u8 = 0x80;

/// TCP segmentation-offload metadata for one oversized transmit frame:
/// the device splits it into `mss`-payload segments, replicating the
/// `hdr_len`-byte header prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxGsoMeta {
    pub ipv6: bool,
    pub hdr_len: u16,
    pub mss: u16,
    /// The header template carries CWR, so the segments are ECN-capable
    /// and the device is told through `VIRTIO_NET_HDR_GSO_ECN`. Only
    /// meaningful with VIRTIO_NET_F_HOST_ECN negotiated.
    pub ecn: bool,
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

/// The virtio-net header the device wrote in front of a received frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RxHeader {
    flags: u8,
    gso_type: u8,
    gso_size: u16,
    csum_start: u16,
    csum_offset: u16,
    /// Buffers this frame occupies, valid only with
    /// `VIRTIO_NET_F_MRG_RXBUF`.
    num_buffers: u16,
    /// The flow hash the device computed, valid only with
    /// `VIRTIO_NET_F_HASH_REPORT` and only when `hash_report` says the
    /// device classified the frame.
    hash_value: u32,
    hash_report: u16,
}

impl RxHeader {
    /// Reads the header. Every field is little-endian, and the eight
    /// trailing hash bytes exist only under `VIRTIO_NET_F_HASH_REPORT`,
    /// which is what `header_len` records.
    fn parse(bytes: &[u8], header_len: usize) -> Self {
        assert!(
            header_len >= size_of::<VirtioNetHeader>() && bytes.len() >= header_len,
            "virtio net receive header is shorter than the header layout"
        );
        let (hash_value, hash_report) = if header_len >= HASH_REPORT_HEADER_BYTES {
            (
                u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
                u16::from_le_bytes([bytes[16], bytes[17]]),
            )
        } else {
            (0, NET_HASH_REPORT_NONE)
        };
        Self {
            flags: bytes[0],
            gso_type: bytes[1],
            gso_size: u16::from_le_bytes([bytes[4], bytes[5]]),
            csum_start: u16::from_le_bytes([bytes[6], bytes[7]]),
            csum_offset: u16::from_le_bytes([bytes[8], bytes[9]]),
            num_buffers: u16::from_le_bytes([bytes[10], bytes[11]]),
            hash_value,
            hash_report,
        }
    }

    /// The hash the device reported, if it reported one.
    const fn flow_hash(self) -> Option<FlowHash> {
        if self.hash_report == NET_HASH_REPORT_NONE {
            None
        } else {
            Some(FlowHash::new(self.hash_value))
        }
    }

    /// The segmentation type without its ECN flag.
    const fn gso_family(self) -> u8 {
        self.gso_type & !VIRTIO_NET_HDR_GSO_ECN
    }
}

/// The first buffer of a mergeable receive chain: which slot it landed
/// in, where in the available ring it was posted, and how many bytes the
/// device wrote into it.
#[derive(Clone, Copy)]
struct RxChainHead {
    slot: u16,
    position: u64,
    used_len: usize,
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
    /// Position in the available ring each in-flight descriptor was
    /// posted at, indexed by descriptor identifier.
    ///
    /// A mergeable receive chain is assembled out of consecutively
    /// available buffers, so the tail buffers of a frame must carry the
    /// positions that follow its head. Recording the order the driver
    /// made buffers available is what lets the reassembly check that
    /// assumption instead of trusting whatever the device completes
    /// next.
    rx_post_position: Box<[u64]>,
    /// Position the next posted buffer is stamped with.
    rx_next_post_position: u64,
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

/// Contiguous frame buffers that mergeable receive chains are assembled
/// into.
///
/// Allocated once at bring-up and never grown. The pool holds as many
/// maximum-size frame buffers as the receive ring can fill — its byte
/// capacity matches the ring's — so a drain only ever runs out while the
/// stack still holds frames handed to it earlier, and then it stops
/// instead of dropping anything. Devices whose largest possible frame
/// fits one receive slot never build a pool at all.
struct RxReassemblyPool {
    buffers: Box<[Arc<RxBufferSlot>]>,
    /// Buffers not currently backing a delivered frame. A buffer returns
    /// here through the same owner drop that recycles receive slots.
    free: Arc<RxReturnedSlots>,
}

impl RxReassemblyPool {
    fn new(count: usize, bytes: usize) -> IoResult<Self> {
        assert!(
            count != 0,
            "a reassembly pool must hold at least one buffer"
        );
        let free = Arc::new(RxReturnedSlots {
            slots: SpinMutex::new(Vec::with_capacity(count)),
        });
        let mut buffers = Vec::with_capacity(count);
        for index in 0..count {
            let slot = u16::try_from(index).map_err(|_| IoError::DeviceFault)?;
            buffers.push(Arc::new(RxBufferSlot {
                slot,
                returned: free.clone(),
                buffer: UnsafeCell::new(vec![0_u8; bytes].into_boxed_slice()),
            }));
            free.slots.lock().push(slot);
        }
        Ok(Self {
            buffers: buffers.into_boxed_slice(),
            free,
        })
    }

    fn has_free(&self) -> bool {
        !self.free.slots.lock().is_empty()
    }

    /// Takes a buffer out of the pool. It returns on its own when the
    /// frame assembled into it is dropped.
    fn take(&self) -> Option<Arc<RxBufferSlot>> {
        let slot = self.free.slots.lock().pop()?;
        Some(self.buffers[usize::from(slot)].clone())
    }
}

struct NetTxState<T: VirtioTransport> {
    tx_queue: VirtQueue<T>,
    tx_buffers: Box<[u8]>,
    tx_buffer_len: usize,
    tx_in_flight: DescriptorBitSet,
    /// Scatter payloads the device is reading in place, indexed by the
    /// descriptor identifier of the chain that points at them.
    ///
    /// A zero-copy submission chains caller-owned bytes by reference
    /// rather than copying them into the slot, so the driver keeps the
    /// refcounted handle for exactly as long as the device owns the
    /// descriptor: taken at submission, dropped when the used ring
    /// returns the identifier. The ring never reissues an identifier
    /// whose chain is still in flight, so one slot per descriptor is
    /// the whole bookkeeping.
    tx_payloads: Box<[Option<Bytes>]>,
}

impl<T: VirtioTransport> NetQueuePair<T> {
    /// Whether either ring of this pair has a completion nobody has
    /// taken yet.
    ///
    /// A pair another processor currently holds is reported as idle:
    /// that processor is draining it, so it needs no wake, and an
    /// interrupt handler must not wait for a lock in any case.
    /// Counts one interrupt for this pair and releases its waiters.
    fn raise_interrupt(&self) {
        self.interrupt_count.fetch_add(1, Ordering::Relaxed);
        self.interrupts.notify_all();
    }

    fn has_pending_completions(&self) -> bool {
        let receive = self
            .rx_state
            .try_lock()
            .is_some_and(|state| state.rx_queue.has_pending_used());
        let transmit = self
            .tx_state
            .try_lock()
            .is_some_and(|state| state.tx_queue.has_pending_used());
        receive || transmit
    }
}

/// One TX/RX queue pair as exposed by VIRTIO_NET_F_MQ. Each pair
/// owns its own pair of `VirtQueue`s, RX buffer slab, and TX buffer
/// slab so submissions on different CPUs do not contend on the same
/// SpinMutex / async lock.
struct NetQueuePair<T: VirtioTransport> {
    /// Interrupts raised for this pair alone.
    ///
    /// The distribution across pairs is the evidence that steering
    /// works: with per-queue vectors each of these is incremented on the
    /// processor that drains the pair, so the counts spread instead of
    /// piling on whichever processor owns a single vector.
    interrupt_count: AtomicU64,
    /// Progress on this pair alone.
    ///
    /// A per-CPU queue layout wants a per-queue wake: a waiter whose
    /// socket lives on shard 3 has nothing to learn from pair 0's
    /// completion, and waking it costs the same as waking the task that
    /// did make progress.
    interrupts: Notify,
    rx_state: AsyncMutex<NetRxState<T>>,
    rx_returned: Arc<RxReturnedSlots>,
    rx_slots: Box<[Arc<RxBufferSlot>]>,
    /// Present only when a frame can span more than one receive slot.
    rx_reassembly: Option<RxReassemblyPool>,
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
    /// Progress that belongs to no single pair: a configuration change,
    /// and the control queue's own completions.
    interrupts: Notify,
    features: NegotiatedFeatures,
    mac_address: [u8; 6],
    max_frame_len: usize,
    /// Largest frame the device may deliver. Equal to `max_frame_len`
    /// unless receive segmentation offload is negotiated, in which case
    /// the device may coalesce several wire segments into one frame.
    max_receive_frame_len: usize,
    header_len: usize,
    /// VIRTIO_NET_F_CSUM was negotiated: frames may carry partial
    /// checksums for the device to finish.
    tx_checksum_negotiated: bool,
    /// VIRTIO_NET_F_HOST_TSO4/6 negotiated for the given families: the
    /// driver may submit oversized TCP segments for device segmentation.
    tso_v4_negotiated: bool,
    tso_v6_negotiated: bool,
    /// VIRTIO_NET_F_HOST_ECN was negotiated: an oversized frame may
    /// carry `VIRTIO_NET_HDR_GSO_ECN`.
    tso_ecn_negotiated: bool,
    /// VIRTIO_NET_F_GUEST_CSUM was negotiated: the device reports per
    /// frame whether it validated the transport checksum or left it
    /// partial.
    guest_checksum_negotiated: bool,
    /// VIRTIO_NET_F_MRG_RXBUF was negotiated: a received frame may span
    /// several receive buffers and the header's `num_buffers` says how
    /// many.
    mergeable_rx_buffers: bool,
    /// VIRTIO_NET_F_GUEST_TSO4/6 and _GUEST_UFO: the device may deliver
    /// frames it coalesced out of several wire segments.
    guest_tso_v4_negotiated: bool,
    guest_tso_v6_negotiated: bool,
    guest_ufo_negotiated: bool,
    /// VIRTIO_NET_F_STATUS was negotiated: the configuration space
    /// carries a link status the driver re-reads on configuration
    /// change.
    status_negotiated: bool,
    /// Last link state read out of the configuration space. Devices
    /// without VIRTIO_NET_F_STATUS are always up.
    link_up: AtomicBool,
    /// The receive-side scaling limits the device reported, present
    /// only when `VIRTIO_NET_F_RSS` was negotiated *and* the device can
    /// hold the table and key this driver's demux is defined in terms
    /// of. A device that cannot is left unsteered rather than
    /// half-programmed: a wrong table would put a flow on a queue whose
    /// processor does not own the socket, which is worse than the hop.
    rss: Option<RssLimits>,
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

pub use helios_netstack::RxFrame;

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

    /// The receive buffer as a raw slice.
    ///
    /// Which of the driver and the device may write to a receive
    /// buffer is decided by the receive ring, not by the borrow
    /// checker: a slot belongs to the driver from the moment the used
    /// ring returns it until the driver makes it available again, and
    /// to the device in between. The slot is shared as `Arc` for
    /// exactly that reason, so there is never a `&mut RxBufferSlot` to
    /// hand out and the buffer is handed over raw instead. Callers
    /// build the mutable reference themselves and write down which
    /// half of that alternation they are in.
    fn buffer_ptr(&self) -> *mut [u8] {
        // SAFETY: `UnsafeCell::get` yields a pointer to the owned
        // `Box`, and `&raw mut` projects through it to the heap slice
        // without ever materialising a reference to the box itself.
        unsafe { &raw mut *(*self.buffer.get()) }
    }
}

/// A virtio feature word, printed the way the specification numbers
/// its bits.
struct FeatureWord(u64);

impl core::fmt::Debug for FeatureWord {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "{:#018x}", self.0)
    }
}

impl<T: VirtioTransport> VirtioNetDevice<T> {
    pub fn new(transport: T) -> IoResult<Self> {
        if transport.device_type() != DeviceType::Network {
            return Err(IoError::Unsupported);
        }

        // The device's own offer, kept so bring-up can report what was
        // on the table next to what the driver took.
        let offered_features = core::cell::Cell::new(0_u64);
        let features = negotiate_with(&transport, |offered| {
            offered_features.set(offered);
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
                let families = offered & (NET_FEATURE_HOST_TSO4 | NET_FEATURE_HOST_TSO6);
                families | (offered & NET_FEATURE_HOST_ECN)
            } else {
                0
            };
            // Receive offloads. GUEST_CSUM stands on its own: it lets the
            // device tell the stack, per frame, that the transport
            // checksum is already accounted for. The receive
            // segmentation bits all depend on it, and additionally need
            // buffers that can hold a coalesced frame — either 64 KiB
            // each or mergeable buffers (virtio 1.2 §5.1.6.3.1). helios
            // pairs them with MRG_RXBUF and page-granular buffers, so
            // they are asked for only when the device offers merging
            // too. ECN is a modifier on a segmentation type, so it is
            // only meaningful once one of the TSO families is in.
            let guest_csum_mask = offered & NET_FEATURE_GUEST_CSUM;
            let mrg_mask = offered & NET_FEATURE_MRG_RXBUF;
            let guest_gso_mask = if guest_csum_mask != 0 && mrg_mask != 0 {
                let families = offered
                    & (NET_FEATURE_GUEST_TSO4 | NET_FEATURE_GUEST_TSO6 | NET_FEATURE_GUEST_UFO);
                let tcp_families = families & (NET_FEATURE_GUEST_TSO4 | NET_FEATURE_GUEST_TSO6);
                let ecn_mask = if tcp_families != 0 {
                    offered & NET_FEATURE_GUEST_ECN
                } else {
                    0
                };
                families | ecn_mask
            } else {
                0
            };
            // Receive-side scaling is what makes the per-CPU queue
            // layout pay, but only together with multiqueue: on a
            // single-pair device there is nothing to steer to. The
            // hash report rides along so the driver reads the number
            // the device already computed instead of computing it
            // again; both are programmed over the control queue.
            let rss_mask = if mq_supported && offered & NET_FEATURE_RSS != 0 {
                NET_FEATURE_RSS | (offered & NET_FEATURE_HASH_REPORT)
            } else {
                0
            };
            // Notification coalescing is programmed over the control
            // queue, so it is only worth asking for when the control
            // queue itself is in. The per-virtqueue form is a strict
            // refinement of the device-wide one and the spec has the
            // driver take both.
            let coalescing_mask = if offered & NET_FEATURE_CTRL_VQ != 0 {
                let device_wide = offered & NET_FEATURE_NOTF_COAL;
                let per_queue = if device_wide != 0 {
                    offered & NET_FEATURE_VQ_NOTF_COAL
                } else {
                    0
                };
                let ctrl_vq = if device_wide != 0 {
                    NET_FEATURE_CTRL_VQ
                } else {
                    0
                };
                device_wide | per_queue | ctrl_vq
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
                | guest_csum_mask
                | mrg_mask
                | guest_gso_mask
                | coalescing_mask
                | rss_mask
        })?;

        let mac_address = read_mac_address(&transport);
        let ip_mtu = read_mtu(&transport, features);
        let max_frame_len = ip_mtu
            .checked_add(ETH_HEADER_LEN)
            .ok_or(IoError::DeviceFault)?;
        // A device that cannot hold the table and key this driver's
        // demux is defined in terms of is left unsteered rather than
        // half-programmed.
        let rss = features
            .device(NET_FEATURE_RSS)
            .then(|| read_rss_limits(&transport))
            .filter(|limits| limits.accepts_our_table());
        if features.device(NET_FEATURE_RSS) && rss.is_none() {
            tracing::warn!(
                "virtio-net offered RSS but cannot hold a {RSS_INDIRECTION_ENTRIES}-entry table with a {RSS_KEY_BYTES}-byte key over the four-tuple hash types; receives stay unsteered"
            );
        }
        // `VIRTIO_NET_F_HASH_REPORT` lengthens the header in both
        // directions, so it is one number every buffer size derives
        // from rather than a per-path branch.
        let header_len = if features.device(NET_FEATURE_HASH_REPORT) {
            HASH_REPORT_HEADER_BYTES
        } else {
            size_of::<VirtioNetHeader>()
        };
        let mergeable_rx_buffers = features.device(NET_FEATURE_MRG_RXBUF);
        let guest_tso_v4_negotiated = features.device(NET_FEATURE_GUEST_TSO4);
        let guest_tso_v6_negotiated = features.device(NET_FEATURE_GUEST_TSO6);
        let guest_ufo_negotiated = features.device(NET_FEATURE_GUEST_UFO);
        let large_receive =
            guest_tso_v4_negotiated || guest_tso_v6_negotiated || guest_ufo_negotiated;
        let max_receive_frame_len = if large_receive {
            MAX_LARGE_RECEIVE_FRAME_BYTES
        } else {
            max_frame_len
        };
        // Receive slots are page-granular. Mergeable buffers make one
        // page the whole slot: a frame larger than a page arrives as a
        // chain. Without merging the device has no way to split a frame,
        // so the slot has to hold the largest one whole.
        let rx_buffer_len = if mergeable_rx_buffers {
            RX_PAGE_BYTES
        } else {
            header_len
                .checked_add(max_receive_frame_len)
                .ok_or(IoError::DeviceFault)?
                .next_multiple_of(RX_PAGE_BYTES)
        };
        let tx_buffer_len = header_len
            .checked_add(max_frame_len)
            .ok_or(IoError::DeviceFault)?;

        // The most pairs worth bringing up. `max_virtqueue_pairs` is a
        // ceiling the device advertises, not a promise that every pair
        // below it is present and usable: a device whose host packet
        // path offers fewer queues than the configuration space
        // advertises reports queue size zero for the pairs it does not
        // have, and the spec has the driver use the ones it finds and
        // tell the device the number over the control queue.
        let advertised_pairs = if features.device(NET_FEATURE_MQ) {
            read_max_virtqueue_pairs(&transport).clamp(1, NET_MAX_QUEUE_PAIRS)
        } else {
            1
        };

        // One record, before the first queue is programmed, holding
        // everything a bring-up failure is diagnosed from: what the
        // device offered, what the driver took, the queue layout the
        // class configuration implies, the count the transport itself
        // reports, and the size the device gives at every index that
        // layout covers. A device whose `num_queues` disagrees with
        // `2 * max_virtqueue_pairs + 1` is about to be programmed for
        // queues it does not have, and nothing else in the boot log
        // would say so.
        let probed_queues = usize::from(advertised_pairs) * 2 + 1;
        let mut queue_sizes = [0_u16; NET_MAX_QUEUE_PAIRS as usize * 2 + 1];
        for (index, size) in queue_sizes[..probed_queues].iter_mut().enumerate() {
            *size = transport.queue_max_size(index as u16);
        }
        tracing::info!(
            offered = ?FeatureWord(offered_features.get()),
            accepted = ?FeatureWord(features.bits()),
            max_virtqueue_pairs = advertised_pairs,
            presented_queues = ?transport.presented_queue_count(),
            queue_sizes = ?&queue_sizes[..probed_queues],
            "virtio-net bring-up"
        );

        let mut queue_pairs: Vec<NetQueuePair<T>> =
            Vec::with_capacity(usize::from(advertised_pairs));
        for pair_idx in 0..advertised_pairs {
            let rx_queue_index = rx_queue_index(pair_idx);
            let tx_queue_index = tx_queue_index(pair_idx);
            let rx_queue_size = transport.queue_max_size(rx_queue_index).min(NET_QUEUE_SIZE);
            let tx_queue_size = transport.queue_max_size(tx_queue_index).min(NET_QUEUE_SIZE);
            if rx_queue_size == 0 || tx_queue_size == 0 {
                // Multiqueue is optional in both directions. Refusing the
                // whole device here would leave a working single-pair NIC
                // unusable because its ceiling was optimistic.
                if pair_idx == 0 {
                    return Err(IoError::InvalidDeviceConfig(
                        "virtio-net function presents no receive or transmit queue",
                    ));
                }
                tracing::warn!(
                    advertised_pairs,
                    usable_pairs = pair_idx,
                    "virtio-net advertised more queue pairs than it presents; \
                     bringing up the pairs that exist"
                );
                break;
            }
            if !rx_queue_size.is_power_of_two() || !tx_queue_size.is_power_of_two() {
                // A ring length that is not a power of two has no valid
                // wrap, so this is the device model being wrong rather
                // than a capability the driver can do without.
                return Err(IoError::InvalidDeviceConfig(
                    "virtio-net queue size is not a power of two",
                ));
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
            let mut rx_post_position = vec![0_u64; usize::from(rx_queue_size)].into_boxed_slice();
            let mut rx_next_post_position = 0_u64;
            for (slot_index, slot) in rx_slots.iter().enumerate() {
                // SAFETY: bring-up owns every slot; none has been made
                // available to the device yet.
                let buffer = unsafe { &mut *slot.buffer_ptr() };
                let token = rx_queue.submit_output_deferred(&transport, buffer)?;
                rx_slot_for_token[usize::from(token)] = slot.slot;
                rx_post_position[usize::from(token)] = rx_next_post_position;
                rx_next_post_position += 1;
                assert!(
                    !rx_in_device.get(slot_index),
                    "virtio net RX slot was posted twice during initialization"
                );
                rx_in_device.set(slot_index);
            }
            rx_queue.publish();
            // A frame spans more than one slot only where merging is
            // negotiated, so that is exactly where the pool exists. It
            // holds as many frame buffers as the ring's byte capacity
            // can produce — never the narrower resource — up to a cap
            // that keeps the pool from dwarfing the ring when frames are
            // small.
            let rx_reassembly = if mergeable_rx_buffers {
                let ring_bytes = rx_buffer_len
                    .checked_mul(rx_buffer_count)
                    .ok_or(IoError::DeviceFault)?;
                let count = ring_bytes
                    .div_ceil(max_receive_frame_len)
                    .clamp(2, MAX_RX_REASSEMBLY_BUFFERS);
                Some(RxReassemblyPool::new(count, max_receive_frame_len)?)
            } else {
                None
            };
            let tx_buffer_count = usize::from(tx_queue_size);
            let tx_buffers = vec![
                0_u8;
                tx_buffer_len
                    .checked_mul(tx_buffer_count)
                    .ok_or(IoError::DeviceFault)?
            ]
            .into_boxed_slice();
            let tx_in_flight = DescriptorBitSet::new(tx_buffer_count);
            let tx_payloads = vec![None; tx_buffer_count].into_boxed_slice();

            queue_pairs.push(NetQueuePair {
                interrupt_count: AtomicU64::new(0),
                interrupts: Notify::new(),
                rx_state: AsyncMutex::new(NetRxState {
                    rx_queue,
                    rx_in_device,
                    rx_slot_for_token,
                    rx_post_position,
                    rx_next_post_position,
                }),
                rx_returned,
                rx_slots: rx_slots.into_boxed_slice(),
                rx_reassembly,
                tx_state: SpinMutex::new(NetTxState {
                    tx_queue,
                    tx_buffers,
                    tx_buffer_len,
                    tx_in_flight,
                    tx_payloads,
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
            let ctrl_index = control_queue_index(device_max);
            let ctrl_size = transport.queue_max_size(ctrl_index).min(NET_QUEUE_SIZE);
            // The driver has already told the device it accepted
            // `CTRL_VQ`, so a control queue that cannot be brought up is
            // a device that disagrees with its own feature bits. Leaving
            // the slot empty here used to defer the failure to the first
            // command, where it surfaced as a bare "operation is not
            // supported" with nothing naming the queue (issue #91).
            if ctrl_size == 0 {
                return Err(IoError::InvalidDeviceConfig(
                    "virtio-net offered the control queue but presents no queue at its index",
                ));
            }
            if !ctrl_size.is_power_of_two() {
                return Err(IoError::InvalidDeviceConfig(
                    "virtio-net control queue size is not a power of two",
                ));
            }
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
            max_receive_frame_len,
            header_len,
            tx_checksum_negotiated: features.device(NET_FEATURE_CSUM),
            tso_v4_negotiated: features.device(NET_FEATURE_HOST_TSO4),
            tso_v6_negotiated: features.device(NET_FEATURE_HOST_TSO6),
            tso_ecn_negotiated: features.device(NET_FEATURE_HOST_ECN),
            guest_checksum_negotiated: features.device(NET_FEATURE_GUEST_CSUM),
            mergeable_rx_buffers,
            guest_tso_v4_negotiated,
            guest_tso_v6_negotiated,
            guest_ufo_negotiated,
            status_negotiated: features.device(NET_FEATURE_STATUS),
            link_up: AtomicBool::new(true),
            rss,
        };
        device
            .link_up
            .store(read_link_up(&device.transport, features), Ordering::Relaxed);

        // Activate every queue pair on the device side. The device
        // ships with a single pair active by default; without this
        // command extra RX queues we just allocated would not
        // receive traffic. A steering device is told the same count
        // inside its RSS configuration instead, which is the command
        // that also hands it the table and the key.
        if device.steers_receives() {
            device.send_rss_config()?;
        } else if features.device(NET_FEATURE_MQ) && device.queue_pair_count() > 1 {
            device.send_set_vq_pairs(device.queue_pair_count())?;
        }

        // Coalescing is programmed after the pairs are active, because
        // the per-virtqueue form addresses queues by index and a pair
        // the device has not been told to activate has no budget to set.
        if features.device(NET_FEATURE_NOTF_COAL) {
            device.send_notification_coalescing()?;
        }

        // The negotiated set is the only record of what the host packet
        // path was actually able to offer: a slirp-backed device
        // negotiates neither multiqueue nor segmentation offload, so a
        // measurement taken against it exercises none of the driver's
        // offload paths. Logging it at bring-up puts the feature set of
        // every run into the boot log, where a benchmark lane can record
        // which backend it really measured. The ring bits are already on
        // the `virtio features negotiated` line this device emitted, so
        // only the device-class facts are repeated here.
        tracing::info!(
            queue_pairs = device.queue_pair_count(),
            csum = device.tx_checksum_negotiated,
            host_tso4 = device.tso_v4_negotiated,
            host_tso6 = device.tso_v6_negotiated,
            host_ecn = device.tso_ecn_negotiated,
            guest_csum = device.guest_checksum_negotiated,
            guest_tso4 = device.guest_tso_v4_negotiated,
            guest_tso6 = device.guest_tso_v6_negotiated,
            guest_ecn = features.device(NET_FEATURE_GUEST_ECN),
            guest_ufo = device.guest_ufo_negotiated,
            mrg_rxbuf = device.mergeable_rx_buffers,
            mq = features.device(NET_FEATURE_MQ),
            ctrl_vq = features.device(NET_FEATURE_CTRL_VQ),
            rss = device.steers_receives(),
            hash_report = features.device(NET_FEATURE_HASH_REPORT),
            notf_coal = features.device(NET_FEATURE_NOTF_COAL),
            vq_notf_coal = features.device(NET_FEATURE_VQ_NOTF_COAL),
            notf_coal_max_packets = NET_NOTF_COAL_MAX_PACKETS,
            notf_coal_max_usecs = NET_NOTF_COAL_MAX_USECS,
            status = device.status_negotiated,
            link_up = device.link_up.load(Ordering::Relaxed),
            max_frame_len = device.max_frame_len,
            max_receive_frame_len = device.max_receive_frame_len,
            rx_buffer_len = device.rx_buffer_len,
            "virtio-net online"
        );
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
    pub fn queue_topology(&self) -> QueueTopology {
        QueueTopology {
            rx_queues: self.queue_pair_count(),
            tx_queues: self.queue_pair_count(),
            tx_queue_depth: self.tx_queue_depth(),
            rss: self.steers_receives(),
        }
    }

    /// Whether the device steers received frames across its queues by
    /// flow hash, rather than delivering everything on queue zero.
    pub fn steers_receives(&self) -> bool {
        self.rss.is_some()
    }

    /// The capability set the network stack specializes its data paths
    /// from, as this device negotiated it.
    ///
    /// Every backend that owns a virtio-net device answers with this:
    /// what the device can do follows from the feature handshake, not
    /// from which machine the driver was instantiated on.
    pub fn interface_capabilities(&self) -> InterfaceCapabilities {
        InterfaceCapabilities {
            max_frame_len: self.max_frame_len,
            checksum: ChecksumOffload {
                // virtio-net reports on the transport checksum only. The
                // IPv4 header checksum is never covered by
                // VIRTIO_NET_F_GUEST_CSUM, so the stack keeps verifying
                // it in software.
                rx_ipv4: false,
                rx_tcp: self.guest_checksum_negotiated,
                rx_udp: self.guest_checksum_negotiated,
                tx_ipv4: false,
                tx_tcp: self.tx_checksum_negotiated,
                tx_udp: self.tx_checksum_negotiated,
            },
            segmentation: SegmentationOffload {
                tx_tcp_ipv4: self.tso_v4_negotiated,
                tx_tcp_ipv6: self.tso_v6_negotiated,
                rx_tcp_ipv4: self.guest_tso_v4_negotiated,
                rx_tcp_ipv6: self.guest_tso_v6_negotiated,
                max_tx_frame_bytes: self.max_transmit_frame_len(),
                max_rx_frame_bytes: self.max_receive_frame_len,
                tx_tcp_ecn: self.tso_ecn_negotiated,
            },
            queues: self.queue_topology(),
            events: EventDeliveryCapabilities {
                polling: true,
                interrupts: true,
                adaptive_moderation: false,
                rx_coalescing: self.notification_coalescing().enabled(),
                tx_coalescing: self.notification_coalescing().enabled(),
                rx_poll_budget: DEFAULT_POLL_BUDGET,
                tx_completion_budget: DEFAULT_POLL_BUDGET,
            },
            direct_tx_dma: true,
            direct_rx_dma: false,
        }
    }

    pub fn queue_pair_count(&self) -> usize {
        self.queue_pairs.len()
    }

    /// The feature set this device negotiated.
    pub fn features(&self) -> NegotiatedFeatures {
        self.features
    }

    /// Issues one control-queue command and waits for the device's ack.
    ///
    /// Every command the driver sends has the same shape — a one-byte
    /// class, a one-byte command id, a fixed payload, and a single ack
    /// byte back — so the submission lives here once and each caller
    /// only spells out its own payload.
    fn send_control_command(&self, class: u8, command: u8, payload: &[u8]) -> IoResult<()> {
        let Some(control) = self.control.as_ref() else {
            return Err(IoError::Unsupported);
        };
        let mut state = control.lock();
        // Destructure under `&mut` so the split borrows of the
        // queue, cmd buffer, and ack buffer are independent for the
        // duration of the submission.
        let NetControlState {
            queue,
            cmd_buffer,
            ack_buffer,
        } = &mut *state;
        let len = 2 + payload.len();
        assert!(
            len <= cmd_buffer.len(),
            "virtio-net control command {class}/{command} needs {len} bytes but the scratch buffer holds {}",
            cmd_buffer.len()
        );
        cmd_buffer[0] = class;
        cmd_buffer[1] = command;
        cmd_buffer[2..len].copy_from_slice(payload);
        ack_buffer[0] = CTRL_ACK_PENDING;
        let cmd_slice: &[u8] = &cmd_buffer[..len];
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

    fn send_set_vq_pairs(&self, pairs: usize) -> IoResult<()> {
        let pairs = u16::try_from(pairs).map_err(|_| IoError::DeviceFault)?;
        let mut payload = [0_u8; CTRL_MQ_PAIRS_PAYLOAD_BYTES];
        payload.copy_from_slice(&pairs.to_le_bytes());
        self.send_control_command(CTRL_CLASS_MQ, CTRL_CMD_MQ_VQ_PAIRS_SET, &payload)
    }

    /// Programs notification coalescing for every queue this device
    /// brought up.
    ///
    /// A per-CPU queue layout wants the per-virtqueue form: each pair is
    /// driven by its own processor at its own rate, and one device-wide
    /// setting would make an idle pair pay the busy pair's delay. When
    /// the device only offers the device-wide form the same budget is
    /// programmed once for receive and once for transmit, which is the
    /// same policy at a coarser grain.
    fn send_notification_coalescing(&self) -> IoResult<()> {
        let budget = CoalescingBudget {
            max_packets: NET_NOTF_COAL_MAX_PACKETS,
            max_usecs: NET_NOTF_COAL_MAX_USECS,
        };
        if self.features.device(NET_FEATURE_VQ_NOTF_COAL) {
            for pair_idx in 0..self.queue_pair_count() {
                let pair_idx = u16::try_from(pair_idx).map_err(|_| IoError::DeviceFault)?;
                for queue in [rx_queue_index(pair_idx), tx_queue_index(pair_idx)] {
                    self.send_control_command(
                        CTRL_CLASS_NOTF_COAL,
                        CTRL_CMD_NOTF_COAL_VQ_SET,
                        &budget.encode_for_queue(queue),
                    )?;
                }
            }
            return Ok(());
        }
        self.send_control_command(
            CTRL_CLASS_NOTF_COAL,
            CTRL_CMD_NOTF_COAL_RX_SET,
            &budget.encode(),
        )?;
        self.send_control_command(
            CTRL_CLASS_NOTF_COAL,
            CTRL_CMD_NOTF_COAL_TX_SET,
            &budget.encode(),
        )
    }

    /// Hands the device the indirection table and the key the driver's
    /// own demux is defined in terms of.
    ///
    /// The table is not stored: entry `i` is `i % pairs`, which is
    /// exactly what [`rss_indirection_entry`] computes when the receive
    /// path has to reach the same answer in software. Programming it
    /// this way is the whole point — a frame the device steers and a
    /// frame the driver hashes land on the same shard.
    fn send_rss_config(&self) -> IoResult<()> {
        let pairs = u16::try_from(self.queue_pair_count()).map_err(|_| IoError::DeviceFault)?;
        let mut payload = [0_u8; CTRL_RSS_CONFIG_PAYLOAD_BYTES];
        let mut offset = 0;
        let mut push = |bytes: &[u8]| {
            payload[offset..offset + bytes.len()].copy_from_slice(bytes);
            offset += bytes.len();
        };
        push(&NET_RSS_HASH_TYPES.to_le_bytes());
        // The mask is the table length minus one, which is why the
        // length has to be a power of two.
        push(&((RSS_INDIRECTION_ENTRIES - 1) as u16).to_le_bytes());
        // A frame the device cannot classify goes here — the same queue
        // the driver's own rule sends a frame with no flow to.
        push(&0_u16.to_le_bytes());
        for slot in 0..RSS_INDIRECTION_ENTRIES {
            let queue = rss_indirection_entry(slot, usize::from(pairs));
            push(&(queue as u16).to_le_bytes());
        }
        push(&pairs.to_le_bytes());
        push(&[RSS_KEY_BYTES as u8]);
        push(&STANDARD_RSS_KEY);
        assert_eq!(
            offset, CTRL_RSS_CONFIG_PAYLOAD_BYTES,
            "the RSS configuration payload was not filled exactly"
        );
        self.send_control_command(CTRL_CLASS_MQ, CTRL_CMD_MQ_RSS_CONFIG, &payload)
    }

    /// Whether the device holds notifications back for a batch or a
    /// delay, and at what grain.
    pub fn notification_coalescing(&self) -> NotificationCoalescing {
        NotificationCoalescing {
            device_wide: self.features.device(NET_FEATURE_NOTF_COAL),
            per_queue: self.features.device(NET_FEATURE_VQ_NOTF_COAL),
        }
    }

    /// Handles an interrupt that names no queue, which is what every
    /// transport with a single interrupt line delivers.
    ///
    /// Returns the queue pairs that actually have completions waiting,
    /// so the backend — which owns the `Cpu` and therefore the IPI —
    /// can wake exactly those pairs' processors instead of every parked
    /// one. A pair another processor currently holds is left out: that
    /// processor is draining it and needs no wake.
    pub fn handle_interrupt(&self) -> QueuePairProgress {
        let status = self.transport.ack_interrupt();
        if status.config_change {
            self.refresh_link_state();
        }
        let mut progress = QueuePairProgress::none();
        for (pair_idx, pair) in self.queue_pairs.iter().enumerate() {
            if !pair.has_pending_completions() {
                continue;
            }
            progress.record(pair_idx);
            pair.raise_interrupt();
        }
        // A configuration change belongs to no pair, and neither does a
        // control-queue completion, so the device-wide notification
        // still exists for the waiters that watch those.
        self.interrupts.notify_all();
        progress
    }

    /// Handles an interrupt raised by one queue pair's own vector.
    ///
    /// A transport with per-queue interrupts already knows which pair
    /// made progress and has already delivered the message to that
    /// pair's processor, so there is nothing to scan and nobody to wake.
    pub fn handle_interrupt_on(&self, pair_idx: usize) {
        let pair_idx = self.normalize_pair_idx(pair_idx);
        self.transport.ack_interrupt();
        self.queue_pairs[pair_idx].raise_interrupt();
    }

    /// Interrupts raised for one queue pair since boot.
    pub fn queue_interrupts(&self, pair_idx: usize) -> u64 {
        self.queue_pairs
            .get(self.normalize_pair_idx(pair_idx))
            .map_or(0, |pair| pair.interrupt_count.load(Ordering::Relaxed))
    }

    /// Handles an interrupt raised by the configuration-change vector.
    pub fn handle_configuration_interrupt(&self) {
        self.transport.ack_interrupt();
        self.refresh_link_state();
        self.interrupts.notify_all();
    }

    /// Carrier as of the last configuration-change interrupt.
    pub fn link_state(&self) -> LinkState {
        if self.link_up.load(Ordering::Acquire) {
            LinkState::Up
        } else {
            LinkState::Down
        }
    }

    /// Re-reads the link status out of the configuration space and
    /// publishes it. Logged only on a transition: a configuration change
    /// can be raised for anything the device keeps there.
    fn refresh_link_state(&self) -> LinkState {
        let up = read_link_up(&self.transport, self.features);
        if self.link_up.swap(up, Ordering::AcqRel) != up {
            tracing::info!(link_up = up, "virtio-net link state changed");
        }
        if up { LinkState::Up } else { LinkState::Down }
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

    /// Whether the device accepts `VIRTIO_NET_HDR_GSO_ECN` on an
    /// oversized frame (VIRTIO_NET_F_HOST_ECN).
    pub fn tso_ecn_negotiated(&self) -> bool {
        self.tso_ecn_negotiated
    }

    /// A segmented frame may only be submitted for a family the device
    /// agreed to split, and may only be marked ECN-capable when the
    /// device agreed to that too.
    fn assert_segmentation_negotiated(&self, gso: TxGsoMeta) {
        assert!(
            self.tso_negotiated(gso.ipv6),
            "segmented frame submitted without negotiated VIRTIO_NET_F_HOST_TSO"
        );
        assert!(
            !gso.ecn || self.tso_ecn_negotiated,
            "ECN-capable segmented frame submitted without negotiated VIRTIO_NET_F_HOST_ECN"
        );
    }

    /// Largest frame the driver hands the device in one submission.
    ///
    /// Equal to [`Self::max_frame_len`] until a TCP segmentation family
    /// is negotiated, at which point the driver may submit one
    /// oversized frame for the device to split. The ceiling is the
    /// widest packet an IP length field can describe plus its Ethernet
    /// header, matching the receive-side bound in virtio 1.2
    /// §5.1.6.3.1.
    pub fn max_transmit_frame_len(&self) -> usize {
        if self.tso_v4_negotiated || self.tso_v6_negotiated {
            MAX_SEGMENTED_TRANSMIT_FRAME_BYTES
        } else {
            self.max_frame_len
        }
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac_address
    }

    pub fn max_frame_len(&self) -> usize {
        self.max_frame_len
    }

    /// Largest frame the device may deliver, which exceeds
    /// [`Self::max_frame_len`] once receive segmentation offload is
    /// negotiated.
    pub fn max_receive_frame_len(&self) -> usize {
        self.max_receive_frame_len
    }

    /// Whether the device reports per received frame what it did about
    /// the transport checksum (VIRTIO_NET_F_GUEST_CSUM).
    pub fn guest_checksum_negotiated(&self) -> bool {
        self.guest_checksum_negotiated
    }

    /// Whether the device may coalesce received segments of the given
    /// address family into one frame (VIRTIO_NET_F_GUEST_TSO4/6).
    pub fn large_receive_negotiated(&self, ipv6: bool) -> bool {
        if ipv6 {
            self.guest_tso_v6_negotiated
        } else {
            self.guest_tso_v4_negotiated
        }
    }

    /// Whether the device may spread one received frame across several
    /// receive buffers (VIRTIO_NET_F_MRG_RXBUF).
    pub fn mergeable_rx_buffers(&self) -> bool {
        self.mergeable_rx_buffers
    }

    pub async fn try_receive_into(&self, output: &mut [u8]) -> IoResult<Option<usize>> {
        const PAIR: usize = 0;
        let mut state = self.queue_pairs[PAIR].rx_state.lock().await;
        self.drain_returned_rx_buffers(PAIR, &mut state)?;
        let Some(frame) = self.receive_next_frame(PAIR, &mut state)? else {
            return Ok(None);
        };
        let frame_len = frame.bytes.len();
        if frame_len > output.len() {
            // Dropping the frame returns its slot; the next drain of
            // this pair reposts it.
            return Err(IoError::OutOfBounds);
        }
        output[..frame_len].copy_from_slice(frame.bytes.as_ref());
        drop(frame);
        self.drain_returned_rx_buffers(PAIR, &mut state)?;
        Ok(Some(frame_len))
    }

    pub async fn try_receive_frame(&self) -> IoResult<Option<RxFrame>> {
        const PAIR: usize = 0;
        let mut state = self.queue_pairs[PAIR].rx_state.lock().await;
        self.drain_returned_rx_buffers(PAIR, &mut state)?;
        self.receive_next_frame(PAIR, &mut state)
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
            let Some(frame) = self.receive_next_frame(pair_idx, state)? else {
                break;
            };
            let slot = &mut frames[received];
            assert!(slot.is_none(), "virtio net RX batch slot was not empty");
            *slot = Some(frame);
            received += 1;
        }
        Ok(received)
    }

    /// Takes the next completed frame off a receive ring, assembling a
    /// mergeable chain when the device split one across buffers.
    ///
    /// `Ok(None)` means the ring has nothing ready — or, when a chain
    /// would need a reassembly buffer and none is free, that the caller
    /// should come back after releasing the frames it already holds. The
    /// used entries stay in the ring either way, so nothing is dropped.
    fn receive_next_frame(
        &self,
        pair_idx: usize,
        state: &mut NetRxState<T>,
    ) -> IoResult<Option<RxFrame>> {
        let reassembly = self.queue_pairs[pair_idx].rx_reassembly.as_ref();
        if reassembly.is_some_and(|pool| !pool.has_free()) {
            return Ok(None);
        }
        let Some((token, used_len)) = state.rx_queue.pop_used_with_len() else {
            return Ok(None);
        };
        let (slot_index, position) = Self::complete_rx_slot(state, token);
        let used_len = used_len as usize;
        if used_len < self.header_len || used_len > self.rx_buffer_len {
            self.repost_rx_buffer(pair_idx, state, slot_index)?;
            return Err(IoError::DeviceFault);
        }
        let slot = &self.queue_pairs[pair_idx].rx_slots[usize::from(slot_index)];
        let header = RxHeader::parse(&slot.buffer()[..self.header_len], self.header_len);
        let offload = match self.rx_offload(header) {
            Ok(offload) => offload,
            Err(error) => {
                self.repost_rx_buffer(pair_idx, state, slot_index)?;
                return Err(error);
            }
        };
        let buffers = if self.mergeable_rx_buffers {
            usize::from(header.num_buffers)
        } else {
            1
        };
        if buffers == 0 {
            self.repost_rx_buffer(pair_idx, state, slot_index)?;
            return Err(IoError::DeviceFault);
        }
        if buffers == 1 {
            return Ok(Some(RxFrame::with_offload(
                self.rx_bytes_from_slot(pair_idx, slot_index, self.header_len..used_len),
                offload,
            )));
        }
        let pool = reassembly.ok_or(IoError::DeviceFault)?;
        let target = pool.take().ok_or(IoError::DeviceFault)?;
        let head = RxChainHead {
            slot: slot_index,
            position,
            used_len,
        };
        let assembled = self.assemble_rx_chain(pair_idx, state, head, buffers, &target);
        // Every buffer of the chain has to go back to the device whether
        // the assembly succeeded or not; the head slot is released by
        // the assembly itself.
        let assembled = assembled?;
        Ok(Some(RxFrame::with_offload(
            Bytes::from_owner(RxFrameOwner {
                slot: target,
                range: 0..assembled,
            }),
            offload,
        )))
    }

    /// Copies a mergeable receive chain into `target` and returns the
    /// assembled frame length.
    ///
    /// The device fills a chain out of consecutively available buffers
    /// and publishes the whole group before the driver can see any of
    /// it, so the tail buffers are already in the used ring and carry
    /// the available-ring positions that follow the head's. A device
    /// that breaks either half of that has handed over a frame this
    /// driver cannot reconstruct, and the chain is refused rather than
    /// stitched together out of unrelated buffers.
    fn assemble_rx_chain(
        &self,
        pair_idx: usize,
        state: &mut NetRxState<T>,
        head: RxChainHead,
        buffers: usize,
        target: &Arc<RxBufferSlot>,
    ) -> IoResult<usize> {
        let head_slot = &self.queue_pairs[pair_idx].rx_slots[usize::from(head.slot)];
        let mut assembled = head.used_len - self.header_len;
        // SAFETY: `target` was taken out of the reassembly pool's free
        // list by this drain and is not referenced by any live frame,
        // and the caller holds the receive state that serialises drains.
        let assembly = unsafe { &mut *target.buffer_ptr() };
        assembly[..assembled].copy_from_slice(&head_slot.buffer()[self.header_len..head.used_len]);
        self.repost_rx_buffer_deferred(pair_idx, state, head.slot)?;
        let mut expected_position = head.position;
        for _ in 1..buffers {
            expected_position += 1;
            let Some((token, used_len)) = state.rx_queue.pop_used_with_len() else {
                state.rx_queue.publish();
                state.rx_queue.notify(&self.transport);
                return Err(IoError::DeviceFault);
            };
            let (slot_index, position) = Self::complete_rx_slot(state, token);
            let used_len = used_len as usize;
            let slot = &self.queue_pairs[pair_idx].rx_slots[usize::from(slot_index)];
            let fits = used_len <= self.rx_buffer_len
                && assembled
                    .checked_add(used_len)
                    .is_some_and(|end| end <= self.max_receive_frame_len);
            if position != expected_position || !fits {
                self.repost_rx_buffer(pair_idx, state, slot_index)?;
                return Err(IoError::DeviceFault);
            }
            // Only the head buffer of a mergeable chain carries the
            // virtio-net header; the rest are frame bytes end to end.
            assembly[assembled..assembled + used_len].copy_from_slice(&slot.buffer()[..used_len]);
            assembled += used_len;
            self.repost_rx_buffer_deferred(pair_idx, state, slot_index)?;
        }
        state.rx_queue.publish();
        state.rx_queue.notify(&self.transport);
        Ok(assembled)
    }

    /// Translates the device's per-frame receive header into the
    /// metadata the network stack decides checksum trust from.
    fn rx_offload(&self, header: RxHeader) -> IoResult<RxFrameOffload> {
        let needs_csum = header.flags & VIRTIO_NET_HDR_F_NEEDS_CSUM != 0;
        let data_valid = header.flags & VIRTIO_NET_HDR_F_DATA_VALID != 0;
        // virtio 1.2 §5.1.6.1: a frame is either partially checksummed
        // or validated, never both, and neither may appear without
        // VIRTIO_NET_F_GUEST_CSUM.
        if needs_csum && data_valid {
            return Err(IoError::DeviceFault);
        }
        if (needs_csum || data_valid) && !self.guest_checksum_negotiated {
            return Err(IoError::DeviceFault);
        }
        let checksum = if data_valid {
            RxChecksumReport::Validated
        } else if needs_csum {
            RxChecksumReport::Partial {
                start: header.csum_start,
                offset: header.csum_offset,
            }
        } else {
            RxChecksumReport::Unverified
        };
        let large_receive_segment_bytes = match header.gso_family() {
            VIRTIO_NET_HDR_GSO_NONE => None,
            VIRTIO_NET_HDR_GSO_TCPV4 if self.guest_tso_v4_negotiated => Some(header.gso_size),
            VIRTIO_NET_HDR_GSO_TCPV6 if self.guest_tso_v6_negotiated => Some(header.gso_size),
            VIRTIO_NET_HDR_GSO_UDP if self.guest_ufo_negotiated => Some(header.gso_size),
            // A segmentation type the driver never asked for: the device
            // is delivering a frame this driver cannot account for.
            _ => return Err(IoError::DeviceFault),
        };
        Ok(RxFrameOffload {
            checksum,
            large_receive_segment_bytes,
            flow_hash: header.flow_hash(),
        })
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
    /// as an external read-only descriptor. The driver holds each
    /// accepted payload's refcounted handle until the used ring returns
    /// its descriptor, so the caller may drop its own handle as soon as
    /// the submission is accepted.
    pub fn try_transmit_scatter_immediate_on_pair(
        &self,
        pair_idx: usize,
        frames: &[helios_netstack::TxFrameRef<'_>],
    ) -> IoResult<Option<usize>> {
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
            tx_payloads,
        } = &mut *state;
        let mut submitted = 0usize;
        for frame in frames {
            let checksum = frame.checksum.map(TxChecksumMeta::from);
            let gso = frame.segmentation.map(TxGsoMeta::from);
            if checksum.is_some() {
                assert!(
                    self.tx_checksum_negotiated,
                    "checksum-offload frame submitted without negotiated VIRTIO_NET_F_CSUM"
                );
            }
            if let Some(gso) = gso {
                self.assert_segmentation_negotiated(gso);
            }
            let payload = frame.payload.filter(|payload| !payload.is_empty());
            // A frame is one buffer (the slot-resident prefix) plus one
            // more when it carries a scatter payload. Asking the ring
            // rather than counting descriptors is what lets an indirect
            // chain cost a single ring slot instead of two.
            let buffers = if payload.is_some() { 2 } else { 1 };
            if !tx_queue.has_room_for(buffers) {
                break;
            }
            let token = tx_queue.next_free_descriptor();
            let token_index = usize::from(token);
            assert!(
                !tx_in_flight.get(token_index),
                "virtio net TX descriptor {token} is still in flight"
            );
            let slot = slot_buffer_mut(tx_buffers, *tx_buffer_len, token_index, "TX");
            let prefix_len = write_tx_payload(slot, self.header_len, frame.bytes, checksum, gso)?;
            let prefix = slot_buffer(tx_buffers, *tx_buffer_len, token_index, prefix_len, "TX");
            let submitted_token = match payload {
                Some(payload) => {
                    tx_queue.submit_read_only_chain_deferred(&self.transport, &[prefix, payload])?
                }
                None => tx_queue.submit_read_only_deferred(&self.transport, prefix)?,
            };
            assert_eq!(
                submitted_token, token,
                "virtio net TX descriptor allocation moved while scatter frame was prepared"
            );
            tx_in_flight.set(token_index);
            // The device now owns the descriptor and reads the payload
            // in place; the handle stays here until it hands the
            // descriptor back.
            tx_payloads[token_index] = payload.cloned();
            submitted += 1;
        }
        if submitted != 0 {
            tx_queue.publish();
            tx_queue.notify(&self.transport);
        }
        Ok(Some(submitted))
    }

    /// Waits for progress on one queue pair, or for anything the device
    /// reports that belongs to no pair.
    ///
    /// An operation belongs to one shard, which drains one pair, so this
    /// is what its wait parks on: a completion on another pair is not
    /// progress it can use, and waking for it would cost the same as
    /// waking the task that did make progress.
    pub async fn wait_for_interrupt_on(&self, pair_idx: usize) {
        let pair_idx = self.normalize_pair_idx(pair_idx);
        let pair = self.queue_pairs[pair_idx].interrupts.notified();
        let mut pair = core::pin::pin!(pair);
        let device = self.interrupts.notified();
        let mut device = core::pin::pin!(device);
        core::future::poll_fn(|cx| {
            use core::future::Future;
            use core::task::Poll;
            if pair.as_mut().poll(cx).is_ready() || device.as_mut().poll(cx).is_ready() {
                return Poll::Ready(());
            }
            Poll::Pending
        })
        .await;
    }

    pub async fn wait_for_interrupt(&self) {
        self.interrupts.notified().await;
    }

    fn rx_bytes_from_slot(&self, pair_idx: usize, slot_index: u16, range: Range<usize>) -> Bytes {
        Bytes::from_owner(RxFrameOwner {
            slot: self.queue_pairs[pair_idx].rx_slots[usize::from(slot_index)].clone(),
            range,
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
        // SAFETY: the assertion above proves the device has returned
        // this slot, and the caller holds the receive state, so the
        // driver is the only writer until it is made available again.
        let buffer = unsafe { &mut *self.queue_pairs[pair_idx].rx_slots[slot].buffer_ptr() };
        let token = state
            .rx_queue
            .submit_output_deferred(&self.transport, buffer)?;
        state.rx_slot_for_token[usize::from(token)] = slot_index;
        state.rx_post_position[usize::from(token)] = state.rx_next_post_position;
        state.rx_next_post_position += 1;
        state.rx_in_device.set(slot);
        Ok(())
    }

    /// Resolves a completed descriptor identifier back to the receive
    /// slot it was carrying and marks that slot as driver-owned again.
    /// Also reports the available-ring position the slot was posted at,
    /// which is what orders the buffers of a mergeable chain.
    fn complete_rx_slot(state: &mut NetRxState<T>, token: u16) -> (u16, u64) {
        let slot_index = *state
            .rx_slot_for_token
            .get(usize::from(token))
            .unwrap_or_else(|| panic!("virtio net RX completion named unknown descriptor {token}"));
        let position = state.rx_post_position[usize::from(token)];
        let slot = usize::from(slot_index);
        assert!(
            state.rx_in_device.get(slot),
            "virtio net RX completion referenced an idle slot {slot_index}"
        );
        state.rx_in_device.clear(slot);
        (slot_index, position)
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
            // The device is done reading this chain, so whatever
            // scatter payload it pointed at is free to go.
            state.tx_payloads[token_index] = None;
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

/// `rss_max_key_size`, `rss_max_indirection_table_length` and
/// `supported_hash_types`, read as one dword each so the transport's
/// aligned config accessors cover them.
fn read_rss_limits<T: VirtioTransport>(transport: &T) -> RssLimits {
    // The three fields straddle one aligned dword: `duplex` at 16,
    // `rss_max_key_size` at 17, and the table length at 18..20.
    let word = transport
        .read_config_u32(NET_CONFIG_DUPLEX_OFFSET)
        .to_le_bytes();
    let key_index = NET_CONFIG_RSS_MAX_KEY_SIZE_OFFSET - NET_CONFIG_DUPLEX_OFFSET;
    let table_index = NET_CONFIG_RSS_MAX_TABLE_LEN_OFFSET - NET_CONFIG_DUPLEX_OFFSET;
    RssLimits {
        max_key_bytes: usize::from(word[key_index]),
        max_table_len: usize::from(u16::from_le_bytes([
            word[table_index],
            word[table_index + 1],
        ])),
        supported_hash_types: transport.read_config_u32(NET_CONFIG_SUPPORTED_HASH_TYPES_OFFSET),
    }
}

/// What a device says it can do about receive-side scaling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RssLimits {
    max_key_bytes: usize,
    max_table_len: usize,
    supported_hash_types: u32,
}

impl RssLimits {
    /// Whether the device can be programmed with the table and key this
    /// driver's own demux is defined in terms of.
    ///
    /// A device that cannot is not steered at all: half-programming it
    /// would put a flow on a queue whose processor does not own the
    /// socket, which is worse than the extra hop.
    const fn accepts_our_table(self) -> bool {
        self.max_key_bytes >= RSS_KEY_BYTES
            && self.max_table_len >= RSS_INDIRECTION_ENTRIES
            && self.supported_hash_types & NET_RSS_HASH_TYPES == NET_RSS_HASH_TYPES
    }
}

fn read_mac_address<T: VirtioTransport>(transport: &T) -> [u8; 6] {
    let low = transport.read_config_u32(0).to_le_bytes();
    let high = transport.read_config_u32(4).to_le_bytes();
    [low[0], low[1], low[2], low[3], high[0], high[1]]
}

/// Reads `status` out of the virtio-net configuration space. A device
/// without VIRTIO_NET_F_STATUS keeps no link status there and is always
/// treated as up.
fn read_link_up<T: VirtioTransport>(transport: &T, features: NegotiatedFeatures) -> bool {
    if !features.device(NET_FEATURE_STATUS) {
        return true;
    }
    // The status field straddles bytes 6..8, the upper half of the
    // aligned dword at offset 4.
    let config = transport
        .read_config_u32(NET_CONFIG_STATUS_OFFSET & !0x3)
        .to_le_bytes();
    let status = u16::from_le_bytes([config[2], config[3]]);
    status & NET_STATUS_LINK_UP != 0
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

/// Writes the virtio-net header for one transmit frame followed by the
/// frame bytes themselves, and returns the total slot bytes used.
///
/// `frame` is the whole wire frame on the copying path and only the
/// replicated header prefix on the scatter path; either way the
/// virtio-net header in front of it describes the complete frame,
/// including the segmentation metadata the device splits it by.
fn write_tx_payload(
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
            let family = if meta.ipv6 {
                VIRTIO_NET_HDR_GSO_TCPV6
            } else {
                VIRTIO_NET_HDR_GSO_TCPV4
            };
            if meta.ecn {
                family | VIRTIO_NET_HDR_GSO_ECN
            } else {
                family
            }
        }
    };
    buffer[header_len..payload_len].copy_from_slice(frame);
    Ok(payload_len)
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;
    use core::mem::size_of;

    use bytes::Bytes;
    use helios_netstack::RxChecksumReport;

    use super::{
        DescriptorBitSet, ETH_HEADER_LEN, MAX_LARGE_RECEIVE_FRAME_BYTES,
        MAX_SEGMENTED_TRANSMIT_FRAME_BYTES, NET_CONFIG_STATUS_OFFSET, NET_FEATURE_CSUM,
        NET_FEATURE_GUEST_CSUM, NET_FEATURE_GUEST_ECN, NET_FEATURE_GUEST_TSO4,
        NET_FEATURE_GUEST_TSO6, NET_FEATURE_GUEST_UFO, NET_FEATURE_HOST_ECN, NET_FEATURE_HOST_TSO4,
        NET_FEATURE_HOST_TSO6, NET_FEATURE_MRG_RXBUF, NET_FEATURE_STATUS, NET_STATUS_LINK_UP,
        RX_PAGE_BYTES, RxFrame, TxChecksumMeta, TxGsoMeta, VIRTIO_NET_HDR_F_DATA_VALID,
        VIRTIO_NET_HDR_F_NEEDS_CSUM, VIRTIO_NET_HDR_GSO_TCPV4, VirtioNetDevice, VirtioNetHeader,
        write_tx_payload,
    };
    use crate::testing::{FakeTransport, FakeTransportConfig};
    use crate::transport::{DeviceType, VirtioFeatures};
    use helios_hal::io::IoError;
    use helios_netstack::LinkState;

    /// One receive completion the fake device hands the driver: the
    /// virtio-net header it writes in front of the frame (head buffers
    /// only) and the frame bytes that follow.
    struct DeviceRxBuffer<'a> {
        header: Option<RxDeviceHeader>,
        payload: &'a [u8],
    }

    #[derive(Clone, Copy, Default)]
    struct RxDeviceHeader {
        flags: u8,
        gso_type: u8,
        gso_size: u16,
        csum_start: u16,
        csum_offset: u16,
        num_buffers: u16,
    }

    /// A driver bound to a fake device, plus the device-side moves the
    /// receive tests need: filling posted buffers and completing them.
    struct NetHarness {
        device: VirtioNetDevice<FakeTransport>,
    }

    impl NetHarness {
        fn new(offered_features: u64) -> Self {
            Self::with_config(offered_features, |_| {})
        }

        fn with_config(offered_features: u64, configure: impl FnOnce(&FakeTransport)) -> Self {
            let transport = FakeTransport::new(FakeTransportConfig {
                device_type: DeviceType::Network,
                offered_features: VirtioFeatures::VERSION_1.bits() | offered_features,
                queue_size: 8,
                supports_queue_reset: false,
                absent_queues: &[],
            });
            configure(&transport);
            Self {
                device: VirtioNetDevice::new(transport).expect("fake virtio-net should initialize"),
            }
        }

        /// Writes one buffer of a receive frame into the slot the driver
        /// posted at descriptor `token` and completes that descriptor,
        /// exactly as a device filling made-available buffers would.
        fn complete_rx(&self, token: u16, buffer: DeviceRxBuffer<'_>) {
            let pair = &self.device.queue_pairs[0];
            let state = pair
                .rx_state
                .try_lock()
                .expect("test receive state is uncontended");
            let slot = usize::from(state.rx_slot_for_token[usize::from(token)]);
            // SAFETY: this stands in for the device writing the buffer
            // it was given, which is the one moment the driver is not
            // touching the slot; the receive state is held for the
            // duration.
            let target = unsafe { &mut *pair.rx_slots[slot].buffer_ptr() };
            let header_len = self.device.header_len;
            let written = match buffer.header {
                Some(header) => {
                    target[..header_len].fill(0);
                    target[0] = header.flags;
                    target[1] = header.gso_type;
                    target[4..6].copy_from_slice(&header.gso_size.to_le_bytes());
                    target[6..8].copy_from_slice(&header.csum_start.to_le_bytes());
                    target[8..10].copy_from_slice(&header.csum_offset.to_le_bytes());
                    target[10..12].copy_from_slice(&header.num_buffers.to_le_bytes());
                    target[header_len..header_len + buffer.payload.len()]
                        .copy_from_slice(buffer.payload);
                    header_len + buffer.payload.len()
                }
                None => {
                    target[..buffer.payload.len()].copy_from_slice(buffer.payload);
                    buffer.payload.len()
                }
            };
            state
                .rx_queue
                .device_complete(token, u32::try_from(written).expect("test length fits"));
        }

        /// Delivers one frame as `parts.len()` mergeable buffers,
        /// starting at descriptor `first_token` and continuing through
        /// consecutive descriptors, and returns what the driver made of
        /// it.
        fn deliver_chain(
            &self,
            first_token: u16,
            header: RxDeviceHeader,
            parts: &[&[u8]],
        ) -> Result<Option<RxFrame>, helios_hal::io::IoError> {
            let header = RxDeviceHeader {
                num_buffers: u16::try_from(parts.len()).expect("test chain length fits"),
                ..header
            };
            self.complete_rx(
                first_token,
                DeviceRxBuffer {
                    header: Some(header),
                    payload: parts[0],
                },
            );
            for (index, part) in parts.iter().enumerate().skip(1) {
                self.complete_rx(
                    first_token + u16::try_from(index).expect("test chain length fits"),
                    DeviceRxBuffer {
                        header: None,
                        payload: part,
                    },
                );
            }
            self.receive()
        }

        fn receive(&self) -> Result<Option<RxFrame>, helios_hal::io::IoError> {
            let mut frames = [const { None }; 1];
            let received = self
                .device
                .try_receive_frames_immediate(&mut frames)?
                .expect("the receive ring is uncontended in tests");
            assert!(received <= 1);
            Ok(frames[0].take())
        }
    }

    /// The driver refuses a frame it cannot account for by faulting the
    /// device rather than by delivering something it made up.
    fn expect_device_fault(result: Result<Option<RxFrame>, helios_hal::io::IoError>) {
        match result {
            Err(helios_hal::io::IoError::DeviceFault) => {}
            Err(error) => panic!("expected a device fault, got {error:?}"),
            Ok(frame) => panic!(
                "expected a device fault, got a {:?}-byte frame",
                frame.map(|frame| frame.bytes.len())
            ),
        }
    }

    const RECEIVE_OFFLOAD_FEATURES: u64 = NET_FEATURE_GUEST_CSUM
        | NET_FEATURE_GUEST_TSO4
        | NET_FEATURE_GUEST_TSO6
        | NET_FEATURE_GUEST_ECN
        | NET_FEATURE_GUEST_UFO
        | NET_FEATURE_MRG_RXBUF;

    /// A device offering the transmit segmentation families gets them,
    /// gets the ECN modifier that rides on them, and reports both to
    /// the network stack — including the oversized frame ceiling that
    /// makes segmentation worth anything.
    #[test]
    fn offered_transmit_segmentation_is_negotiated_and_published() {
        let harness = NetHarness::new(
            NET_FEATURE_CSUM | NET_FEATURE_HOST_TSO4 | NET_FEATURE_HOST_TSO6 | NET_FEATURE_HOST_ECN,
        );
        let features = harness.device.features();

        assert!(features.device(NET_FEATURE_HOST_TSO4));
        assert!(features.device(NET_FEATURE_HOST_TSO6));
        assert!(features.device(NET_FEATURE_HOST_ECN));
        assert!(harness.device.tso_negotiated(false));
        assert!(harness.device.tso_negotiated(true));
        assert!(harness.device.tso_ecn_negotiated());

        let capabilities = harness.device.interface_capabilities();
        assert!(capabilities.segmentation.tx_tcp_ipv4);
        assert!(capabilities.segmentation.tx_tcp_ipv6);
        assert!(capabilities.segmentation.tx_tcp_ecn);
        assert_eq!(
            capabilities.segmentation.max_tx_frame_bytes,
            MAX_SEGMENTED_TRANSMIT_FRAME_BYTES
        );
        assert!(capabilities.segmentation.max_tx_frame_bytes > capabilities.max_frame_len);
    }

    /// Transmit segmentation needs the device to finish each segment's
    /// checksum, so a device offering TSO without CSUM gets neither the
    /// families nor the oversized frame ceiling.
    #[test]
    fn transmit_segmentation_is_refused_without_transmit_checksum() {
        let harness = NetHarness::new(NET_FEATURE_HOST_TSO4 | NET_FEATURE_HOST_TSO6);

        assert!(!harness.device.features().device(NET_FEATURE_HOST_TSO4));
        assert!(!harness.device.features().device(NET_FEATURE_HOST_TSO6));
        assert!(!harness.device.tso_negotiated(false));
        let capabilities = harness.device.interface_capabilities();
        assert!(!capabilities.segmentation.tx_tcp_ipv4);
        assert_eq!(
            capabilities.segmentation.max_tx_frame_bytes,
            capabilities.max_frame_len
        );
    }

    /// ECN is a modifier on a transmit segmentation type, so a device
    /// offering it without a TSO family must not have it accepted.
    #[test]
    fn host_ecn_is_refused_without_a_transmit_segmentation_family() {
        let harness = NetHarness::new(NET_FEATURE_CSUM | NET_FEATURE_HOST_ECN);

        assert!(!harness.device.features().device(NET_FEATURE_HOST_ECN));
        assert!(!harness.device.tso_ecn_negotiated());
    }

    #[test]
    fn every_offered_receive_offload_is_negotiated() {
        let harness = NetHarness::new(RECEIVE_OFFLOAD_FEATURES);
        let features = harness.device.features();

        assert!(features.device(NET_FEATURE_GUEST_CSUM));
        assert!(features.device(NET_FEATURE_GUEST_TSO4));
        assert!(features.device(NET_FEATURE_GUEST_TSO6));
        assert!(features.device(NET_FEATURE_GUEST_ECN));
        assert!(features.device(NET_FEATURE_GUEST_UFO));
        assert!(features.device(NET_FEATURE_MRG_RXBUF));
        assert!(harness.device.guest_checksum_negotiated());
        assert!(harness.device.mergeable_rx_buffers());
        assert!(harness.device.large_receive_negotiated(false));
        assert!(harness.device.large_receive_negotiated(true));
        // Mergeable buffers are what make a page-granular receive slot
        // enough for a frame the device coalesced out of many segments.
        assert_eq!(harness.device.rx_buffer_len, RX_PAGE_BYTES);
        assert_eq!(
            harness.device.max_receive_frame_len(),
            MAX_LARGE_RECEIVE_FRAME_BYTES
        );
        assert_eq!(harness.device.max_frame_len(), 1500 + ETH_HEADER_LEN);
    }

    /// Receive segmentation needs somewhere to put a 64 KiB frame. With
    /// page-granular slots that is mergeable buffers, so a device that
    /// offers segmentation without merging gets neither.
    #[test]
    fn receive_segmentation_is_refused_without_mergeable_buffers() {
        let harness = NetHarness::new(
            NET_FEATURE_GUEST_CSUM | NET_FEATURE_GUEST_TSO4 | NET_FEATURE_GUEST_TSO6,
        );
        let features = harness.device.features();

        assert!(features.device(NET_FEATURE_GUEST_CSUM));
        assert!(!features.device(NET_FEATURE_GUEST_TSO4));
        assert!(!features.device(NET_FEATURE_GUEST_TSO6));
        assert!(!harness.device.large_receive_negotiated(false));
        // Without merging every frame has to fit one slot whole.
        assert_eq!(harness.device.rx_buffer_len, RX_PAGE_BYTES);
        assert_eq!(
            harness.device.max_receive_frame_len(),
            1500 + ETH_HEADER_LEN
        );
    }

    /// ECN is a modifier on a segmentation type, so a device offering it
    /// without a TCP segmentation family must not have it accepted.
    #[test]
    fn guest_ecn_is_refused_without_a_tcp_segmentation_family() {
        let harness = NetHarness::new(
            NET_FEATURE_GUEST_CSUM
                | NET_FEATURE_MRG_RXBUF
                | NET_FEATURE_GUEST_ECN
                | NET_FEATURE_GUEST_UFO,
        );
        let features = harness.device.features();

        assert!(features.device(NET_FEATURE_GUEST_UFO));
        assert!(!features.device(NET_FEATURE_GUEST_ECN));
    }

    /// The receive offloads must not be claimed to a device that never
    /// offered them.
    #[test]
    fn receive_offloads_are_not_claimed_to_a_device_without_them() {
        let harness = NetHarness::new(0);
        let features = harness.device.features();

        assert!(!features.device(NET_FEATURE_GUEST_CSUM));
        assert!(!features.device(NET_FEATURE_MRG_RXBUF));
        assert!(!harness.device.guest_checksum_negotiated());
        assert!(!harness.device.mergeable_rx_buffers());
    }

    /// A frame that fits one buffer is handed over as a borrow of the
    /// receive slot the device wrote it into — no copy, and the slot
    /// stays out of the ring until the frame is dropped.
    #[test]
    fn a_single_buffer_frame_borrows_its_receive_slot() {
        let harness = NetHarness::new(RECEIVE_OFFLOAD_FEATURES);
        let payload: Vec<u8> = (0..512_u32).map(|index| index as u8).collect();

        let frame = harness
            .deliver_chain(0, RxDeviceHeader::default(), &[&payload])
            .expect("a well-formed frame is accepted")
            .expect("the driver should deliver the frame");

        assert_eq!(frame.bytes.as_ref(), payload.as_slice());
        let slot_start = harness.device.queue_pairs[0].rx_slots[0]
            .buffer()
            .as_ptr()
            .addr();
        assert_eq!(
            frame.bytes.as_ptr().addr(),
            slot_start + harness.device.header_len,
            "a single-buffer frame must borrow the slot rather than copy out of it"
        );
    }

    /// A mergeable chain is assembled in the order the buffers were made
    /// available, across any number of buffers.
    #[test]
    fn a_mergeable_chain_is_assembled_across_buffers() {
        for parts in [2_usize, 3] {
            let harness = NetHarness::new(RECEIVE_OFFLOAD_FEATURES);
            let head: Vec<u8> = vec![0xa1; 4000];
            let tails: Vec<Vec<u8>> = (1..parts)
                .map(|index| vec![0xb0 + index as u8; 3000])
                .collect();
            let mut buffers: Vec<&[u8]> = vec![head.as_slice()];
            buffers.extend(tails.iter().map(Vec::as_slice));
            let mut expected: Vec<u8> = Vec::new();
            for buffer in &buffers {
                expected.extend_from_slice(buffer);
            }

            let frame = harness
                .deliver_chain(0, RxDeviceHeader::default(), &buffers)
                .expect("a well-formed chain is accepted")
                .expect("the driver should deliver the assembled frame");

            assert_eq!(frame.bytes.len(), expected.len());
            assert_eq!(frame.bytes.as_ref(), expected.as_slice());
        }
    }

    /// Every buffer of an assembled chain goes back to the device, so a
    /// chained frame does not leak receive slots.
    #[test]
    fn an_assembled_chain_reposts_every_buffer() {
        let harness = NetHarness::new(RECEIVE_OFFLOAD_FEATURES);
        let head = vec![0x11_u8; 4000];
        let tail = vec![0x22_u8; 1000];

        let frame = harness
            .deliver_chain(0, RxDeviceHeader::default(), &[&head, &tail])
            .expect("a well-formed chain is accepted")
            .expect("the driver should deliver the assembled frame");
        drop(frame);
        harness.receive().expect("an empty ring is not an error");

        let state = harness.device.queue_pairs[0]
            .rx_state
            .try_lock()
            .expect("test receive state is uncontended");
        assert_eq!(
            state.rx_queue.available_descriptors(),
            0,
            "every receive slot must be back in the device's hands"
        );
    }

    /// A device that completes a buffer which was not the next one made
    /// available has not delivered a chain this driver can reconstruct.
    /// Stitching it together anyway would splice an unrelated frame into
    /// the middle of this one.
    #[test]
    fn an_out_of_order_chain_tail_is_refused() {
        let harness = NetHarness::new(RECEIVE_OFFLOAD_FEATURES);
        let head = vec![0x33_u8; 4000];
        let tail = vec![0x44_u8; 1000];

        harness.complete_rx(
            0,
            DeviceRxBuffer {
                header: Some(RxDeviceHeader {
                    num_buffers: 2,
                    ..RxDeviceHeader::default()
                }),
                payload: &head,
            },
        );
        // The buffer posted third, not the one that follows the head.
        harness.complete_rx(
            2,
            DeviceRxBuffer {
                header: None,
                payload: &tail,
            },
        );

        expect_device_fault(harness.receive());
    }

    /// The per-frame checksum report is what the stack decides trust
    /// from, so each header flag has to arrive intact.
    #[test]
    fn the_per_frame_checksum_report_reaches_the_stack() {
        let harness = NetHarness::new(RECEIVE_OFFLOAD_FEATURES);
        let payload = vec![0x5a_u8; 64];

        let validated = harness
            .deliver_chain(
                0,
                RxDeviceHeader {
                    flags: VIRTIO_NET_HDR_F_DATA_VALID,
                    ..RxDeviceHeader::default()
                },
                &[&payload],
            )
            .expect("a validated frame is accepted")
            .expect("the driver should deliver the frame");
        assert_eq!(validated.offload.checksum, RxChecksumReport::Validated);
        assert_eq!(validated.offload.large_receive_segment_bytes, None);
        drop(validated);

        let partial = harness
            .deliver_chain(
                1,
                RxDeviceHeader {
                    flags: VIRTIO_NET_HDR_F_NEEDS_CSUM,
                    csum_start: 34,
                    csum_offset: 16,
                    ..RxDeviceHeader::default()
                },
                &[&payload],
            )
            .expect("a partially checksummed frame is accepted")
            .expect("the driver should deliver the frame");
        assert_eq!(
            partial.offload.checksum,
            RxChecksumReport::Partial {
                start: 34,
                offset: 16,
            }
        );
        drop(partial);

        let plain = harness
            .deliver_chain(2, RxDeviceHeader::default(), &[&payload])
            .expect("a frame without offload metadata is accepted")
            .expect("the driver should deliver the frame");
        assert_eq!(plain.offload.checksum, RxChecksumReport::Unverified);
    }

    /// virtio 1.2 §5.1.6.1 allows one of the two checksum flags, never
    /// both, and neither at all without VIRTIO_NET_F_GUEST_CSUM.
    #[test]
    fn contradictory_checksum_flags_are_refused() {
        let harness = NetHarness::new(RECEIVE_OFFLOAD_FEATURES);
        let payload = vec![0x5a_u8; 64];

        expect_device_fault(harness.deliver_chain(
            0,
            RxDeviceHeader {
                flags: VIRTIO_NET_HDR_F_DATA_VALID | VIRTIO_NET_HDR_F_NEEDS_CSUM,
                ..RxDeviceHeader::default()
            },
            &[&payload],
        ));

        let without_offload = NetHarness::new(0);
        expect_device_fault(without_offload.deliver_chain(
            0,
            RxDeviceHeader {
                flags: VIRTIO_NET_HDR_F_DATA_VALID,
                ..RxDeviceHeader::default()
            },
            &[&payload],
        ));
    }

    /// A coalesced frame reports the wire segment size it was built from,
    /// and a segmentation type the driver never negotiated is refused.
    #[test]
    fn a_coalesced_frame_reports_its_segment_size() {
        let harness = NetHarness::new(RECEIVE_OFFLOAD_FEATURES);
        let payload = vec![0x77_u8; 6000];

        let frame = harness
            .deliver_chain(
                0,
                RxDeviceHeader {
                    flags: VIRTIO_NET_HDR_F_DATA_VALID,
                    gso_type: VIRTIO_NET_HDR_GSO_TCPV4,
                    gso_size: 1448,
                    ..RxDeviceHeader::default()
                },
                &[&payload[..4000], &payload[4000..]],
            )
            .expect("a coalesced frame is accepted")
            .expect("the driver should deliver the frame");
        assert_eq!(frame.offload.large_receive_segment_bytes, Some(1448));
        assert_eq!(frame.bytes.len(), payload.len());

        let without_segmentation = NetHarness::new(NET_FEATURE_GUEST_CSUM | NET_FEATURE_MRG_RXBUF);
        expect_device_fault(without_segmentation.deliver_chain(
            0,
            RxDeviceHeader {
                gso_type: VIRTIO_NET_HDR_GSO_TCPV4,
                gso_size: 1448,
                ..RxDeviceHeader::default()
            },
            &[&payload[..1000]],
        ));
    }

    /// A configuration-change interrupt is what tells the driver to
    /// re-read the link status; a used-buffer interrupt is not.
    #[test]
    fn a_configuration_change_republishes_the_link_state() {
        let harness = NetHarness::with_config(NET_FEATURE_STATUS, |transport| {
            transport.set_config_u16(NET_CONFIG_STATUS_OFFSET, NET_STATUS_LINK_UP);
        });
        assert_eq!(harness.device.link_state(), LinkState::Up);

        // The link drops, but only a queue interrupt is raised: nothing
        // told the driver its configuration moved.
        harness
            .device
            .transport
            .set_config_u16(NET_CONFIG_STATUS_OFFSET, 0);
        harness.device.transport.raise_interrupt(1);
        harness.device.handle_interrupt();
        assert_eq!(harness.device.link_state(), LinkState::Up);

        harness.device.transport.raise_interrupt(2);
        harness.device.handle_interrupt();
        assert_eq!(harness.device.link_state(), LinkState::Down);

        harness
            .device
            .transport
            .set_config_u16(NET_CONFIG_STATUS_OFFSET, NET_STATUS_LINK_UP);
        harness.device.transport.raise_interrupt(3);
        harness.device.handle_interrupt();
        assert_eq!(harness.device.link_state(), LinkState::Up);
        assert_eq!(harness.device.transport.acknowledged_interrupts(), 3);
    }

    /// A device without VIRTIO_NET_F_STATUS keeps no link status, and
    /// the zeroed configuration space must not read as a dead link.
    #[test]
    fn a_device_without_status_is_always_up() {
        let harness = NetHarness::new(0);

        assert_eq!(harness.device.link_state(), LinkState::Up);
        harness.device.transport.raise_interrupt(2);
        harness.device.handle_interrupt();
        assert_eq!(harness.device.link_state(), LinkState::Up);
    }

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

    /// A scatter frame reaches the device as two descriptors: the
    /// header prefix out of the driver's own slot, and the payload
    /// pointed at where the caller already had it. The driver keeps the
    /// payload alive for exactly as long as the device owns the
    /// descriptor, so the caller may drop its handle immediately.
    #[test]
    fn a_scatter_frame_chains_its_payload_by_reference_until_completion() {
        let harness = NetHarness::new(NET_FEATURE_CSUM);
        let headers = [0x11_u8; 54];
        let payload = Bytes::from(vec![0xa5_u8; 2048]);
        let payload_address = payload.as_ptr() as u64;
        let token = {
            let state = harness.device.queue_pairs[0]
                .tx_state
                .try_lock()
                .expect("test transmit state is uncontended");
            state.tx_queue.next_free_descriptor()
        };

        let submitted = harness
            .device
            .try_transmit_scatter_immediate_on_pair(
                0,
                &[helios_netstack::TxFrameRef {
                    bytes: &headers,
                    payload: Some(&payload),
                    checksum: None,
                    segmentation: None,
                }],
            )
            .expect("scatter submission should succeed")
            .expect("the transmit ring is uncontended in tests");
        assert_eq!(submitted, 1);
        // The caller's handle is gone; only the driver's pin keeps the
        // payload alive while the device reads it.
        drop(payload);

        let chain = {
            let state = harness.device.queue_pairs[0]
                .tx_state
                .try_lock()
                .expect("test transmit state is uncontended");
            let pinned = state.tx_payloads[usize::from(token)]
                .as_ref()
                .expect("the driver pins a scatter payload until completion");
            assert_eq!(pinned.as_ptr() as u64, payload_address);
            assert_eq!(pinned.len(), 2048);
            state.tx_queue.device_chain(token)
        };
        assert_eq!(chain.len(), 2, "prefix descriptor plus payload descriptor");
        assert_eq!(
            chain[0].1 as usize,
            harness.device.header_len + headers.len(),
            "the prefix descriptor covers the virtio header and the frame headers"
        );
        assert_eq!(
            chain[1],
            (payload_address, 2048, false),
            "the payload descriptor points at the caller's bytes, read-only"
        );

        harness.device.queue_pairs[0]
            .tx_state
            .try_lock()
            .expect("test transmit state is uncontended")
            .tx_queue
            .device_complete(token, 0);
        let reclaimed = harness
            .device
            .reclaim_transmit_completions_immediate_on_pair(0, 8)
            .expect("completion drain should succeed")
            .expect("the transmit ring is uncontended in tests");
        assert_eq!(reclaimed, 1);
        let state = harness.device.queue_pairs[0]
            .tx_state
            .try_lock()
            .expect("test transmit state is uncontended");
        assert!(
            state.tx_payloads[usize::from(token)].is_none(),
            "a completed descriptor releases the payload it was reading"
        );
    }

    /// The used ring is not the available ring: a device may finish
    /// the second chain first, and the driver must release that chain's
    /// payload and only that one.
    #[test]
    fn an_out_of_order_completion_releases_only_its_own_payload() {
        let harness = NetHarness::new(NET_FEATURE_CSUM);
        let headers = [0x22_u8; 54];
        let first = Bytes::from(vec![0x11_u8; 512]);
        let second = Bytes::from(vec![0x22_u8; 768]);
        let first_address = first.as_ptr() as u64;
        let second_address = second.as_ptr() as u64;
        let first_token = {
            let state = harness.device.queue_pairs[0]
                .tx_state
                .try_lock()
                .expect("test transmit state is uncontended");
            state.tx_queue.next_free_descriptor()
        };

        let submitted = harness
            .device
            .try_transmit_scatter_immediate_on_pair(
                0,
                &[
                    helios_netstack::TxFrameRef {
                        bytes: &headers,
                        payload: Some(&first),
                        checksum: None,
                        segmentation: None,
                    },
                    helios_netstack::TxFrameRef {
                        bytes: &headers,
                        payload: Some(&second),
                        checksum: None,
                        segmentation: None,
                    },
                ],
            )
            .expect("scatter submission should succeed")
            .expect("the transmit ring is uncontended in tests");
        assert_eq!(submitted, 2);
        drop(first);
        drop(second);

        let second_token = {
            let state = harness.device.queue_pairs[0]
                .tx_state
                .try_lock()
                .expect("test transmit state is uncontended");
            let second_token = state
                .tx_payloads
                .iter()
                .position(|pinned| {
                    pinned
                        .as_ref()
                        .is_some_and(|payload| payload.as_ptr() as u64 == second_address)
                })
                .expect("the second frame's payload is pinned");
            u16::try_from(second_token).expect("test token fits")
        };
        assert_ne!(second_token, first_token);

        // The device finishes the second chain first.
        harness.device.queue_pairs[0]
            .tx_state
            .try_lock()
            .expect("test transmit state is uncontended")
            .tx_queue
            .device_complete(second_token, 0);
        let reclaimed = harness
            .device
            .reclaim_transmit_completions_immediate_on_pair(0, 8)
            .expect("completion drain should succeed")
            .expect("the transmit ring is uncontended in tests");
        assert_eq!(reclaimed, 1);

        let state = harness.device.queue_pairs[0]
            .tx_state
            .try_lock()
            .expect("test transmit state is uncontended");
        assert!(
            state.tx_payloads[usize::from(second_token)].is_none(),
            "the completed chain released its payload"
        );
        let still_pinned = state.tx_payloads[usize::from(first_token)]
            .as_ref()
            .expect("the chain the device still owns keeps its payload");
        assert_eq!(still_pinned.as_ptr() as u64, first_address);
        assert_eq!(still_pinned.len(), 512);
    }

    /// virtio 1.2 §5.1.6.2: an oversized frame carries NEEDS_CSUM with
    /// its csum_start/csum_offset, the segmentation family in
    /// `gso_type`, the replicated header length in `hdr_len`, and the
    /// per-segment payload in `gso_size`. Every byte of that header is
    /// pinned here, for both address families and with and without the
    /// ECN modifier.
    #[test]
    fn tx_payload_write_populates_gso_header() {
        let header_len = size_of::<VirtioNetHeader>();
        let mut buffer = [0u8; 256];
        let frame = [0u8; 60];
        let checksum = Some(TxChecksumMeta {
            start: 34,
            offset: 16,
        });
        write_tx_payload(
            &mut buffer,
            header_len,
            &frame,
            checksum,
            Some(TxGsoMeta {
                ipv6: false,
                hdr_len: 54,
                mss: 1460,
                ecn: false,
            }),
        )
        .expect("gso payload fits");
        assert_eq!(buffer[0], super::VIRTIO_NET_HDR_F_NEEDS_CSUM);
        assert_eq!(buffer[1], super::VIRTIO_NET_HDR_GSO_TCPV4);
        assert_eq!(&buffer[2..4], &54u16.to_le_bytes());
        assert_eq!(&buffer[4..6], &1460u16.to_le_bytes());
        assert_eq!(&buffer[6..8], &34u16.to_le_bytes());
        assert_eq!(&buffer[8..10], &16u16.to_le_bytes());

        // IPv6 selects the TCPV6 family, and CWR in the header template
        // rides along as the ECN modifier on it.
        write_tx_payload(
            &mut buffer,
            header_len,
            &frame,
            checksum,
            Some(TxGsoMeta {
                ipv6: true,
                hdr_len: 74,
                mss: 1440,
                ecn: true,
            }),
        )
        .expect("ipv6 gso payload fits");
        assert_eq!(
            buffer[1],
            super::VIRTIO_NET_HDR_GSO_TCPV6 | super::VIRTIO_NET_HDR_GSO_ECN
        );
        assert_eq!(&buffer[2..4], &74u16.to_le_bytes());
        assert_eq!(&buffer[4..6], &1440u16.to_le_bytes());

        // A non-GSO frame reusing the slot scrubs gso_type back to NONE.
        write_tx_payload(&mut buffer, header_len, &frame, None, None).expect("plain payload fits");
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
                None,
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
            write_tx_payload(&mut buffer, header_len, &second, None, None)
                .expect("second payload fits"),
            header_len + second.len()
        );
        assert_eq!(&buffer[..header_len], zero_header);
        assert_eq!(&buffer[header_len..header_len + second.len()], second);
    }

    /// virtio-net lays its queues out RX0, TX0, RX1, TX1, ... and the
    /// control queue follows every pair the *device* advertises, not
    /// every pair the driver activated. Programming a coalescing budget
    /// for the wrong index would silently tune somebody else's ring.
    #[test]
    fn queue_indices_follow_the_virtio_net_layout() {
        assert_eq!(super::rx_queue_index(0), 0);
        assert_eq!(super::tx_queue_index(0), 1);
        assert_eq!(super::rx_queue_index(3), 6);
        assert_eq!(super::tx_queue_index(3), 7);
        assert_eq!(super::control_queue_index(4), 8);
    }

    /// The table the device is handed has to be the same function the
    /// driver evaluates in software, entry for entry, or a steered
    /// frame lands on a queue whose processor does not own the socket.
    #[test]
    fn the_rss_table_matches_the_software_rule() {
        for pairs in [1usize, 2, 3, 4, 8] {
            for slot in 0..super::RSS_INDIRECTION_ENTRIES {
                let queue = super::rss_indirection_entry(slot, pairs);
                assert!(queue < pairs);
                assert_eq!(
                    queue,
                    super::FlowHash::new(slot as u32).bucket(pairs),
                    "the programmed table and the software bucket must agree"
                );
            }
        }
    }

    /// A device that cannot hold the driver's table or key, or that
    /// cannot hash all four flow types, is left unsteered rather than
    /// half-programmed.
    #[test]
    fn rss_limits_are_checked_before_the_device_is_steered() {
        let usable = super::RssLimits {
            max_key_bytes: super::RSS_KEY_BYTES,
            max_table_len: super::RSS_INDIRECTION_ENTRIES,
            supported_hash_types: super::NET_RSS_HASH_TYPES,
        };
        assert!(usable.accepts_our_table());
        assert!(
            !super::RssLimits {
                max_key_bytes: super::RSS_KEY_BYTES - 1,
                ..usable
            }
            .accepts_our_table()
        );
        assert!(
            !super::RssLimits {
                max_table_len: super::RSS_INDIRECTION_ENTRIES - 1,
                ..usable
            }
            .accepts_our_table()
        );
        assert!(
            !super::RssLimits {
                supported_hash_types: super::NET_RSS_HASH_TYPES & !super::NET_HASH_TYPE_UDPV6,
                ..usable
            }
            .accepts_our_table()
        );
        // A device offering more than the driver needs is still usable.
        assert!(
            super::RssLimits {
                max_key_bytes: 64,
                max_table_len: 512,
                supported_hash_types: u32::MAX,
            }
            .accepts_our_table()
        );
    }

    /// The progress mask an interrupt hands back is what a single-line
    /// transport steers by, so it has to name exactly the pairs with
    /// completions and nothing else.
    #[test]
    fn queue_pair_progress_names_only_the_pairs_recorded() {
        let mut progress = super::QueuePairProgress::none();
        assert!(progress.is_empty());
        assert_eq!(progress.iter().count(), 0);

        progress.record(0);
        progress.record(3);
        // Recording a pair twice is not two wakes.
        progress.record(3);
        assert!(!progress.is_empty());
        assert_eq!(progress.iter().collect::<alloc::vec::Vec<_>>(), [0, 3]);

        let mut full = super::QueuePairProgress::none();
        for pair_idx in 0..usize::from(super::NET_MAX_QUEUE_PAIRS) {
            full.record(pair_idx);
        }
        assert_eq!(full.iter().count(), usize::from(super::NET_MAX_QUEUE_PAIRS));
    }

    #[test]
    #[should_panic(expected = "outside the maximum this driver brings up")]
    fn a_pair_beyond_the_maximum_is_rejected() {
        let mut progress = super::QueuePairProgress::none();
        progress.record(usize::from(super::NET_MAX_QUEUE_PAIRS));
    }

    /// The hash the device reports is only meaningful when it says it
    /// classified the frame, and it only exists in the longer header.
    #[test]
    fn a_reported_hash_needs_both_the_long_header_and_a_report_type() {
        let mut header = [0_u8; super::HASH_REPORT_HEADER_BYTES];
        header[12..16].copy_from_slice(&0xdead_beef_u32.to_le_bytes());
        header[16..18].copy_from_slice(&1_u16.to_le_bytes());

        let long = super::RxHeader::parse(&header, super::HASH_REPORT_HEADER_BYTES);
        assert_eq!(
            long.flow_hash().map(super::FlowHash::get),
            Some(0xdead_beef)
        );

        // The same bytes read as a short header report nothing: those
        // eight bytes are the next frame's, not a hash.
        let short = super::RxHeader::parse(&header, size_of::<VirtioNetHeader>());
        assert_eq!(short.flow_hash(), None);

        // A long header whose report type is NONE means the device
        // classified nothing.
        header[16..18].copy_from_slice(&super::NET_HASH_REPORT_NONE.to_le_bytes());
        assert_eq!(
            super::RxHeader::parse(&header, super::HASH_REPORT_HEADER_BYTES).flow_hash(),
            None
        );
    }

    #[test]
    fn coalescing_budget_encodes_the_control_queue_payloads() {
        let budget = super::CoalescingBudget {
            max_packets: 8,
            max_usecs: 50,
        };

        // `struct virtio_net_ctrl_coal`: max_packets then max_usecs.
        assert_eq!(budget.encode(), [8, 0, 0, 0, 50, 0, 0, 0]);

        // `struct virtio_net_ctrl_coal_vq`: the virtqueue number, two
        // reserved bytes, then the same budget.
        assert_eq!(
            budget.encode_for_queue(super::tx_queue_index(2)),
            [5, 0, 0, 0, 8, 0, 0, 0, 50, 0, 0, 0]
        );
    }

    /// The virtio-net configuration field holding `max_virtqueue_pairs`.
    const CONFIG_MAX_VIRTQUEUE_PAIRS_OFFSET: usize = 8;

    /// A device presenting `queue_size`-deep queues at every index
    /// except the ones named, advertising `advertised_pairs` in its
    /// configuration space.
    fn multiqueue_transport(
        offered_features: u64,
        advertised_pairs: u16,
        absent_queues: &'static [u16],
    ) -> FakeTransport {
        let transport = FakeTransport::new(FakeTransportConfig {
            device_type: DeviceType::Network,
            offered_features: VirtioFeatures::VERSION_1.bits() | offered_features,
            queue_size: 8,
            supports_queue_reset: false,
            absent_queues,
        });
        transport.set_config_u16(CONFIG_MAX_VIRTQUEUE_PAIRS_OFFSET, advertised_pairs);
        transport
    }

    /// `max_virtqueue_pairs` is a ceiling the device advertises, not a
    /// promise. A driver that treats a missing pair as fatal turns a
    /// working single-pair NIC into a boot failure, which is what the
    /// x86 bench lane hit under a multi-queue tap (issue #91).
    #[test]
    fn a_device_advertising_more_pairs_than_it_presents_comes_up_on_the_pairs_it_has() {
        let transport = multiqueue_transport(
            super::NET_FEATURE_MQ | super::NET_FEATURE_CTRL_VQ,
            4,
            // Pair 1's receive queue is missing, so only pair 0 is
            // usable. The control queue still sits at 2 * 4.
            &[2],
        );

        let device = VirtioNetDevice::new(transport)
            .expect("a device with one usable pair is a device the driver can drive");

        assert_eq!(device.queue_pair_count(), 1);
    }

    /// The driver accepts `CTRL_VQ` before it can see whether the queue
    /// is there. Discovering it is not has to name the device's own
    /// disagreement, not surface later as a bare "operation is not
    /// supported" from the first command sent (issue #91).
    #[test]
    fn a_control_queue_the_device_does_not_present_is_named_at_bring_up() {
        // The driver only accepts `CTRL_VQ` when it has something to
        // say over it, so this is the multiqueue shape with the control
        // queue — at 2 * 4 — missing.
        let transport =
            multiqueue_transport(super::NET_FEATURE_MQ | super::NET_FEATURE_CTRL_VQ, 4, &[8]);

        let Err(error) = VirtioNetDevice::new(transport) else {
            panic!("a device that offers a queue it does not have is misconfigured");
        };

        assert!(
            matches!(error, IoError::InvalidDeviceConfig(detail) if detail.contains("control queue")),
            "the failure must name the control queue, got {error}"
        );
    }
}
