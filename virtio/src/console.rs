use alloc::boxed::Box;

use async_lock::Mutex as AsyncMutex;
use helios_hal::io::{IoError, IoResult};
use spin::Mutex as SpinMutex;

use crate::features::{NegotiatedFeatures, RING_FEATURES, negotiate};
use crate::inflight::InFlight;
use crate::notify::Notify;
use crate::queue::VirtQueue;
use crate::transport::{DeviceStatus, DeviceType, VirtioTransport};

const RECEIVE_QUEUE_INDEX: u16 = 0;
const TRANSMIT_QUEUE_INDEX: u16 = 1;
const CONSOLE_QUEUE_SIZE: u16 = 16;
/// Both console queues carry exactly one buffer per request.
const CONSOLE_CHAIN_LIMIT: u16 = 1;
const RECEIVE_BUFFER_SIZE: usize = 4096;
const TRANSMIT_CHUNK_SIZE: usize = 4096;

pub struct VirtioConsoleDevice<T: VirtioTransport> {
    transport: T,
    receive: SpinMutex<ReceiveState<T>>,
    transmit: AsyncMutex<VirtQueue<T>>,
    transmit_inflight: InFlight<{ CONSOLE_QUEUE_SIZE as usize }>,
    interrupts: Notify,
    features: NegotiatedFeatures,
}

struct ReceiveState<T: VirtioTransport> {
    queue: VirtQueue<T>,
    buffer: Box<[u8]>,
    /// Whether the single receive buffer is currently with the device.
    posted: bool,
    available: usize,
    offset: usize,
}

impl<T: VirtioTransport> VirtioConsoleDevice<T> {
    pub fn new(transport: T) -> IoResult<Self> {
        if transport.device_type() != DeviceType::Console {
            return Err(IoError::Unsupported);
        }

        let features = negotiate(&transport, RING_FEATURES)?;

        let receive_size = transport
            .queue_max_size(RECEIVE_QUEUE_INDEX)
            .min(CONSOLE_QUEUE_SIZE);
        let transmit_size = transport
            .queue_max_size(TRANSMIT_QUEUE_INDEX)
            .min(CONSOLE_QUEUE_SIZE);
        if receive_size == 0
            || transmit_size == 0
            || !receive_size.is_power_of_two()
            || !transmit_size.is_power_of_two()
        {
            return Err(IoError::Unsupported);
        }

        let receive = ReceiveState::new(VirtQueue::new(
            &transport,
            RECEIVE_QUEUE_INDEX,
            receive_size,
            CONSOLE_CHAIN_LIMIT,
            features,
        )?);
        let transmit = VirtQueue::new(
            &transport,
            TRANSMIT_QUEUE_INDEX,
            transmit_size,
            CONSOLE_CHAIN_LIMIT,
            features,
        )?;
        transport.set_status(
            DeviceStatus::ACKNOWLEDGE
                | DeviceStatus::DRIVER
                | DeviceStatus::FEATURES_OK
                | DeviceStatus::DRIVER_OK,
        );

        let device = Self {
            transport,
            receive: SpinMutex::new(receive),
            transmit: AsyncMutex::new(transmit),
            transmit_inflight: InFlight::new(),
            interrupts: Notify::new(),
            features,
        };
        device.prime_receive_queue()?;
        Ok(device)
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

    pub fn try_read_byte(&self) -> Option<u8> {
        let mut receive = self.receive.lock();
        receive.try_read_byte(&self.transport)
    }

    pub async fn write_bytes(&self, bytes: &[u8]) -> IoResult<()> {
        for chunk in bytes.chunks(TRANSMIT_CHUNK_SIZE) {
            let token = {
                let mut queue = self.transmit.lock().await;
                let mut outputs: [&mut [u8]; 0] = [];
                let token = queue.submit(&self.transport, &[chunk], &mut outputs)?;
                queue.notify(&self.transport);
                self.transmit_inflight.register(token);
                token
            };

            loop {
                if self.transmit_inflight.take(token).is_some() {
                    break;
                }

                let drained = match self.transmit.try_lock() {
                    Some(mut queue) => queue.drain_used(|token, len| {
                        self.transmit_inflight.complete(token, len);
                    }),
                    None => 0,
                };
                if drained != 0 {
                    self.interrupts.notify_all();
                    continue;
                }

                self.interrupts.notified().await;
            }
        }
        Ok(())
    }

    fn prime_receive_queue(&self) -> IoResult<()> {
        let mut receive = self.receive.lock();
        receive.queue_buffer(&self.transport)
    }
}

impl<T: VirtioTransport> Drop for VirtioConsoleDevice<T> {
    fn drop(&mut self) {
        self.receive.get_mut().queue.shutdown(&self.transport);
        self.transmit.get_mut().shutdown(&self.transport);
    }
}

impl<T: VirtioTransport> ReceiveState<T> {
    fn new(queue: VirtQueue<T>) -> Self {
        Self {
            queue,
            buffer: alloc::vec![0_u8; RECEIVE_BUFFER_SIZE].into_boxed_slice(),
            posted: false,
            available: 0,
            offset: 0,
        }
    }

    fn try_read_byte(&mut self, transport: &T) -> Option<u8> {
        if self.offset < self.available {
            let byte = self.buffer[self.offset];
            self.offset += 1;
            if self.offset == self.available {
                self.available = 0;
                self.offset = 0;
                self.queue_buffer(transport).unwrap_or_else(|error| {
                    panic!("failed to re-arm virtio console receive queue: {error:?}")
                });
            }
            return Some(byte);
        }

        self.queue_buffer(transport).unwrap_or_else(|error| {
            panic!("failed to arm virtio console receive queue: {error:?}")
        });

        let used_len = self.take_completion()?;

        self.available = used_len as usize;
        assert!(
            self.available <= self.buffer.len(),
            "virtio console receive length {} exceeds buffer {}",
            self.available,
            self.buffer.len()
        );
        self.offset = 0;

        if self.available == 0 {
            self.queue_buffer(transport).unwrap_or_else(|error| {
                panic!("failed to requeue empty virtio console receive buffer: {error:?}")
            });
            return None;
        }

        let byte = self.buffer[0];
        self.offset = 1;
        if self.offset == self.available {
            self.available = 0;
            self.offset = 0;
            self.queue_buffer(transport).unwrap_or_else(|error| {
                panic!("failed to requeue single-byte virtio console receive buffer: {error:?}")
            });
        }
        Some(byte)
    }

    /// Reaps the receive buffer if the device has filled it.
    ///
    /// Exactly one buffer is ever outstanding, so the completion the
    /// device publishes identifies itself: the driver does not have to
    /// match a descriptor identifier against the one it submitted, which
    /// would be an assumption about completion order that EVENT_IDX and
    /// IN_ORDER both allow the device to break.
    fn take_completion(&mut self) -> Option<u32> {
        let mut completion = None;
        self.queue.drain_used(|_id, len| {
            assert!(
                completion.replace(len).is_none(),
                "virtio console completed more receive buffers than were posted"
            );
        });
        let len = completion?;
        self.posted = false;
        Some(len)
    }

    fn queue_buffer(&mut self, transport: &T) -> IoResult<()> {
        if self.posted || self.available != 0 {
            return Ok(());
        }

        let mut outputs = [self.buffer.as_mut()];
        self.queue.submit(transport, &[], &mut outputs)?;
        self.queue.notify(transport);
        self.posted = true;
        Ok(())
    }
}
