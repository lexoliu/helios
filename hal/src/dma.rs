//! Memory a bus master can reach.
//!
//! A device issues addresses in its own address space. These are the value
//! types and the contract a driver allocates device-visible memory through,
//! whatever stands between the device and RAM.

use core::alloc::Layout;

use thiserror::Error;

use crate::io::{IoError, IoResult};

/// An address one device issues.
///
/// Meaningful only in that device's address space: a physical address on a
/// direct platform, an I/O virtual address in its domain behind a
/// translation unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeviceAddress(u64);

impl DeviceAddress {
    pub const fn new(address: u64) -> Self {
        Self(address)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// What kind of address a [`DmaMemory`] hands to a device.
///
/// This decides whether a driver has to negotiate an access-platform
/// feature: a device that is given platform addresses must be told so, or
/// it would read its descriptor rings as physical addresses and translate
/// nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaAddressing {
    /// The device sees physical addresses.
    Physical,
    /// The device sees addresses the platform translates on its behalf.
    Platform,
}

/// Why a buffer cannot be given to a device.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum DmaError {
    /// A buffer outside every window the device can reach.
    #[error("{bytes} bytes at {address:#x} are not reachable by the device")]
    Unreachable {
        /// The kernel virtual address of the first byte of the refused
        /// buffer.
        address: usize,
        bytes: usize,
    },
}

impl From<DmaError> for IoError {
    fn from(error: DmaError) -> Self {
        match error {
            DmaError::Unreachable { .. } => Self::DeviceFault,
        }
    }
}

/// A buffer a device can reach, owned by the driver that allocated it.
pub trait DmaBuffer: Send + Sync + 'static {
    /// The address the device has to issue to reach the first byte.
    fn device_address(&self) -> DeviceAddress;
    fn as_ptr(&self) -> *mut u8;
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Memory a device reaches with no mapping request on the data path.
pub trait DmaMemory: Send + Sync + 'static {
    type Buffer: DmaBuffer;

    fn allocate_zeroed(&self, layout: Layout) -> IoResult<Self::Buffer>;

    /// The address a device has to issue to reach `bytes`.
    ///
    /// The whole slice has to be device-reachable and contiguous in the
    /// device's address space: a buffer that starts inside what the device
    /// can reach and runs past its end is refused, not translated by its
    /// first byte.
    fn device_range(&self, bytes: &[u8]) -> Result<DeviceAddress, DmaError>;

    /// The address a device has to issue to reach `bytes` of physical
    /// memory at `physical`, which this memory did not allocate.
    ///
    /// [`DmaMemory::device_range`] answers for a buffer the driver holds a
    /// slice of. This one answers for memory that is somebody else's and
    /// is named by its physical address alone, a guest's own pinned pages
    /// handed to the device by address instead of copied. A pool whose
    /// device sees physical addresses answers with the address it was
    /// given; one behind a translation answers with the address inside the
    /// device's domain, and refuses a run the domain does not map
    /// contiguously.
    fn foreign_range(&self, physical: u64, bytes: u64) -> IoResult<u64>;

    /// What kind of address this memory hands out.
    fn addressing(&self) -> DmaAddressing;
}
