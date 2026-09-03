//! Handing one device to a driver that does not live in the kernel.
//!
//! A [`DeviceGrant`] is everything a driver needs to drive one device
//! and nothing else: the register windows it may map, the interrupt
//! sources it may receive, and a budget of pinned memory it may point
//! the device's DMA engine at. The kernel builds it from what a backend
//! discovered, hands it to exactly one instance, and takes it all back
//! when that instance dies.
//!
//! # Why a grant rather than a handle per resource
//!
//! Because reclaim has to be complete. A driver that dies with its
//! register window still mapped, its interrupt still unmasked, or its
//! DMA still pinned leaves the kernel holding a device that is writing
//! into memory nobody owns. Bundling the resources means there is one
//! place that knows the whole set, and one operation —
//! [`DeviceGrant::reclaim`] — that returns all of it in an order that
//! is safe to perform on a device whose driver is already gone.
//!
//! # Reclaim order
//!
//! 1. **Quiesce.** Stop the device mastering the bus. This is the only
//!    step that actually makes the rest safe, and it is the one the
//!    kernel can perform without knowing what the device is — clearing
//!    the bus-master enable in PCI configuration space, or the
//!    equivalent for the transport.
//! 2. **Mask.** No further interrupts from a device with no driver.
//! 3. **Detach.** Take the endpoint out of its translation domain,
//!    where the platform has one, so any DMA still in flight faults
//!    instead of landing.
//! 4. **Unmap.** Take the register windows out of the dead instance's
//!    address space.
//! 5. **Unpin.** Return the DMA frames.
//!
//! Step 5 is conditional, and that is the honest part. On a platform
//! with an IOMMU, step 3 proves the device can no longer reach those
//! frames and they go straight back to the pool. Without one, the
//! kernel has quiesced the device but cannot prove the bus is idle, so
//! the frames are quarantined on the grant rather than handed to the
//! next allocation. A quarantined run is released by
//! [`DeviceGrant::release_quarantine`] once the owner has proven the
//! device is stopped — a bus reset, a power cycle, or simply never.
//! Returning them eagerly would be a use-after-free with a device on
//! the other end of it.

mod grant;

pub use grant::{
    DeviceGrant, DeviceGrantError, DeviceHost, DmaRegion, GrantReclaim, GuestWindow,
    MAX_DEVICE_DMA_REGIONS, MAX_DEVICE_INTERRUPTS, MAX_DEVICE_REGIONS, MappedRegion,
};
