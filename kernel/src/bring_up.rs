//! Driving a device's future to completion during bring-up, before the
//! executor exists.
//!
//! This is one of the `block_on` sites AGENTS.md §4 allows: a bootstrap
//! entry point that runs on the bootstrap processor before the executor
//! starts, with nothing else to yield to. It is only for a future that
//! waits on nothing but a device and reads the device again on every
//! poll, as every measured-boot operation does (`hal::measured_boot`),
//! so the spin waits on a device register rather than on software
//! state, and each wait is bounded by the device's own timeout.

use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

/// Polls `future` until it completes.
///
/// `future` must wait on nothing but a device it reads again on every
/// poll; one that parks on a waker would spin forever here.
pub fn complete_device_future_at_bring_up<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => core::hint::spin_loop(),
        }
    }
}
