//! The handles a driver holds, as its own store sees them.
//!
//! A driver reaches its device through two handles: one for the device
//! itself and one per pinned buffer. Neither carries the thing it names
//! — the lease is owned by the instance's store, and so is every buffer
//! — because a handle that owned any of it would let a driver keep a
//! mapping alive past the reclaim that is supposed to take it away.
//! What a handle carries is the identity to check against, so a handle
//! that outlived a reclaim is refused rather than answered.

use super::grant::DeviceName;
use super::lease::DmaBuffer;

/// A driver's handle on the device its instance was granted.
///
/// An instance holds at most one device, so the handle is the
/// capability and not a selector: every operation on it goes to the one
/// lease its store owns, after checking that the lease is still for the
/// device the handle was opened on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantHandle {
    device: DeviceName,
}

impl GrantHandle {
    pub const fn new(device: DeviceName) -> Self {
        Self { device }
    }

    pub const fn device(&self) -> &DeviceName {
        &self.device
    }
}

/// A driver's handle on one buffer it pinned for its device.
///
/// The descriptor is a copy: the pin itself belongs to the lease and
/// goes away with it, so a handle outliving a reclaim describes memory
/// the driver can no longer reach rather than keeping it reachable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaBufferHandle {
    device: DeviceName,
    buffer: DmaBuffer,
}

impl DmaBufferHandle {
    pub const fn new(device: DeviceName, buffer: DmaBuffer) -> Self {
        Self { device, buffer }
    }

    pub const fn device(&self) -> &DeviceName {
        &self.device
    }

    pub const fn buffer(&self) -> DmaBuffer {
        self.buffer
    }
}
