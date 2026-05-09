use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use async_lock::Mutex as AsyncMutex;
use core::cell::UnsafeCell;
use core::future::Future;
use core::mem::size_of;

use helios_hal::io::{IoError, IoResult};
use spin::Mutex as SpinMutex;

use crate::notify::Notify;
use crate::queue::VirtQueue;
use crate::transport::{DeviceStatus, DeviceType, VirtioFeatures, VirtioTransport};

const NET_QUEUE_SIZE: u16 = 256;
const DESCRIPTOR_BITSET_WORDS: usize =
    (NET_QUEUE_SIZE as usize + usize::BITS as usize - 1) / usize::BITS as usize;
const ETH_HEADER_LEN: usize = 14;
const DEFAULT_IP_MTU: usize = 1500;
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
    rx_in_device: DescriptorBitSet,
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
    rx_buffers: UnsafeCell<Box<[u8]>>,
    tx_state: SpinMutex<NetTxState<T>>,
}

// SAFETY: each queue pair's RX buffer slab is mutated only behind
// `rx_state`'s async lock (RX completion handler) and read through
// `BorrowedRxFrame` whose lifetime keeps the device borrow alive
// until the consumer reposts the descriptor. TX mutation is gated
// by `tx_state`. Wrapping in `UnsafeCell` mirrors the
// pre-multi-queue layout so the existing safety argument carries
// over per pair.
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
    mac_address: [u8; 6],
    max_frame_len: usize,
    header_len: usize,
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

pub struct BorrowedRxFrame<'a, T: VirtioTransport> {
    device: &'a VirtioNetDevice<T>,
    /// Queue pair this frame came from. Reposting must hit the
    /// same pair so the descriptor returns to the device-side
    /// available ring it was popped from.
    pair_idx: u16,
    token: u16,
    frame_start: usize,
    frame_end: usize,
    reposted: bool,
}

// SAFETY: each `NetQueuePair`'s RX/TX state is independently
// synchronised by its own async / spin lock; `control` follows the
// same single-mutex discipline as the legacy single-queue
// `tx_state`. Crossing pair boundaries always happens via Box-slice
// indexing (no aliasing) so the device as a whole is `Sync`.
unsafe impl<T: VirtioTransport> Sync for VirtioNetDevice<T> {}

impl<T: VirtioTransport> AsRef<[u8]> for BorrowedRxFrame<'_, T> {
    fn as_ref(&self) -> &[u8] {
        assert!(
            !self.reposted,
            "borrowed virtio RX frame was already reposted"
        );
        let token_index = usize::from(self.token);
        let pair_idx = usize::from(self.pair_idx);
        &self.device.rx_slot(pair_idx, token_index, self.frame_end)
            [self.frame_start..self.frame_end]
    }
}

impl<T: VirtioTransport> Drop for BorrowedRxFrame<'_, T> {
    fn drop(&mut self) {
        assert!(
            self.reposted,
            "borrowed virtio RX frame was dropped without reposting"
        );
    }
}

impl<T: VirtioTransport> VirtioNetDevice<T> {
    pub fn new(transport: T) -> IoResult<Self> {
        if transport.device_type() != DeviceType::Network {
            return Err(IoError::Unsupported);
        }

        transport.reset();
        transport.set_status(DeviceStatus::ACKNOWLEDGE);
        transport.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER);

        let offered = transport.device_features();
        // Only ask for MQ together with CTRL_VQ — without the
        // control queue there is no way to enable additional pairs
        // beyond the default single pair.
        let mq_supported =
            offered & NET_FEATURE_MQ != 0 && offered & NET_FEATURE_CTRL_VQ != 0;
        let mq_mask = if mq_supported {
            NET_FEATURE_MQ | NET_FEATURE_CTRL_VQ
        } else {
            0
        };
        let accepted = offered
            & (VirtioFeatures::VERSION_1.bits()
                | NET_FEATURE_MAC
                | NET_FEATURE_STATUS
                | NET_FEATURE_MTU
                | mq_mask);
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

        let mac_address = read_mac_address(&transport);
        let ip_mtu = read_mtu(&transport, accepted);
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

        let pair_count = if accepted & NET_FEATURE_MQ != 0 {
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
            let rx_queue_size = transport
                .queue_max_size(rx_queue_index)
                .min(NET_QUEUE_SIZE);
            let tx_queue_size = transport
                .queue_max_size(tx_queue_index)
                .min(NET_QUEUE_SIZE);
            if rx_queue_size == 0
                || tx_queue_size == 0
                || !rx_queue_size.is_power_of_two()
                || !tx_queue_size.is_power_of_two()
            {
                return Err(IoError::Unsupported);
            }

            let mut rx_queue = VirtQueue::new(&transport, rx_queue_index, rx_queue_size)?;
            let tx_queue = VirtQueue::new(&transport, tx_queue_index, tx_queue_size)?;
            let rx_buffer_count = usize::from(rx_queue_size);
            let mut rx_buffers = vec![
                0_u8;
                rx_buffer_len
                    .checked_mul(rx_buffer_count)
                    .ok_or(IoError::DeviceFault)?
            ]
            .into_boxed_slice();
            let mut rx_in_device = DescriptorBitSet::new(rx_buffer_count);
            for _ in 0..usize::from(rx_queue_size) {
                let token = rx_queue.next_free_descriptor();
                let token_index = usize::from(token);
                let submitted_token = rx_queue.submit(
                    &transport,
                    &[],
                    &mut [slot_buffer_mut(
                        &mut rx_buffers,
                        rx_buffer_len,
                        token_index,
                        "RX",
                    )],
                )?;
                assert_eq!(
                    submitted_token, token,
                    "virtio net RX descriptor allocation moved while buffer was prepared"
                );
                assert!(
                    !rx_in_device.get(token_index),
                    "virtio net RX token was allocated twice during initialization"
                );
                rx_in_device.set(token_index);
            }
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
                }),
                rx_buffers: UnsafeCell::new(rx_buffers),
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
        let control = if accepted & NET_FEATURE_CTRL_VQ != 0 {
            let device_max = if accepted & NET_FEATURE_MQ != 0 {
                read_max_virtqueue_pairs(&transport).max(1)
            } else {
                1
            };
            let ctrl_index = device_max * 2;
            let ctrl_size = transport.queue_max_size(ctrl_index).min(NET_QUEUE_SIZE);
            if ctrl_size != 0 && ctrl_size.is_power_of_two() {
                let queue = VirtQueue::new(&transport, ctrl_index, ctrl_size)?;
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
            mac_address,
            max_frame_len,
            header_len,
        };

        // Activate every queue pair on the device side. The device
        // ships with a single pair active by default; without this
        // command extra RX queues we just allocated would not
        // receive traffic.
        if accepted & NET_FEATURE_MQ != 0 && device.queue_pair_count() > 1 {
            device.send_set_vq_pairs(device.queue_pair_count())?;
        }
        Ok(device)
    }

    /// Number of TX/RX queue pairs the device exposes. Always at
    /// least 1; greater than 1 only when both `VIRTIO_NET_F_MQ` and
    /// `VIRTIO_NET_F_CTRL_VQ` were negotiated and the device
    /// advertised multiple pairs.
    pub fn queue_pair_count(&self) -> usize {
        self.queue_pairs.len()
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
        let _token =
            queue.submit(&self.transport, &[cmd_slice], &mut [ack_slice])?;
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
        self.interrupts.notify_one();
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
        let Some((token, used_len)) = state.rx_queue.pop_used_with_len() else {
            return Ok(None);
        };
        let token_index = usize::from(token);
        Self::mark_rx_completed(&mut state, token);
        let used_len = used_len as usize;
        if used_len < self.header_len || used_len > self.rx_buffer_len {
            self.repost_rx_buffer(PAIR, &mut state, token)?;
            return Err(IoError::DeviceFault);
        }

        let frame_len = used_len - self.header_len;
        if frame_len > output.len() {
            self.repost_rx_buffer(PAIR, &mut state, token)?;
            return Err(IoError::OutOfBounds);
        }
        output[..frame_len].copy_from_slice(
            &self.rx_slot(PAIR, token_index, used_len)[self.header_len..used_len],
        );
        self.repost_rx_buffer(PAIR, &mut state, token)?;
        Ok(Some(frame_len))
    }

    pub async fn try_receive_frame(&self) -> IoResult<Option<BorrowedRxFrame<'_, T>>> {
        const PAIR: usize = 0;
        let mut state = self.queue_pairs[PAIR].rx_state.lock().await;
        let Some((token, used_len)) = state.rx_queue.pop_used_with_len() else {
            return Ok(None);
        };
        Self::mark_rx_completed(&mut state, token);
        let used_len = used_len as usize;
        if used_len < self.header_len || used_len > self.rx_buffer_len {
            self.repost_rx_buffer(PAIR, &mut state, token)?;
            return Err(IoError::DeviceFault);
        }

        Ok(Some(BorrowedRxFrame {
            device: self,
            pair_idx: PAIR as u16,
            token,
            frame_start: self.header_len,
            frame_end: used_len,
            reposted: false,
        }))
    }

    pub fn try_receive_frames_immediate<'a, 'slots>(
        &'a self,
        frames: &'slots mut [Option<BorrowedRxFrame<'a, T>>],
    ) -> IoResult<Option<usize>>
    where
        'a: 'slots,
    {
        // Drain across every queue pair so the device's RX hashing
        // never strands frames on a non-default pair. We iterate
        // pair 0 first (matches single-queue behaviour) then walk
        // additional pairs, locking one at a time and stopping when
        // the slot fills.
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
            while received < frames.len() {
                let Some((token, used_len)) = state.rx_queue.pop_used_with_len() else {
                    break;
                };
                Self::mark_rx_completed(&mut state, token);
                let used_len = used_len as usize;
                if used_len < self.header_len || used_len > self.rx_buffer_len {
                    self.repost_rx_buffer(pair_idx, &mut state, token)?;
                    return Err(IoError::DeviceFault);
                }

                let slot = &mut frames[received];
                assert!(slot.is_none(), "virtio net RX batch slot was not empty");
                *slot = Some(BorrowedRxFrame {
                    device: self,
                    pair_idx: pair_idx as u16,
                    token,
                    frame_start: self.header_len,
                    frame_end: used_len,
                    reposted: false,
                });
                received += 1;
            }
        }
        if !any_locked {
            return Ok(None);
        }
        Ok(Some(received))
    }

    pub async fn repost_rx_frame(&self, mut frame: BorrowedRxFrame<'_, T>) -> IoResult<()> {
        assert!(
            !frame.reposted,
            "borrowed virtio RX frame was reposted twice"
        );
        frame.reposted = true;
        let pair_idx = usize::from(frame.pair_idx);
        let mut state = self.queue_pairs[pair_idx].rx_state.lock().await;
        self.repost_rx_buffer(pair_idx, &mut state, frame.token)
    }

    pub fn repost_rx_frames_immediate<'a, 'slots>(
        &'a self,
        frames: &'slots mut [Option<BorrowedRxFrame<'a, T>>],
    ) -> IoResult<Option<()>>
    where
        'a: 'slots,
    {
        // Group reposts by their originating pair; lock each pair
        // once and walk its frames in reverse so consecutive
        // descriptors stay at the free head and publish in a single
        // avail-idx write. With multi-queue RX, frames from
        // different pairs are interleaved in the slot, so we make
        // one outer pass per pair index that appears.
        let mut any_locked = false;
        for pair_idx in 0..self.queue_pairs.len() {
            // Skip pairs that don't have any frames in the slot to
            // avoid an unnecessary try_lock.
            if !frames.iter().flatten().any(|f| usize::from(f.pair_idx) == pair_idx) {
                continue;
            }
            let Some(mut state) = self.queue_pairs[pair_idx].rx_state.try_lock() else {
                continue;
            };
            any_locked = true;
            let mut reposted = 0usize;
            for frame in frames.iter_mut().rev() {
                let pair_match = frame
                    .as_ref()
                    .map(|f| usize::from(f.pair_idx) == pair_idx)
                    .unwrap_or(false);
                if !pair_match {
                    continue;
                }
                let Some(mut owned) = frame.take() else {
                    continue;
                };
                assert!(
                    !owned.reposted,
                    "borrowed virtio RX frame was reposted twice"
                );
                owned.reposted = true;
                self.repost_rx_buffer_deferred(pair_idx, &mut state, owned.token)?;
                reposted += 1;
            }
            if reposted != 0 {
                state.rx_queue.publish_deferred_heads();
            }
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
        Frame: AsRef<[u8]>,
    {
        self.validate_tx_frames(frames)?;
        let mut state = self.queue_pairs[0].tx_state.lock();
        Self::drain_tx_completions_when_full(&mut state, frames.len());
        let mut next_frame = 0usize;
        self.submit_available_tx_frames(&mut state, frames, &mut next_frame)
    }

    pub fn try_transmit_frames_immediate<Frame>(&self, frames: &[Frame]) -> IoResult<Option<usize>>
    where
        Frame: AsRef<[u8]>,
    {
        self.validate_tx_frames(frames)?;
        self.try_transmit_valid_frames_immediate(frames)
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
        Frame: AsRef<[u8]>,
    {
        self.try_transmit_valid_frames_immediate(frames)
    }

    fn try_transmit_valid_frames_immediate<Frame>(
        &self,
        frames: &[Frame],
    ) -> IoResult<Option<usize>>
    where
        Frame: AsRef<[u8]>,
    {
        let Some(mut state) = self.queue_pairs[0].tx_state.try_lock() else {
            return Ok(None);
        };
        Self::drain_tx_completions_when_full(&mut state, frames.len());
        let mut next_frame = 0usize;
        self.submit_available_tx_frames(&mut state, frames, &mut next_frame)
            .map(Some)
    }

    pub async fn transmit_frames_with_wait<Frame, Wait, Fut>(
        &self,
        frames: &[Frame],
        mut wait: Wait,
    ) -> IoResult<()>
    where
        Frame: AsRef<[u8]>,
        Wait: FnMut() -> Fut,
        Fut: Future<Output = ()>,
    {
        self.validate_tx_frames(frames)?;

        let mut next_frame = 0usize;
        while next_frame < frames.len() {
            let submitted = {
                let mut state = self.queue_pairs[0].tx_state.lock();
                Self::drain_tx_completions_when_full(&mut state, frames.len() - next_frame);
                self.submit_available_tx_frames(&mut state, frames, &mut next_frame)?
            };

            if submitted != 0 {
                continue;
            }

            let should_wait = {
                let mut state = self.queue_pairs[0].tx_state.lock();
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
        Frame: AsRef<[u8]>,
    {
        for frame in frames {
            let frame = frame.as_ref();
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
        Frame: AsRef<[u8]>,
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
            let frame = frames[*next_frame].as_ref();
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
            )?;
            let payload = slot_buffer(tx_buffers, *tx_buffer_len, token_index, payload_len, "TX");
            let submitted_token = tx_queue.submit_read_only_deferred(&self.transport, payload)?;
            assert_eq!(
                submitted_token, token,
                "virtio net TX descriptor allocation moved while payload was prepared"
            );
            tx_in_flight.set(token_index);
            tx_queue.stage_deferred_head(token);
            submitted += 1;
            *next_frame += 1;
        }
        if submitted != 0 {
            tx_queue.publish_deferred_heads();
            tx_queue.notify(&self.transport);
        }
        Ok(submitted)
    }

    pub async fn reclaim_transmit_completions(&self, budget: usize) -> IoResult<usize> {
        let mut state = self.queue_pairs[0].tx_state.lock();
        Ok(Self::drain_tx_completions(&mut state, budget))
    }

    pub fn reclaim_transmit_completions_immediate(&self, budget: usize) -> IoResult<Option<usize>> {
        let Some(mut state) = self.queue_pairs[0].tx_state.try_lock() else {
            return Ok(None);
        };
        Ok(Some(Self::drain_tx_completions(&mut state, budget)))
    }

    pub async fn wait_for_interrupt(&self) {
        self.interrupts.notified().await;
    }

    fn rx_slot(&self, pair_idx: usize, token_index: usize, payload_len: usize) -> &[u8] {
        let buffers = unsafe { &*self.queue_pairs[pair_idx].rx_buffers.get() };
        slot_buffer(buffers, self.rx_buffer_len, token_index, payload_len, "RX")
    }

    fn rx_slot_mut(&self, pair_idx: usize, token_index: usize) -> &mut [u8] {
        let buffers = unsafe { &mut *self.queue_pairs[pair_idx].rx_buffers.get() };
        slot_buffer_mut(buffers, self.rx_buffer_len, token_index, "RX")
    }

    fn repost_rx_buffer(
        &self,
        pair_idx: usize,
        state: &mut NetRxState<T>,
        token: u16,
    ) -> IoResult<()> {
        let submitted_token = self.repost_rx_buffer_deferred(pair_idx, state, token)?;
        state.rx_queue.publish_deferred_heads();
        assert_eq!(
            submitted_token, token,
            "virtio net RX descriptor allocation moved while buffer was reposted"
        );
        Ok(())
    }

    fn repost_rx_buffer_deferred(
        &self,
        pair_idx: usize,
        state: &mut NetRxState<T>,
        token: u16,
    ) -> IoResult<u16> {
        let token_index = usize::from(token);
        assert!(
            !state.rx_in_device.get(token_index),
            "virtio net RX buffer was reposted while still owned by the device"
        );
        let submitted_token = state.rx_queue.submit_output_at_deferred(
            &self.transport,
            token,
            self.rx_slot_mut(pair_idx, token_index),
        )?;
        state.rx_in_device.set(token_index);
        Ok(submitted_token)
    }

    fn mark_rx_completed(state: &mut NetRxState<T>, token: u16) {
        let token_index = usize::from(token);
        assert!(
            state.rx_in_device.get(token_index),
            "virtio net RX completion referenced an idle token {token}"
        );
        state.rx_in_device.clear(token_index);
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

fn read_mtu<T: VirtioTransport>(transport: &T, accepted_features: u64) -> usize {
    if accepted_features & NET_FEATURE_MTU == 0 {
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

fn write_tx_payload(buffer: &mut [u8], header_len: usize, frame: &[u8]) -> IoResult<usize> {
    let payload_len = header_len
        .checked_add(frame.len())
        .ok_or(IoError::DeviceFault)?;
    if payload_len > buffer.len() {
        return Err(IoError::InvalidBufferLength {
            required_multiple: 1,
            actual: frame.len(),
        });
    }

    // TX slots are zero-initialized and the device only reads them, so the
    // virtio-net header remains the all-zero "no offload" header across reuse.
    buffer[header_len..payload_len].copy_from_slice(frame);
    Ok(payload_len)
}

#[cfg(test)]
mod tests {
    use core::mem::size_of;

    use super::{DescriptorBitSet, VirtioNetHeader, write_tx_payload};

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
    fn tx_payload_write_preserves_zero_net_header() {
        let expected_header = [0; size_of::<VirtioNetHeader>()];
        let header_len = expected_header.len();
        let mut buffer = [0u8; 64];
        let first = [1u8; 8];
        let second = [2u8; 4];

        assert_eq!(
            write_tx_payload(&mut buffer, header_len, &first).expect("first payload fits"),
            header_len + first.len()
        );
        assert_eq!(&buffer[..header_len], expected_header);
        assert_eq!(&buffer[header_len..header_len + first.len()], first);

        assert_eq!(
            write_tx_payload(&mut buffer, header_len, &second).expect("second payload fits"),
            header_len + second.len()
        );
        assert_eq!(&buffer[..header_len], expected_header);
        assert_eq!(&buffer[header_len..header_len + second.len()], second);
    }
}
