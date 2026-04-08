use alloc::boxed::Box;

use helios_hal::io::{IoError, IoResult};
use spin::Mutex;

use crate::queue::VirtQueue;
use crate::transport::{DeviceStatus, DeviceType, VirtioFeatures, VirtioTransport};

const RECEIVE_QUEUE_INDEX: u16 = 0;
const TRANSMIT_QUEUE_INDEX: u16 = 1;
const CONSOLE_QUEUE_SIZE: u16 = 16;
const RECEIVE_BUFFER_SIZE: usize = 4096;
const TRANSMIT_CHUNK_SIZE: usize = 4096;

pub struct VirtioConsoleDevice<T: VirtioTransport> {
    transport: T,
    receive: Mutex<ReceiveState<T>>,
    transmit: Mutex<VirtQueue<T>>,
}

struct ReceiveState<T: VirtioTransport> {
    queue: VirtQueue<T>,
    buffer: Box<[u8]>,
    token: Option<u16>,
    available: usize,
    offset: usize,
}

impl<T: VirtioTransport> VirtioConsoleDevice<T> {
    pub fn new(transport: T) -> IoResult<Self> {
        if transport.device_type() != DeviceType::Console {
            return Err(IoError::Unsupported);
        }

        transport.reset();
        transport.set_status(DeviceStatus::ACKNOWLEDGE);
        transport.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER);

        let offered = transport.device_features();
        let accepted = offered & VirtioFeatures::VERSION_1.bits();
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
        )?);
        let transmit = VirtQueue::new(&transport, TRANSMIT_QUEUE_INDEX, transmit_size)?;
        transport.set_status(
            DeviceStatus::ACKNOWLEDGE
                | DeviceStatus::DRIVER
                | DeviceStatus::FEATURES_OK
                | DeviceStatus::DRIVER_OK,
        );

        let device = Self {
            transport,
            receive: Mutex::new(receive),
            transmit: Mutex::new(transmit),
        };
        device.prime_receive_queue()?;
        Ok(device)
    }

    pub fn try_read_byte(&self) -> Option<u8> {
        let mut receive = self.receive.lock();
        receive.try_read_byte(&self.transport)
    }

    pub fn write_bytes(&self, bytes: &[u8]) -> IoResult<()> {
        let mut queue = self.transmit.lock();
        for chunk in bytes.chunks(TRANSMIT_CHUNK_SIZE) {
            let mut outputs: [&mut [u8]; 0] = [];
            let token = queue.submit(&[chunk], &mut outputs)?;
            queue.notify(&self.transport);
            loop {
                if let Some((used, _len)) = queue.pop_used_with_len() {
                    assert_eq!(used, token, "virtio console transmit token mismatch");
                    break;
                }
                core::hint::spin_loop();
            }
        }
        Ok(())
    }

    fn prime_receive_queue(&self) -> IoResult<()> {
        let mut receive = self.receive.lock();
        receive.queue_buffer(&self.transport)
    }
}

impl<T: VirtioTransport> ReceiveState<T> {
    fn new(queue: VirtQueue<T>) -> Self {
        Self {
            queue,
            buffer: alloc::vec![0_u8; RECEIVE_BUFFER_SIZE].into_boxed_slice(),
            token: None,
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

        let (token, used_len) = self.queue.pop_used_with_len()?;
        let expected = self
            .token
            .take()
            .expect("virtio console receive completion arrived without a pending buffer");
        assert_eq!(token, expected, "virtio console receive token mismatch");

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

    fn queue_buffer(&mut self, transport: &T) -> IoResult<()> {
        if self.token.is_some() || self.available != 0 {
            return Ok(());
        }

        let mut outputs = [self.buffer.as_mut()];
        let token = self.queue.submit(&[], &mut outputs)?;
        self.queue.notify(transport);
        self.token = Some(token);
        Ok(())
    }
}
