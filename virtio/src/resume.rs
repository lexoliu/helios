//! Stopping a device's requests and bringing it back after a reset.
//!
//! A guest resumed from a hibernate image finds every virtio device
//! reset: the device has lost its status, its features, its queues and
//! the little state the driver had given it outside the rings, while the
//! driver's ring memory, its callers and their parked requests are all
//! exactly as they were. Each driver therefore has two operations.
//!
//! * `quiesce` waits for every request the device owns on a *request*
//!   queue to complete. A request is something a caller is waiting on:
//!   it finishes by itself, and the completion is recorded where its
//!   waiter will find it. A *receive* queue (net receive, vsock receive,
//!   an event queue, a statistics queue) is not waited on, because its
//!   buffers are posted for the device to use whenever it has something
//!   to say, and may never complete.
//! * `reinitialize` re-runs the status handshake with the features the
//!   driver negotiated before, re-programs every queue at its existing
//!   memory with fresh indices, posts every receive pool again, and
//!   restores the device state that lives outside the rings. Whatever a
//!   receive buffer held when the image was taken is discarded: the
//!   device that wrote into it is gone.
//!
//! Concurrency contract, common to every driver:
//!
//! * Both run on one processor while the machine is frozen: no task is
//!   polled and no interrupt is delivered to a driver, so no request can
//!   be submitted and no completion can be reaped by anyone else while
//!   they run. A queue lock that a driver cannot take is therefore a
//!   task that held it across the freeze, which is a defect and reported
//!   as [`QuiesceError::QueueBusy`] or [`ReinitError::QueueBusy`] rather
//!   than waited for.
//! * `quiesce` reaps the used ring itself, on the calling processor,
//!   exactly as the drivers' interrupt and wait paths do, and records
//!   each completion where its waiter looks. The waiter's waker is woken
//!   at that moment, so a task that was parked on a request is runnable
//!   again by the time the machine thaws, and a request that completed
//!   during the freeze is never waited for again.
//! * Waiting for a completion is a spin on device memory, the
//!   hardware-synchronisation spin AGENTS.md allows. It carries a
//!   caller-supplied expiry, because this crate has no clock: the owner
//!   of the freeze knows its own deadline and says when it has passed.
//! * A `reinitialize` that fails leaves the device FAILED and every
//!   completion table of the driver faulted, so a caller that is parked
//!   or arrives after the thaw gets a `DeviceFault` and never waits for a
//!   device that was not brought back. A table serves requests again only
//!   when the whole re-initialisation succeeded.
//! * A restore command that outlived its deadline is still the device's,
//!   and the driver gave its buffers up. That device is lost: the driver
//!   latches it ([`Lost`]) and every later `reinitialize` answers
//!   [`ReinitError::DeviceLost`].
//! * `reinitialize` runs after every `quiesce`, before any task is
//!   polled again. It takes each queue lock with `try_lock` and holds it
//!   only while the queue is re-programmed.

use alloc::vec::Vec;
use core::alloc::Layout;
use core::sync::atomic::{AtomicBool, Ordering};

use async_lock::Mutex as AsyncMutex;
use helios_hal::dma::DmaMemory;
use helios_hal::io::IoError;
use helios_hal::iommu::DmaAccess;

use crate::bus::DeviceBus;
use crate::features::RenegotiateError;
use crate::queue::VirtQueue;
use crate::transport::{DeviceStatus, DeviceType, VirtioTransport};

/// The proof that a device was reset and rebuilt.
///
/// Every `reinitialize` returns one on success. What the device lost is
/// the device's, but what depends on it is somebody else's: a balloon's
/// owner must hand its inflated runs back, and the 9p client must drop a
/// session the server has forgotten. Those owners react to this value
/// instead of re-deriving the fact from a generation counter, so a
/// device the image did not reset (and which therefore was never
/// re-initialised) does not trigger them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "a reset device invalidates state its owner keeps elsewhere"]
pub struct DeviceReset {
    /// The device that was reset.
    pub device: DeviceType,
}

/// Why a device could not be brought to a stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum QuiesceError {
    /// The expiry passed with requests still with the device.
    #[error("virtio {device:?} queue {queue} still owns {count} requests")]
    RequestsStillInFlight {
        /// The device.
        device: DeviceType,
        /// The queue index that did not drain.
        queue: u16,
        /// Requests on it still with the device.
        count: usize,
    },
    /// A queue lock was held across the freeze, so its owner cannot have
    /// been stopped at a scheduler boundary.
    #[error("virtio {device:?} queue {queue} is locked by a task that was not stopped")]
    QueueBusy {
        /// The device.
        device: DeviceType,
        /// The queue index whose lock could not be taken.
        queue: u16,
    },
    /// The device broke the used-ring contract while its queue was
    /// drained.
    #[error("virtio {device:?} queue {queue} broke the used-ring contract")]
    DeviceFault {
        /// The device.
        device: DeviceType,
        /// The queue index that faulted.
        queue: u16,
    },
}

/// Why a device could not be brought back after a reset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReinitError {
    /// The status handshake did not go through again.
    #[error(transparent)]
    Renegotiate(#[from] RenegotiateError),
    /// The device no longer offers a queue as deep as the driver's ring.
    #[error(
        "virtio {device:?} queue {queue} was built {size} deep and the device now offers {offered}"
    )]
    QueueShrunk {
        /// The device.
        device: DeviceType,
        /// The queue index.
        queue: u16,
        /// The depth the driver's ring has.
        size: u16,
        /// The depth the device now offers.
        offered: u16,
    },
    /// A queue lock was held across the freeze.
    #[error("virtio {device:?} queue {queue} is locked by a task that was not stopped")]
    QueueBusy {
        /// The device.
        device: DeviceType,
        /// The queue index whose lock could not be taken.
        queue: u16,
    },
    /// The device is not the one the driver brought up: something it
    /// reported about itself differs. A device is untrusted, so what it
    /// reads back is compared, not assumed.
    #[error("virtio {device:?} changed under the driver: {what}")]
    DeviceChanged {
        /// The device.
        device: DeviceType,
        /// What differs.
        what: &'static str,
    },
    /// The vsock device now assigns the guest a different address, which
    /// every connection the image holds was made under.
    #[error("virtio-vsock guest CID was {expected} and is now {found}")]
    GuestCidChanged {
        /// The CID the driver brought up.
        expected: u64,
        /// The CID the device reports now.
        found: u64,
    },
    /// The driver holds device state that cannot be created again.
    #[error("virtio {device:?} holds {what}, which a reset device cannot be given back")]
    StateNotRestorable {
        /// The device.
        device: DeviceType,
        /// The state.
        what: &'static str,
    },
    /// An earlier re-initialisation left a command with the device and
    /// the deadline passed, so the device may still write into buffers
    /// the driver gave up. Nothing can be asked of it again.
    #[error("virtio {device:?} was lost by an earlier re-initialisation")]
    DeviceLost {
        /// The device.
        device: DeviceType,
    },
    /// The device did not answer a command that restores state it lost
    /// before the caller's deadline.
    #[error("virtio {device:?} did not answer the command that restores {what} in time")]
    RestoreTimedOut {
        /// The device.
        device: DeviceType,
        /// The state being restored.
        what: &'static str,
    },
    /// The device refused a command that restores state it lost.
    #[error("virtio {device:?} refused to restore {what}: {error}")]
    Restore {
        /// The device.
        device: DeviceType,
        /// The state being restored.
        what: &'static str,
        /// What the device answered.
        error: IoError,
    },
}

/// Spins until `step` reports that nothing is left in flight.
///
/// `step` does one pass of the driver's own reaping for `queue` and
/// answers how many requests the device still owns afterwards. `expired`
/// is asked between passes and, when it says the owner's deadline has
/// passed, the wait ends with the count that remains.
pub(crate) fn settle(
    device: DeviceType,
    queue: u16,
    expired: &mut impl FnMut() -> bool,
    mut step: impl FnMut() -> Result<usize, QuiesceError>,
) -> Result<(), QuiesceError> {
    loop {
        let count = step()?;
        if count == 0 {
            return Ok(());
        }
        if expired() {
            return Err(QuiesceError::RequestsStillInFlight {
                device,
                queue,
                count,
            });
        }
        // A request completes when the device model gets to it, which is
        // hardware progress and not a wait on software state.
        core::hint::spin_loop();
    }
}

/// Why one polled command did not produce an answer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PolledError {
    /// The caller's deadline passed with the command still the device's.
    Expired,
    /// The queue refused the command or the device faulted.
    Queue(IoError),
    /// A queue lock was held across the freeze.
    Busy,
}

/// Runs one command on a queue the caller owns exclusively, by polling
/// its used ring, for a machine that is frozen: no interrupt will be
/// delivered and no task will run, so nothing else could complete it.
///
/// The buffers are copies made here in the device's own memory, because
/// the device reads the command from them and writes the answer into
/// them. They are not the control slots of the queue's identifiers: a
/// quiesce may have reaped a completion its waiter has not collected yet,
/// and the slot of that identifier still holds the waiter's reply when the
/// ring hands the identifier out again. When `expired` ends the wait the
/// command is still the device's, so the buffers are leaked rather than
/// freed under a device that may yet write to them. Device memory that
/// refuses the copies fails the command: the machine is frozen, so
/// nothing would return memory to wait for. Returns the answer and the
/// number of bytes the device wrote.
pub(crate) fn polled_command<T: VirtioTransport>(
    transport: &T,
    queue: &AsyncMutex<VirtQueue<T>>,
    request: &[u8],
    response_bytes: usize,
    expired: &mut impl FnMut() -> bool,
) -> Result<(Vec<u8>, u32), PolledError> {
    let dma = transport.bus().dma();
    let copy = |bytes: usize, access: DmaAccess| {
        let layout = Layout::array::<u8>(bytes).map_err(|_| IoError::OutOfBounds)?;
        dma.allocate(layout, access).map_err(IoError::from)
    };
    let mut command = copy(request.len(), DmaAccess::READ).map_err(PolledError::Queue)?;
    command.as_mut()[..request.len()].copy_from_slice(request);
    // A queue that carries no reply (the gpu cursor queue) takes a chain
    // with no writable buffer.
    let mut response = if response_bytes == 0 {
        None
    } else {
        Some(copy(response_bytes, DmaAccess::WRITE).map_err(PolledError::Queue)?)
    };
    let token = {
        let mut ring = queue.try_lock().ok_or(PolledError::Busy)?;
        let inputs = [&command.as_ref()[..request.len()]];
        let token = match response.as_mut() {
            None => ring.submit(transport, &inputs, &mut []),
            Some(response) => ring.submit(
                transport,
                &inputs,
                &mut [&mut response.as_mut()[..response_bytes]],
            ),
        }
        .map_err(PolledError::Queue)?;
        ring.notify(transport);
        token
    };
    let written = loop {
        // The lock is taken for one pop and released before `expired`
        // runs, so the caller's deadline check is free to look at the
        // queue.
        let popped = queue
            .try_lock()
            .ok_or(PolledError::Busy)?
            .pop_used_with_len()
            .map_err(PolledError::Queue)?;
        match popped {
            Some((completed, written)) if completed == token => break written,
            // Nothing else is in flight on a queue the caller owns
            // exclusively, so completing any other chain breaks the
            // used-ring contract. The device is untrusted, so that is a
            // fault it is refused for, not a kernel panic, and the
            // command may still be its own: the buffers are leaked as on
            // expiry.
            Some(_) => {
                core::mem::forget(command);
                core::mem::forget(response);
                return Err(PolledError::Queue(IoError::DeviceFault));
            }
            None if expired() => {
                core::mem::forget(command);
                core::mem::forget(response);
                return Err(PolledError::Expired);
            }
            None => core::hint::spin_loop(),
        }
    };
    transport.ack_interrupt();
    let answer = response.as_ref().map_or_else(Vec::new, |response| {
        response.as_ref()[..response_bytes].to_vec()
    });
    Ok((answer, written))
}

/// Tells a re-handshaken device that the driver is ready, once every
/// queue it uses has been programmed again.
pub(crate) fn driver_ok<T: VirtioTransport>(transport: &T) {
    transport.set_status(
        DeviceStatus::ACKNOWLEDGE
            | DeviceStatus::DRIVER
            | DeviceStatus::FEATURES_OK
            | DeviceStatus::DRIVER_OK,
    );
}

/// How a re-initialisation ended, for the completion tables that front the
/// device's queues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Resumed {
    /// The device is back and its queues are programmed again.
    Back,
    /// The device was not brought back.
    Failed,
}

/// Finishes a re-initialisation: on success lets `tables` serve requests
/// again, and on failure marks the device FAILED and has `tables` fail
/// every request, so nobody parks on a device nobody drives.
///
/// `tables` is called once with the outcome, for every completion table
/// of the driver.
pub(crate) fn conclude<T: VirtioTransport, R>(
    transport: &T,
    outcome: Result<R, ReinitError>,
    tables: impl FnOnce(Resumed),
) -> Result<R, ReinitError> {
    match outcome {
        Ok(value) => {
            tables(Resumed::Back);
            Ok(value)
        }
        Err(error) => {
            transport.set_status(transport.status() | DeviceStatus::FAILED);
            tables(Resumed::Failed);
            Err(error)
        }
    }
}

/// Whether a driver's device was lost by a restore command that timed out.
///
/// The timed-out command is still counted in its queue, so the queue
/// cannot be programmed again, and the buffers it carried were leaked to
/// a device that may yet write them. Both are permanent, so the state is
/// a terminal one the driver checks on entry rather than a condition a
/// retry might clear. Written only by `reinitialize`, which runs on one
/// processor while the machine is frozen.
#[derive(Debug, Default)]
pub(crate) struct Lost(AtomicBool);

impl Lost {
    pub(crate) const fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    /// Refuses a device an earlier re-initialisation lost.
    pub(crate) fn refuse(&self, device: DeviceType) -> Result<(), ReinitError> {
        if self.0.load(Ordering::Acquire) {
            return Err(ReinitError::DeviceLost { device });
        }
        Ok(())
    }

    /// Passes `outcome` through, latching the loss when it is a restore
    /// command that timed out.
    pub(crate) fn observe<R>(&self, outcome: Result<R, ReinitError>) -> Result<R, ReinitError> {
        if matches!(outcome, Err(ReinitError::RestoreTimedOut { .. })) {
            self.0.store(true, Ordering::Release);
        }
        outcome
    }
}
