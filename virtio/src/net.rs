use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use async_lock::{Mutex, MutexGuard};
use core::future::Future;
use core::mem::size_of;

use helios_hal::io::{IoError, IoResult};

use crate::notify::Notify;
use crate::queue::VirtQueue;
use crate::transport::{DeviceStatus, DeviceType, VirtioFeatures, VirtioTransport};

const RX_QUEUE_INDEX: u16 = 0;
const TX_QUEUE_INDEX: u16 = 1;
const NET_QUEUE_SIZE: u16 = 256;
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
    rx_buffers: Box<[Option<Box<[u8]>>]>,
}

struct NetTxState<T: VirtioTransport> {
    tx_queue: VirtQueue<T>,
    tx_buffers: Box<[Box<[u8]>]>,
    tx_in_flight: Box<[bool]>,
}

pub struct VirtioNetDevice<T: VirtioTransport> {
    transport: T,
    rx_state: Mutex<NetRxState<T>>,
    tx_state: Mutex<NetTxState<T>>,
    tx_gate: Mutex<()>,
    interrupts: Notify,
    mac_address: [u8; 6],
    max_frame_len: usize,
    header_len: usize,
}

pub struct BorrowedRxFrame<'a, T: VirtioTransport> {
    state: MutexGuard<'a, NetRxState<T>>,
    buffer: Option<Box<[u8]>>,
    frame_start: usize,
    frame_end: usize,
}

impl<T: VirtioTransport> AsRef<[u8]> for BorrowedRxFrame<'_, T> {
    fn as_ref(&self) -> &[u8] {
        let buffer = self
            .buffer
            .as_ref()
            .expect("borrowed virtio RX frame was already reposted");
        &buffer[self.frame_start..self.frame_end]
    }
}

impl<T: VirtioTransport> Drop for BorrowedRxFrame<'_, T> {
    fn drop(&mut self) {
        assert!(
            self.buffer.is_none(),
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
        let mut rx_buffers = vec![None; usize::from(rx_queue_size)].into_boxed_slice();
        for _ in 0..usize::from(rx_queue_size) {
            let mut buffer = vec![0_u8; rx_buffer_len].into_boxed_slice();
            let token = rx_queue.submit(&transport, &[], &mut [buffer.as_mut()])?;
            assert!(
                rx_buffers[usize::from(token)].is_none(),
                "virtio net RX token was allocated twice during initialization"
            );
            rx_buffers[usize::from(token)] = Some(buffer);
        }
        let tx_buffers = (0..usize::from(tx_queue_size))
            .map(|_| vec![0_u8; tx_buffer_len].into_boxed_slice())
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let tx_in_flight = vec![false; usize::from(tx_queue_size)].into_boxed_slice();

        transport.set_status(
            DeviceStatus::ACKNOWLEDGE
                | DeviceStatus::DRIVER
                | DeviceStatus::FEATURES_OK
                | DeviceStatus::DRIVER_OK,
        );
        rx_queue.notify(&transport);

        Ok(Self {
            transport,
            rx_state: Mutex::new(NetRxState {
                rx_queue,
                rx_buffers,
            }),
            tx_state: Mutex::new(NetTxState {
                tx_queue,
                tx_buffers,
                tx_in_flight,
            }),
            tx_gate: Mutex::new(()),
            interrupts: Notify::new(),
            mac_address,
            max_frame_len,
            header_len,
        })
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

    pub async fn next_frame(&self) -> IoResult<Box<[u8]>> {
        loop {
            if let Some(frame) = self.try_receive().await? {
                return Ok(frame);
            }
            self.interrupts.notified().await;
        }
    }

    pub async fn try_receive(&self) -> IoResult<Option<Box<[u8]>>> {
        let mut state = self.rx_state.lock().await;
        let Some((token, used_len)) = state.rx_queue.pop_used_with_len() else {
            return Ok(None);
        };
        let Some(buffer) = state.rx_buffers[usize::from(token)].take() else {
            panic!("virtio net RX completion referenced an unknown token {token}");
        };
        let used_len = used_len as usize;
        if used_len < self.header_len || used_len > buffer.len() {
            Self::repost_rx_buffer(&mut state, &self.transport, buffer)?;
            return Err(IoError::DeviceFault);
        }

        let frame = buffer[self.header_len..used_len]
            .to_vec()
            .into_boxed_slice();
        Self::repost_rx_buffer(&mut state, &self.transport, buffer)?;
        Ok(Some(frame))
    }

    pub async fn try_receive_into(&self, output: &mut [u8]) -> IoResult<Option<usize>> {
        let mut state = self.rx_state.lock().await;
        let Some((token, used_len)) = state.rx_queue.pop_used_with_len() else {
            return Ok(None);
        };
        let Some(buffer) = state.rx_buffers[usize::from(token)].take() else {
            panic!("virtio net RX completion referenced an unknown token {token}");
        };
        let used_len = used_len as usize;
        if used_len < self.header_len || used_len > buffer.len() {
            Self::repost_rx_buffer(&mut state, &self.transport, buffer)?;
            return Err(IoError::DeviceFault);
        }

        let frame_len = used_len - self.header_len;
        if frame_len > output.len() {
            Self::repost_rx_buffer(&mut state, &self.transport, buffer)?;
            return Err(IoError::OutOfBounds);
        }
        output[..frame_len].copy_from_slice(&buffer[self.header_len..used_len]);
        Self::repost_rx_buffer(&mut state, &self.transport, buffer)?;
        Ok(Some(frame_len))
    }

    pub async fn try_receive_frame(&self) -> IoResult<Option<BorrowedRxFrame<'_, T>>> {
        let mut state = self.rx_state.lock().await;
        let Some((token, used_len)) = state.rx_queue.pop_used_with_len() else {
            return Ok(None);
        };
        let Some(buffer) = state.rx_buffers[usize::from(token)].take() else {
            panic!("virtio net RX completion referenced an unknown token {token}");
        };
        let used_len = used_len as usize;
        if used_len < self.header_len || used_len > buffer.len() {
            Self::repost_rx_buffer(&mut state, &self.transport, buffer)?;
            return Err(IoError::DeviceFault);
        }

        Ok(Some(BorrowedRxFrame {
            state,
            buffer: Some(buffer),
            frame_start: self.header_len,
            frame_end: used_len,
        }))
    }

    pub async fn repost_rx_frame(&self, mut frame: BorrowedRxFrame<'_, T>) -> IoResult<()> {
        let buffer = frame
            .buffer
            .take()
            .expect("borrowed virtio RX frame was reposted twice");
        Self::repost_rx_buffer(&mut frame.state, &self.transport, buffer)
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
        mut wait: Wait,
    ) -> IoResult<()>
    where
        Wait: FnMut() -> Fut,
        Fut: Future<Output = ()>,
    {
        for frame in frames {
            if frame.is_empty() || frame.len() > self.max_frame_len {
                return Err(IoError::InvalidBufferLength {
                    required_multiple: 1,
                    actual: frame.len(),
                });
            }
        }

        let _tx_turn = self.tx_gate.lock().await;
        let mut next_frame = 0usize;
        while next_frame < frames.len() {
            let mut submitted = 0usize;
            {
                let mut state = self.tx_state.lock().await;
                Self::drain_tx_completions(&mut state, usize::MAX);
                let NetTxState {
                    tx_queue,
                    tx_buffers,
                    tx_in_flight,
                } = &mut *state;
                while next_frame < frames.len() && tx_queue.available_descriptors() != 0 {
                    let token = tx_queue.next_free_descriptor();
                    let token_index = usize::from(token);
                    assert!(
                        !tx_in_flight[token_index],
                        "virtio net TX descriptor {token} is still in flight"
                    );
                    let payload_len = write_tx_payload(
                        &mut tx_buffers[token_index],
                        self.header_len,
                        frames[next_frame],
                    )?;
                    let payload = &tx_buffers[token_index][..payload_len];
                    let submitted_token = tx_queue.submit(&self.transport, &[payload], &mut [])?;
                    assert_eq!(
                        submitted_token, token,
                        "virtio net TX descriptor allocation moved while payload was prepared"
                    );
                    tx_in_flight[token_index] = true;
                    submitted += 1;
                    next_frame += 1;
                }
                if submitted != 0 {
                    tx_queue.notify(&self.transport);
                }
            }

            if submitted != 0 {
                continue;
            }

            let should_wait = {
                let mut state = self.tx_state.lock().await;
                Self::drain_tx_completions(&mut state, usize::MAX);
                state.tx_queue.available_descriptors() == 0
            };
            if should_wait {
                wait().await;
            }
        }
        Ok(())
    }

    pub async fn reclaim_transmit_completions(&self, budget: usize) -> IoResult<usize> {
        let mut state = self.tx_state.lock().await;
        Ok(Self::drain_tx_completions(&mut state, budget))
    }

    pub async fn wait_for_interrupt(&self) {
        self.interrupts.notified().await;
    }

    fn repost_rx_buffer(
        state: &mut NetRxState<T>,
        transport: &T,
        mut buffer: Box<[u8]>,
    ) -> IoResult<()> {
        let token = state
            .rx_queue
            .submit(transport, &[], &mut [buffer.as_mut()])?;
        assert!(
            state.rx_buffers[usize::from(token)].is_none(),
            "virtio net RX buffer was reposted into an occupied token slot"
        );
        state.rx_buffers[usize::from(token)] = Some(buffer);
        Ok(())
    }

    fn drain_tx_completions(state: &mut NetTxState<T>, budget: usize) -> usize {
        let mut completed = 0usize;
        while completed < budget {
            let Some(token) = state.tx_queue.pop_used() else {
                break;
            };
            let token_index = usize::from(token);
            assert!(
                state.tx_in_flight[token_index],
                "virtio net TX completion referenced idle descriptor {token}"
            );
            state.tx_in_flight[token_index] = false;
            completed += 1;
        }
        completed
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
