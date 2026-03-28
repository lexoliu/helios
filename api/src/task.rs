//! Async task utilities modeled after `async_std::task`.

use std::time::Duration;

use crate::bindings::wasi::clocks::monotonic_clock;

/// Suspends the current async task for at least `duration`.
pub async fn sleep(duration: Duration) {
    monotonic_clock::wait_for(duration_to_nanos(duration)).await;
}

/// Yields execution back to the host scheduler once.
pub async fn yield_now() {
    monotonic_clock::wait_until(monotonic_clock::now()).await;
}

fn duration_to_nanos(duration: Duration) -> u64 {
    duration
        .as_nanos()
        .try_into()
        .expect("duration does not fit into wasi nanoseconds")
}
