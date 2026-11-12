//! Memory a bus master can reach.
//!
//! A device issues addresses in its own address space. These are the value
//! types and the contract a driver allocates device-visible memory through,
//! whatever stands between the device and RAM.
//!
//! # Buffers a request owns
//!
//! A request that transfers data takes its buffers by value and hands them
//! back with its result ([`BufResult`]). It does not borrow them, because a
//! borrow ends when the request's future is dropped, and the device does
//! not stop when the future does: a guest that cancels a read, or an
//! instance killed in the middle of a host call, drops a future whose chain
//! the device is still writing. Had the buffer been borrowed, it would go
//! back to its pool or allocator at that moment and the device's late write
//! would land in whatever was allocated there next.
//!
//! So the device side owns a request's buffers from submission until the
//! device returns the chain:
//!
//! - A request that completes hands its buffers back to the caller with the
//!   result, error results included.
//! - A request whose future is dropped after submission leaves its buffers
//!   parked with the in-flight identifier of its chain. The device's late
//!   completion drops them, and only then may pooled memory be reused.
//! - A request whose device stopped answering (the queue faulted) keeps its
//!   buffers parked until the queue or the device is reset, and its caller
//!   gets the error without them ([`BufError::buffers`] is `None`).
//!
//! This is the completion-based I/O model: tokio-uring's
//! `IoBuf`/`IoBufMut` and `BufResult`, monoio and glommio take buffers by
//! value for exactly this cancellation hazard, and Linux blk-mq keeps a
//! request, and the bio it carries, owned by the driver until the device
//! completes it. Parking storage is fixed per queue and indexed by the
//! chain's head descriptor, so a request allocates nothing to be
//! cancellable.

use core::alloc::Layout;
use core::fmt;
use core::future::Future;

use thiserror::Error;

use crate::io::{IoError, IoResult};
use crate::iommu::DmaAccess;

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

/// A sample of a [`DmaMemory`]'s space signal.
///
/// The signal is a generation that only moves forward: a mark names the
/// generation a waiter saw, and [`DmaMemory::space_changed`] resolves once
/// the signal has been raised after it. The value means something only to
/// the memory that produced it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaSpaceMark(u64);

impl DmaSpaceMark {
    pub const fn new(generation: u64) -> Self {
        Self(generation)
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
    /// Nothing reachable is free now, and more is on its way: wait as
    /// [`DmaMemory::want_space`] says and ask again. (FreeBSD busdma's
    /// deferred load, `EINPROGRESS`.)
    #[error("the device's memory is being replenished; {bytes} bytes were not available")]
    Replenishing { bytes: usize },
    /// The device's memory cannot grow: the budget it grows under is
    /// spent, or the machine refused the memory. Not a device failure.
    #[error("the device's memory is exhausted; {bytes} bytes were refused")]
    Exhausted { bytes: usize },
    /// A run larger than one contiguous piece of the device's memory.
    #[error("{bytes} bytes exceed the {limit}-byte contiguous run the device's memory serves")]
    TooLarge { bytes: usize, limit: usize },
    /// A length the device cannot transfer: zero, or not a whole number
    /// of the `required_multiple` bytes one of its transfer units holds.
    #[error("length {bytes} is invalid; it must be a non-zero multiple of {required_multiple}")]
    InvalidLength {
        bytes: usize,
        required_multiple: usize,
    },
}

impl From<DmaError> for IoError {
    fn from(error: DmaError) -> Self {
        match error {
            DmaError::Unreachable { .. } => Self::DeviceFault,
            DmaError::Replenishing { .. } => Self::Replenishing,
            DmaError::Exhausted { .. } => Self::Exhausted,
            DmaError::TooLarge { .. } => Self::OutOfBounds,
            DmaError::InvalidLength {
                bytes,
                required_multiple,
            } => Self::InvalidBufferLength {
                required_multiple,
                actual: bytes,
            },
        }
    }
}

/// The outcome of a request that took its buffers by value.
///
/// A completed request hands `buffers` back in both arms. See the module
/// documentation for who owns them in between.
pub type BufResult<T, B, E = IoError> = Result<(T, B), BufError<B, E>>;

/// A request that failed, with its buffers when the device let them go.
pub struct BufError<B, E = IoError> {
    pub error: E,
    /// The buffers the request was given. `None` only when the device
    /// still holds them: the request was in flight when its queue stopped
    /// being driven, so they stay parked with the chain until the queue or
    /// the device is reset.
    pub buffers: Option<B>,
}

impl<B, E> BufError<B, E> {
    /// A failure the device never saw: the buffers come straight back.
    pub fn returned(error: E, buffers: B) -> Self {
        Self {
            error,
            buffers: Some(buffers),
        }
    }

    /// The error, letting go of the buffers.
    pub fn into_error(self) -> E {
        self.error
    }

    /// The same failure with its buffers rewrapped, for a layer that hands
    /// back a different shape than the one below it.
    pub fn map_buffers<C>(self, map: impl FnOnce(B) -> C) -> BufError<C, E> {
        BufError {
            error: self.error,
            buffers: self.buffers.map(map),
        }
    }

    /// The same failure with its error converted.
    pub fn map_error<F>(self, map: impl FnOnce(E) -> F) -> BufError<B, F> {
        BufError {
            error: map(self.error),
            buffers: self.buffers,
        }
    }
}

impl<B, E: fmt::Debug> fmt::Debug for BufError<B, E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BufError")
            .field("error", &self.error)
            .field("buffers_returned", &self.buffers.is_some())
            .finish()
    }
}

impl<B, E: fmt::Display> fmt::Display for BufError<B, E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

/// A buffer a device can reach, owned by the driver that allocated it.
///
/// The bytes are not inside the value: moving a buffer does not move the
/// memory it names, so a chain described from a buffer stays valid after
/// the buffer moves into the slot that parks it.
///
/// Dropping one never blocks, takes no lock, and never waits on another
/// processor: a driver's completion path, which may be an interrupt
/// handler on any processor, drops the buffers of requests nobody waits
/// on.
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
    /// A buffer the device reaches and the CPU reads and writes as bytes.
    ///
    /// The byte view is what lets a consumer fill or read a buffer it
    /// allocated at its origin without knowing which device it is for. The
    /// caller upholds the device-side half of the contract: it does not
    /// touch the bytes of a buffer while a request that names it is in
    /// flight.
    type Buffer: DmaBuffer + AsRef<[u8]> + AsMut<[u8]>;

    /// Zeroed memory for the life of the device: rings, control slots,
    /// receive and event buffers. It is never freed while the device
    /// lives, and `access` is what the device may do with it.
    fn allocate_resident(
        &self,
        layout: Layout,
        access: DmaAccess,
    ) -> Result<Self::Buffer, DmaError>;

    /// Zeroed memory a request carries and gives back: payloads, block
    /// pages, 9p messages. One run, contiguous in the device's address
    /// space, that the device may use as `access` says.
    ///
    /// A refusal that clears with time is [`DmaError::Replenishing`]; one
    /// that does not is [`DmaError::Exhausted`]. Both are waited out the
    /// same way: see [`DmaMemory::want_space`].
    fn allocate(&self, layout: Layout, access: DmaAccess) -> Result<Self::Buffer, DmaError>;

    /// [`DmaMemory::allocate`] without zeroing the memory: its bytes are
    /// unspecified.
    ///
    /// The caller, or the device it hands the run to, writes every byte
    /// before anything reads it. A run the caller fills before a device
    /// reads it, or that the device must write whole before the caller
    /// accepts it, is such a run; zeroing it first is a pass over the
    /// memory that cannot change an outcome. The refusals, the waiting
    /// and the contiguity are [`DmaMemory::allocate`]'s.
    ///
    /// # Concurrency
    ///
    /// As [`DmaMemory::allocate`].
    fn allocate_overwritten(
        &self,
        layout: Layout,
        access: DmaAccess,
    ) -> Result<Self::Buffer, DmaError>;

    /// The longest run [`DmaMemory::allocate`] can ever serve, in bytes:
    /// the largest contiguous piece the memory is made of. A request for
    /// more is refused with [`DmaError::TooLarge`] whatever else is free.
    /// A fact of the memory, not of any consumer; a driver publishes the
    /// transfers it carries below it.
    ///
    /// # Concurrency
    ///
    /// Any task on any processor may call this; the answer never changes
    /// while the memory lives.
    fn max_run_bytes(&self) -> usize;

    /// Declares that the caller was refused and will wait for space, and
    /// returns the signal's generation as of that declaration.
    ///
    /// # Waiting for space
    ///
    /// After [`DmaError::Replenishing`] or [`DmaError::Exhausted`] the
    /// caller:
    ///
    /// 1. calls `want_space`, which marks the memory starved, samples the
    ///    signal and orders both before whatever the caller does next;
    /// 2. asks for the allocation once more, which is the test the
    ///    declaration armed;
    /// 3. only if that is refused too, parks on
    ///    [`DmaMemory::space_changed`] with the mark from step 1.
    ///
    /// Memory freed, a chunk published or a growth refused after step 1
    /// has moved the generation past the mark, so the park returns at
    /// once; memory freed before step 1 is found by step 2. Neither can
    /// be slept through.
    ///
    /// # Concurrency
    ///
    /// Any task on any processor may call this. The signal is raised by
    /// whoever returns memory or ends a growth attempt, which includes an
    /// interrupt handler on any processor: raising it is non-blocking,
    /// takes no lock and allocates nothing. It is raised only while some
    /// caller is starved, so a pool nobody is waiting on pays no wake.
    /// A pool that never refuses has nothing to declare and returns the
    /// generation unchanged.
    fn want_space(&self) -> DmaSpaceMark;

    /// Resolves once memory was freed, a chunk published, or a growth
    /// refused after `mark`.
    ///
    /// Resolving says an allocation is worth asking for again, not that
    /// it will succeed: the answer may be [`DmaError::Exhausted`] where it
    /// was [`DmaError::Replenishing`], and another caller may take the
    /// space first. A pool that never refuses resolves at once.
    fn space_changed(&self, mark: DmaSpaceMark) -> impl Future<Output = ()> + Send + '_;

    /// Whether an allocation was refused and nothing has come back since:
    /// a writability hint for a caller that has not asked for memory
    /// itself. A pool that never refuses answers `false`.
    ///
    /// Only a hint, never a reservation: the next allocation is the
    /// authority.
    fn short_of_space(&self) -> bool;

    /// Samples the signal's generation without declaring a wait, for a
    /// readiness waiter that asks for no memory.
    ///
    /// Taken before the state the waiter inspects, exactly as the mark of
    /// [`DmaMemory::want_space`] is, and waited on with
    /// [`DmaMemory::space_changed`]. It does not mark the memory starved,
    /// so a free with no allocator waiting still raises nothing: pair it
    /// with [`DmaMemory::short_of_space`], which is set only by a refusal.
    fn space_mark(&self) -> DmaSpaceMark;

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
