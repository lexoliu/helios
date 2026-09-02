//! Shared virtio feature negotiation.
//!
//! Every helios virtio driver performs exactly the same status/feature
//! handshake before it programs its virtqueues. [`negotiate`] owns that
//! handshake and hands back a [`NegotiatedFeatures`] value the driver
//! stores and passes to [`crate::queue::VirtQueue::new`], so the ring
//! implementation and the driver always agree on what the device
//! accepted.
//!
//! Concurrency contract: negotiation runs once per device during
//! single-processor bring-up, before any queue is programmed and before
//! the device is allowed to raise an interrupt. It takes no locks.

use helios_hal::io::{IoError, IoResult};

use crate::transport::{DeviceStatus, VirtioFeatures, VirtioTransport};

/// Transport-level ring features every helios driver asks for.
///
/// Device-class bits (virtio-net checksum offload, virtio-blk read-only,
/// …) are added by the driver on top of this mask.
pub const RING_FEATURES: u64 = VirtioFeatures::VERSION_1.bits()
    | VirtioFeatures::RING_INDIRECT_DESC.bits()
    | VirtioFeatures::RING_EVENT_IDX.bits()
    | VirtioFeatures::RING_PACKED.bits()
    | VirtioFeatures::IN_ORDER.bits()
    | VirtioFeatures::NOTIFICATION_DATA.bits()
    | VirtioFeatures::RING_RESET.bits();

/// The feature bits a device accepted, as a typed view over the raw
/// driver feature word.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NegotiatedFeatures(u64);

impl NegotiatedFeatures {
    /// Builds a feature view directly from a driver feature word.
    ///
    /// Drivers get their value from [`negotiate`]; this entry point
    /// exists for tests that drive a queue without a device handshake.
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    /// The raw driver feature word.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// VIRTIO_F_RING_EVENT_IDX: both sides publish the index they want
    /// their next notification at.
    pub const fn event_idx(self) -> bool {
        self.contains(VirtioFeatures::RING_EVENT_IDX)
    }

    /// VIRTIO_F_INDIRECT_DESC: a chain may live in a driver-allocated
    /// descriptor table referenced by a single ring descriptor.
    pub const fn indirect(self) -> bool {
        self.contains(VirtioFeatures::RING_INDIRECT_DESC)
    }

    /// VIRTIO_F_RING_PACKED: the queue uses the packed ring layout.
    pub const fn packed(self) -> bool {
        self.contains(VirtioFeatures::RING_PACKED)
    }

    /// VIRTIO_F_IN_ORDER: the device uses buffers in the order the
    /// driver made them available, and may report a whole batch with a
    /// single used entry.
    pub const fn in_order(self) -> bool {
        self.contains(VirtioFeatures::IN_ORDER)
    }

    /// VIRTIO_F_NOTIFICATION_DATA: queue notifications carry the ring
    /// position the driver has published up to.
    pub const fn notification_data(self) -> bool {
        self.contains(VirtioFeatures::NOTIFICATION_DATA)
    }

    /// VIRTIO_F_RING_RESET: a single virtqueue can be reset and
    /// re-programmed without resetting the whole device.
    pub const fn ring_reset(self) -> bool {
        self.contains(VirtioFeatures::RING_RESET)
    }

    /// Whether every bit of a device-class mask was accepted.
    pub const fn device(self, mask: u64) -> bool {
        self.0 & mask == mask
    }

    const fn contains(self, feature: VirtioFeatures) -> bool {
        self.0 & feature.bits() != 0
    }
}

/// Runs the virtio device status and feature handshake.
///
/// The device is reset, told a driver is present, offered `wanted`
/// masked against what it advertises, and finally asked to confirm the
/// selection through `FEATURES_OK`. The caller programs its virtqueues
/// afterwards and sets `DRIVER_OK` itself.
pub fn negotiate<T: VirtioTransport>(transport: &T, wanted: u64) -> IoResult<NegotiatedFeatures> {
    negotiate_with(transport, |_| wanted)
}

/// Negotiation for drivers whose request depends on what the device
/// offers.
///
/// `select` is handed the offered feature word and returns the mask the
/// driver wants. It runs inside the handshake, after the device has been
/// reset and told a driver is present, so a driver never has to poke the
/// feature registers before the status protocol allows it (virtio 1.2
/// §3.1.1) just to decide what to ask for. virtio-net needs this: it may
/// only ask for multiqueue together with a control queue, and for TCP
/// segmentation only together with checksum offload.
pub fn negotiate_with<T, Select>(transport: &T, select: Select) -> IoResult<NegotiatedFeatures>
where
    T: VirtioTransport,
    Select: FnOnce(u64) -> u64,
{
    transport.reset();
    transport.set_status(DeviceStatus::ACKNOWLEDGE);
    transport.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER);

    let offered = transport.device_features();
    let mut accepted = offered & select(offered);
    if accepted & VirtioFeatures::VERSION_1.bits() == 0 {
        return Err(IoError::InvalidDeviceConfig(
            "virtio device does not support the 1.0 specification",
        ));
    }

    // VIRTIO_F_RING_RESET is only observable through a transport
    // register. The virtio-mmio register layout defines none, so a
    // device behind that transport must not be told the driver will
    // use the feature.
    if !transport.supports_queue_reset() {
        accepted &= !VirtioFeatures::RING_RESET.bits();
    }

    transport.set_driver_features(accepted);
    transport
        .set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER | DeviceStatus::FEATURES_OK);
    if !transport.status().contains(DeviceStatus::FEATURES_OK) {
        return Err(IoError::InvalidDeviceConfig(
            "virtio device rejected the negotiated feature set",
        ));
    }

    let features = NegotiatedFeatures(accepted);
    tracing::info!(
        device = ?transport.device_type(),
        offered = offered,
        accepted = accepted,
        ring = if features.packed() { "packed" } else { "split" },
        indirect = features.indirect(),
        event_idx = features.event_idx(),
        in_order = features.in_order(),
        notification_data = features.notification_data(),
        ring_reset = features.ring_reset(),
        "virtio features negotiated"
    );
    Ok(features)
}

#[cfg(test)]
mod tests {
    use super::{RING_FEATURES, negotiate};
    use crate::testing::{FakeTransport, FakeTransportConfig};
    use crate::transport::VirtioFeatures;

    #[test]
    fn negotiation_intersects_offered_and_wanted_features() {
        let transport = FakeTransport::new(FakeTransportConfig {
            offered_features: VirtioFeatures::VERSION_1.bits()
                | VirtioFeatures::RING_EVENT_IDX.bits()
                | VirtioFeatures::RING_PACKED.bits(),
            ..FakeTransportConfig::default()
        });

        let features = negotiate(&transport, RING_FEATURES).expect("negotiation should succeed");

        assert!(features.event_idx());
        assert!(features.packed());
        assert!(!features.indirect());
        assert!(!features.in_order());
    }

    #[test]
    fn ring_reset_is_dropped_when_the_transport_has_no_reset_register() {
        let transport = FakeTransport::new(FakeTransportConfig {
            offered_features: VirtioFeatures::VERSION_1.bits() | VirtioFeatures::RING_RESET.bits(),
            supports_queue_reset: false,
            ..FakeTransportConfig::default()
        });

        let features = negotiate(&transport, RING_FEATURES).expect("negotiation should succeed");

        assert!(!features.ring_reset());
        assert_eq!(
            transport.driver_features(),
            VirtioFeatures::VERSION_1.bits(),
            "the driver feature word must not claim a feature the transport cannot honour"
        );
    }

    #[test]
    fn a_device_without_version_1_is_rejected() {
        let transport = FakeTransport::new(FakeTransportConfig {
            offered_features: VirtioFeatures::RING_EVENT_IDX.bits(),
            ..FakeTransportConfig::default()
        });

        negotiate(&transport, RING_FEATURES).expect_err("a legacy-only device must be rejected");
    }
}
