use alloc::boxed::Box;
use alloc::vec;
use async_lock::Mutex as AsyncMutex;
use core::cell::UnsafeCell;
use core::future::Future;
use core::mem::size_of;

use helios_hal::io::{IoError, IoResult};
use spin::Mutex as SpinMutex;

use crate::notify::Notify;
use crate::queue::VirtQueue;
use crate::transport::{DeviceStatus, DeviceType, VirtioFeatures, VirtioTransport};

const RX_QUEUE_INDEX: u16 = 0;
const TX_QUEUE_INDEX: u16 = 1;
const NET_QUEUE_SIZE: u16 = 256;
const DESCRIPTOR_BITSET_WORDS: usize =
    (NET_QUEUE_SIZE as usize + usize::BITS as usize - 1) / usize::BITS as usize;
const ETH_HEADER_LEN: usize = 14;
const DEFAULT_IP_MTU: usize = 1500;
const NET_FEATURE_MAC: u64 = 1 << 5;
const NET_FEATURE_STATUS: u64 = 1 << 16;
const NET_FEATURE_MTU: u64 = 1 << 3;

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

struct DescriptorBitSet {
    words: [usize; DESCRIPTOR_BITSET_WORDS],
    bits: usize,
}

pub struct VirtioNetDevice<T: VirtioTransport> {
    transport: T,
    rx_state: AsyncMutex<NetRxState<T>>,
    rx_buffers: UnsafeCell<Box<[u8]>>,
    rx_buffer_len: usize,
    tx_state: SpinMutex<NetTxState<T>>,
    interrupts: Notify,
    mac_address: [u8; 6],
    max_frame_len: usize,
    header_len: usize,
}

pub struct BorrowedRxFrame<'a, T: VirtioTransport> {
    device: &'a VirtioNetDevice<T>,
    token: u16,
    frame_start: usize,
    frame_end: usize,
    reposted: bool,
}

// SAFETY: RX buffer mutation is tied to completed descriptor tokens and
// reposted under `rx_state`; TX mutation stays behind `tx_state`.
unsafe impl<T: VirtioTransport> Sync for VirtioNetDevice<T> {}

impl<T: VirtioTransport> AsRef<[u8]> for BorrowedRxFrame<'_, T> {
    fn as_ref(&self) -> &[u8] {
        assert!(
            !self.reposted,
            "borrowed virtio RX frame was already reposted"
        );
        let token_index = usize::from(self.token);
        &self.device.rx_slot(token_index, self.frame_end)[self.frame_start..self.frame_end]
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
        let accepted = offered
            & (VirtioFeatures::VERSION_1.bits()
                | NET_FEATURE_MAC
                | NET_FEATURE_STATUS
                | NET_FEATURE_MTU);
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

        let rx_queue_size = transport.queue_max_size(RX_QUEUE_INDEX).min(NET_QUEUE_SIZE);
        let tx_queue_size = transport.queue_max_size(TX_QUEUE_INDEX).min(NET_QUEUE_SIZE);
        if rx_queue_size == 0
            || tx_queue_size == 0
            || !rx_queue_size.is_power_of_two()
            || !tx_queue_size.is_power_of_two()
        {
            return Err(IoError::Unsupported);
        }

        let mut rx_queue = VirtQueue::new(&transport, RX_QUEUE_INDEX, rx_queue_size)?;
        let tx_queue = VirtQueue::new(&transport, TX_QUEUE_INDEX, tx_queue_size)?;
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

        transport.set_status(
            DeviceStatus::ACKNOWLEDGE
                | DeviceStatus::DRIVER
                | DeviceStatus::FEATURES_OK
                | DeviceStatus::DRIVER_OK,
        );
        rx_queue.notify(&transport);

        Ok(Self {
            transport,
            rx_state: AsyncMutex::new(NetRxState {
                rx_queue,
                rx_in_device,
            }),
            rx_buffers: UnsafeCell::new(rx_buffers),
            rx_buffer_len,
            tx_state: SpinMutex::new(NetTxState {
                tx_queue,
                tx_buffers,
                tx_buffer_len,
                tx_in_flight,
            }),
            interrupts: Notify::new(),
            mac_address,
            max_frame_len,
            header_len,
        })
    }

    pub fn handle_interrupt(&self) {
        self.transport.ack_interrupt();
        // AArch64/HVF TCP reports consistently point at network TX/RX pump
        // work, not at WIT syscall entry, as the IO-bound gap. Treat virtio
        // interrupts as readiness edges and let the pump drain batches after
        // waking; counting every interrupt just turns interrupt storms into
        // stale permits that keep the cooperative executor spinning.
        self.interrupts.notify_readiness();
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac_address
    }

    pub fn max_frame_len(&self) -> usize {
        self.max_frame_len
    }

    pub async fn try_receive_into(&self, output: &mut [u8]) -> IoResult<Option<usize>> {
        let mut state = self.rx_state.lock().await;
        let Some((token, used_len)) = state.rx_queue.pop_used_with_len() else {
            return Ok(None);
        };
        let token_index = usize::from(token);
        Self::mark_rx_completed(&mut state, token);
        let used_len = used_len as usize;
        if used_len < self.header_len || used_len > self.rx_buffer_len {
            self.repost_rx_buffer(&mut state, token)?;
            return Err(IoError::DeviceFault);
        }

        let frame_len = used_len - self.header_len;
        if frame_len > output.len() {
            self.repost_rx_buffer(&mut state, token)?;
            return Err(IoError::OutOfBounds);
        }
        output[..frame_len]
            .copy_from_slice(&self.rx_slot(token_index, used_len)[self.header_len..used_len]);
        self.repost_rx_buffer(&mut state, token)?;
        Ok(Some(frame_len))
    }

    pub async fn try_receive_frame(&self) -> IoResult<Option<BorrowedRxFrame<'_, T>>> {
        let mut state = self.rx_state.lock().await;
        let Some((token, used_len)) = state.rx_queue.pop_used_with_len() else {
            return Ok(None);
        };
        Self::mark_rx_completed(&mut state, token);
        let used_len = used_len as usize;
        if used_len < self.header_len || used_len > self.rx_buffer_len {
            self.repost_rx_buffer(&mut state, token)?;
            return Err(IoError::DeviceFault);
        }

        Ok(Some(BorrowedRxFrame {
            device: self,
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
        let Some(mut state) = self.rx_state.try_lock() else {
            return Ok(None);
        };
        let mut received = 0usize;
        for frame in frames {
            assert!(frame.is_none(), "virtio net RX batch slot was not empty");
            let Some((token, used_len)) = state.rx_queue.pop_used_with_len() else {
                break;
            };
            Self::mark_rx_completed(&mut state, token);
            let used_len = used_len as usize;
            if used_len < self.header_len || used_len > self.rx_buffer_len {
                self.repost_rx_buffer(&mut state, token)?;
                return Err(IoError::DeviceFault);
            }

            *frame = Some(BorrowedRxFrame {
                device: self,
                token,
                frame_start: self.header_len,
                frame_end: used_len,
                reposted: false,
            });
            received += 1;
        }
        Ok(Some(received))
    }

    pub async fn repost_rx_frame(&self, mut frame: BorrowedRxFrame<'_, T>) -> IoResult<()> {
        assert!(
            !frame.reposted,
            "borrowed virtio RX frame was reposted twice"
        );
        frame.reposted = true;
        let mut state = self.rx_state.lock().await;
        self.repost_rx_buffer(&mut state, frame.token)
    }

    pub fn repost_rx_frames_immediate<'a, 'slots>(
        &'a self,
        frames: &'slots mut [Option<BorrowedRxFrame<'a, T>>],
    ) -> IoResult<Option<()>>
    where
        'a: 'slots,
    {
        let Some(mut state) = self.rx_state.try_lock() else {
            return Ok(None);
        };
        for frame in frames {
            let Some(mut frame) = frame.take() else {
                continue;
            };
            assert!(
                !frame.reposted,
                "borrowed virtio RX frame was reposted twice"
            );
            frame.reposted = true;
            self.repost_rx_buffer(&mut state, frame.token)?;
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
        let mut state = self.tx_state.lock();
        Self::drain_tx_completions_when_full(&mut state, frames.len());
        let mut next_frame = 0usize;
        self.submit_available_tx_frames(&mut state, frames, &mut next_frame)
    }

    pub fn try_transmit_frames_immediate<Frame>(&self, frames: &[Frame]) -> IoResult<Option<usize>>
    where
        Frame: AsRef<[u8]>,
    {
        self.validate_tx_frames(frames)?;
        let Some(mut state) = self.tx_state.try_lock() else {
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
                let mut state = self.tx_state.lock();
                Self::drain_tx_completions_when_full(&mut state, frames.len() - next_frame);
                self.submit_available_tx_frames(&mut state, frames, &mut next_frame)?
            };

            if submitted != 0 {
                continue;
            }

            let should_wait = {
                let mut state = self.tx_state.lock();
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
        let mut state = self.tx_state.lock();
        Ok(Self::drain_tx_completions(&mut state, budget))
    }

    pub async fn wait_for_interrupt(&self) {
        self.interrupts.notified().await;
    }

    fn rx_slot(&self, token_index: usize, payload_len: usize) -> &[u8] {
        let buffers = unsafe { &*self.rx_buffers.get() };
        slot_buffer(buffers, self.rx_buffer_len, token_index, payload_len, "RX")
    }

    fn rx_slot_mut(&self, token_index: usize) -> &mut [u8] {
        let buffers = unsafe { &mut *self.rx_buffers.get() };
        slot_buffer_mut(buffers, self.rx_buffer_len, token_index, "RX")
    }

    fn repost_rx_buffer(&self, state: &mut NetRxState<T>, token: u16) -> IoResult<()> {
        let token_index = usize::from(token);
        assert!(
            !state.rx_in_device.get(token_index),
            "virtio net RX buffer was reposted while still owned by the device"
        );
        let submitted_token = state.rx_queue.submit_output_at(
            &self.transport,
            token,
            self.rx_slot_mut(token_index),
        )?;
        assert_eq!(
            submitted_token, token,
            "virtio net RX descriptor allocation moved while buffer was reposted"
        );
        state.rx_in_device.set(token_index);
        Ok(())
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

fn as_bytes<T>(value: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
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

    let header = VirtioNetHeader::default();
    buffer[..header_len].copy_from_slice(as_bytes(&header));
    buffer[header_len..payload_len].copy_from_slice(frame);
    Ok(payload_len)
}

#[cfg(test)]
mod tests {
    use super::DescriptorBitSet;

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
}
