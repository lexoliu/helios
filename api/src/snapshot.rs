//! Releasing the machine's host share for a VMM snapshot.
//!
//! A VMM that snapshots the machine refuses while the 9p host share is
//! attached. [`prepare`] has the kernel give the share back and hold it
//! back; the hold ends when the machine restored from the snapshot
//! advances its generation, or, on the machine that keeps running,
//! through [`finished`].

use crate::bindings::helios::system::snapshot as raw;
pub use crate::bindings::helios::system::snapshot::Prepared;

/// Waits for the host-share operations in flight to finish, returns the
/// share to the host and holds it there. Operations issued from now
/// until the hold ends wait; none fails.
pub async fn prepare() -> Prepared {
    raw::prepare().await
}

/// Ends the hold [`prepare`] took, for the machine that keeps running
/// after the snapshot. A machine restored from the snapshot has its hold
/// ended by its generation advancing instead, and this is then a no-op.
pub fn finished() {
    raw::finished();
}
