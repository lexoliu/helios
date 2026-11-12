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
/// The entropy queue only ever carries a single writable buffer.
const REQUEST_CHAIN_LIMIT: u16 = 1;

pub struct VirtioRngDevice<T: VirtioTransport> {
    transport: T,
    queue: Mutex<VirtQueue<T>>,
    inflight: InFlight<{ REQUEST_QUEUE_SIZE as usize }>,
    interrupts: Notify,
    features: NegotiatedFeatures,
}

impl<T: VirtioTransport> VirtioRngDevice<T> {
    pub fn new(transport: T) -> IoResult<Self> {
        if transport.device_type() != DeviceType::Entropy {
            return Err(IoError::Unsupported);
        }

        let features = negotiate(&transport, RING_FEATURES)?;

        let queue_size = transport
            .queue_max_size(REQUEST_QUEUE_INDEX)
            .min(REQUEST_QUEUE_SIZE);
        if queue_size == 0 || !queue_size.is_power_of_two() {
            return Err(IoError::Unsupported);
        }

        let queue = VirtQueue::new(
            &transport,
            REQUEST_QUEUE_INDEX,
            queue_size,
            REQUEST_CHAIN_LIMIT,
            features,
        )?;
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
        })
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
    /// [`Self::quiesce`]. Entropy keeps no state outside its ring.
    pub fn reinitialize(&self) -> Result<DeviceReset, ReinitError> {
        conclude(&self.transport, self.restore(), |resumed| {
            self.inflight.resume(resumed);
        })
    }

    fn restore(&self) -> Result<DeviceReset, ReinitError> {
        renegotiate(&self.transport, self.features)?;
        reinitialize_requests(&self.queue, &self.transport, REQUEST_QUEUE_INDEX)?;
        driver_ok(&self.transport);
        Ok(DeviceReset {
            device: DeviceType::Entropy,
        })
    }

    pub async fn fill(&self, buffer: &mut [u8]) -> IoResult<()> {
        if buffer.is_empty() {
            return Ok(());
        }

        let mut filled = 0usize;
        while filled < buffer.len() {
            let len = self.fill_chunk(&mut buffer[filled..]).await?;
            if len == 0 {
                return Err(IoError::DeviceFault);
            }
            filled = filled.checked_add(len).ok_or(IoError::DeviceFault)?;
        }
        Ok(())
    }

    async fn fill_chunk(&self, buffer: &mut [u8]) -> IoResult<usize> {
        let capacity = buffer.len();
        let token = submit_chain(
            &self.inflight,
            &self.queue,
            &self.transport,
            &[],
            &mut [buffer],
        )
        .await?;

        let len = await_completion(&self.inflight, &self.queue, token, || {
            self.interrupts.notified()
        })
        .await?;
        let len = usize::try_from(len).map_err(|_| IoError::DeviceFault)?;
        if len > capacity {
            return Err(IoError::DeviceFault);
        }
        Ok(len)
    }
}

impl<T: VirtioTransport> Drop for VirtioRngDevice<T> {
    fn drop(&mut self) {
        self.queue.get_mut().shutdown(&self.transport);
    }
}

#[cfg(test)]
mod tests {
    use super::{DeviceReset, DeviceType, IoError, QuiesceError, ReinitError, VirtioRngDevice};
    use crate::features::RenegotiateError;
    use crate::testing::{FakeTransport, FakeTransportConfig};
    use crate::transport::VirtioFeatures;
    use core::pin::pin;
    use futures_lite::future::{block_on, poll_once};

    fn device() -> VirtioRngDevice<FakeTransport> {
        VirtioRngDevice::new(FakeTransport::new(FakeTransportConfig {
            device_type: DeviceType::Entropy,
            offered_features: VirtioFeatures::VERSION_1.bits(),
            queue_size: 8,
            supports_queue_reset: false,
            absent_queues: &[],
        }))
        .expect("entropy device should initialize")
    }

    /// Plays the device: publishes a used entry for `token` and raises
    /// the device's interrupt.
    fn complete(device: &VirtioRngDevice<FakeTransport>, token: u16, len: u32) {
        device
            .queue
            .try_lock()
            .expect("a parked driver does not hold the queue lock")
            .device_complete(token, len);
        device.handle_interrupt();
    }

    #[test]
    fn a_wrong_device_type_is_rejected() {
        let rejected = VirtioRngDevice::new(FakeTransport::new(FakeTransportConfig {
            device_type: DeviceType::Block,
            ..FakeTransportConfig::default()
        }))
        .err();
        assert_eq!(rejected, Some(IoError::Unsupported));
    }

    #[test]
    fn fill_resubmits_until_the_buffer_is_full() {
        let device = device();
        let mut buffer = [0_u8; 64];
        let mut fill = pin!(device.fill(&mut buffer));

        assert!(
            block_on(poll_once(fill.as_mut())).is_none(),
            "the first chunk is still with the device"
        );
        // A virtio entropy device may answer with fewer bytes than the
        // buffer holds; the driver has to ask again for the remainder.
        complete(&device, 0, 32);
        assert!(
            block_on(poll_once(fill.as_mut())).is_none(),
            "a short answer leaves half the buffer unfilled"
        );
        complete(&device, 1, 32);
        assert_eq!(block_on(poll_once(fill.as_mut())), Some(Ok(())));
    }

    #[test]
    fn completions_are_routed_by_descriptor_not_by_arrival_order() {
        let device = device();
        let mut first = [0_u8; 16];
        let mut second = [0_u8; 16];
        let mut fill_first = pin!(device.fill(&mut first));
        let mut fill_second = pin!(device.fill(&mut second));

        assert!(block_on(poll_once(fill_first.as_mut())).is_none());
        assert!(block_on(poll_once(fill_second.as_mut())).is_none());

        // The device answers the second request first. The waiter that
        // wakes drains the used ring for everyone, so the first request
        // must not take the completion that is not addressed to it.
        complete(&device, 1, 16);
        assert!(
            block_on(poll_once(fill_first.as_mut())).is_none(),
            "the first request is still outstanding"
        );
        assert_eq!(block_on(poll_once(fill_second.as_mut())), Some(Ok(())));

        complete(&device, 0, 16);
        assert_eq!(block_on(poll_once(fill_first.as_mut())), Some(Ok(())));
    }

    #[test]
    fn a_zero_length_completion_is_a_device_fault() {
        let device = device();
        let mut buffer = [0_u8; 32];
        let mut fill = pin!(device.fill(&mut buffer));

        assert!(block_on(poll_once(fill.as_mut())).is_none());
        complete(&device, 0, 0);
        assert_eq!(
            block_on(poll_once(fill.as_mut())),
            Some(Err(IoError::DeviceFault))
        );
    }

    #[test]
    fn a_completion_longer_than_the_buffer_is_a_device_fault() {
        let device = device();
        let mut buffer = [0_u8; 32];
        let mut fill = pin!(device.fill(&mut buffer));

        assert!(block_on(poll_once(fill.as_mut())).is_none());
        complete(&device, 0, 64);
        assert_eq!(
            block_on(poll_once(fill.as_mut())),
            Some(Err(IoError::DeviceFault))
        );
    }

    /// A device that offers a ring feature besides the mandatory one, so
    /// a test can withdraw it.
    fn device_with_event_idx() -> VirtioRngDevice<FakeTransport> {
        VirtioRngDevice::new(FakeTransport::new(FakeTransportConfig {
            device_type: DeviceType::Entropy,
            offered_features: VirtioFeatures::VERSION_1.bits()
                | VirtioFeatures::RING_EVENT_IDX.bits(),
            queue_size: 8,
            supports_queue_reset: false,
            absent_queues: &[],
        }))
        .expect("entropy device should initialize")
    }

    #[test]
    fn quiesce_waits_for_the_requests_the_device_owns() {
        let device = device();
        let mut buffer = [0_u8; 16];
        let mut fill = pin!(device.fill(&mut buffer));
        assert!(block_on(poll_once(fill.as_mut())).is_none());

        // The device answers while the quiesce spins.
        let mut passes = 0;
        device
            .quiesce(|| {
                passes += 1;
                complete(&device, 0, 16);
                false
            })
            .expect("the device answers inside the deadline");
        assert_eq!(passes, 1);

        assert_eq!(
            block_on(poll_once(fill.as_mut())),
            Some(Ok(())),
            "the completion reaped during the quiesce is still the waiter's"
        );
    }

    #[test]
    fn quiesce_gives_up_when_the_deadline_passes() {
        let device = device();
        let mut buffer = [0_u8; 16];
        let mut fill = pin!(device.fill(&mut buffer));
        assert!(block_on(poll_once(fill.as_mut())).is_none());

        assert_eq!(
            device.quiesce(|| true),
            Err(QuiesceError::RequestsStillInFlight {
                device: DeviceType::Entropy,
                queue: 0,
                count: 1,
            })
        );
    }

    #[test]
    fn a_reset_device_serves_requests_again_with_the_features_it_had() {
        let device = device();
        let mut first = [0_u8; 16];
        let mut fill = pin!(device.fill(&mut first));
        assert!(block_on(poll_once(fill.as_mut())).is_none());
        device
            .quiesce(|| {
                complete(&device, 0, 16);
                false
            })
            .expect("the device answers inside the deadline");

        device.transport.reset_behind_the_driver();
        let reset = device
            .reinitialize()
            .expect("the device offers what it did");
        assert_eq!(
            reset,
            DeviceReset {
                device: DeviceType::Entropy
            }
        );

        // The request that completed before the reset is collected after
        // it, and the ring starts again from its first descriptor.
        assert_eq!(block_on(poll_once(fill.as_mut())), Some(Ok(())));
        assert_eq!(device.transport.driver_features(), device.features().bits());
        assert_eq!(
            device.transport.driver_reset_count(),
            2,
            "bring-up and the re-initialisation each ran the handshake from a reset"
        );
        let programmed = device.transport.programmed_queues();
        assert_eq!(programmed.len(), 2);
        assert_eq!(programmed[0], programmed[1]);
        let mut second = [0_u8; 8];
        let mut fill = pin!(device.fill(&mut second));
        assert!(block_on(poll_once(fill.as_mut())).is_none());
        complete(&device, 0, 8);
        assert_eq!(block_on(poll_once(fill.as_mut())), Some(Ok(())));
    }

    #[test]
    fn a_device_that_offers_fewer_features_after_the_reset_is_refused() {
        let device = device_with_event_idx();
        assert!(device.features().event_idx());
        device.quiesce(|| false).expect("nothing is in flight");

        device
            .transport
            .withdraw_features(VirtioFeatures::RING_EVENT_IDX.bits());
        assert_eq!(
            device.reinitialize(),
            Err(ReinitError::Renegotiate(
                RenegotiateError::FeatureWithdrawn {
                    device: DeviceType::Entropy,
                    bit: 29,
                }
            ))
        );
    }

    #[test]
    fn an_empty_buffer_never_reaches_the_device() {
        let device = device();
        let mut buffer = [0_u8; 0];
        assert_eq!(block_on(device.fill(&mut buffer)), Ok(()));
        assert_eq!(device.transport.kick_count(), 0);
    }
}
