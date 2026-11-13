//! Bounded, yielding waits on a device register.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;

use helios_hal::cpu::{Cpu, Instant, Timebase};

use crate::transport::TransportError;

/// The monotonic time a wait measures its timeout against.
///
/// Every [`Cpu`] is one; the trait exists so a test can drive time
/// without standing up a processor.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
    fn timebase(&self) -> Timebase;
}

impl<C: Cpu> Clock for C {
    fn now(&self) -> Instant {
        Cpu::now(self)
    }

    fn timebase(&self) -> Timebase {
        Cpu::timebase(self)
    }
}

/// Polls `ready` until it holds, yielding to the executor after every
/// miss, and fails with `state` once `timeout` has passed.
///
/// The time is read before the condition, so a timeout is reported only
/// after a test that started past the deadline still failed.
pub(crate) async fn wait_until<W, C: Clock>(
    window: &mut W,
    clock: &C,
    state: &'static str,
    timeout: Duration,
    mut ready: impl FnMut(&mut W) -> bool,
) -> Result<(), TransportError> {
    let timebase = clock.timebase();
    let deadline = clock
        .now()
        .saturating_add(timebase.duration_to_ticks(timeout));
    loop {
        let now = clock.now();
        if ready(window) {
            return Ok(());
        }
        if now >= deadline {
            return Err(TransportError::Timeout {
                state,
                waited_ms: timeout.as_millis() as u64,
            });
        }
        YieldNow { yielded: false }.await;
    }
}

/// Returns to the executor once, asking to be polled again.
struct YieldNow {
    yielded: bool,
}

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        if self.yielded {
            return Poll::Ready(());
        }
        self.yielded = true;
        context.waker().wake_by_ref();
        Poll::Pending
    }
}
