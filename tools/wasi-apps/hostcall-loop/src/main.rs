//! `hostcall-loop`: measures the cost of the cheapest WASI host call.
//!
//! `wasi:clocks/monotonic-clock.now` does no I/O and returns one integer,
//! so its per-call time is the runtime's host-call overhead itself. The
//! Linux counterpart makes the same number of `clock_gettime` calls, the
//! cheapest syscall-shaped operation a native process has.

use std::num::ParseIntError;
use std::time::Duration;

use helios_bench_metrics::report_metric;
use thiserror::Error;
use wasi::clocks::monotonic_clock;

#[derive(Debug, Error)]
enum HostcallLoopError {
    #[error("usage: hostcall-loop <calls>")]
    Usage,
    #[error("unexpected argument `{0}`")]
    UnexpectedArgument(String),
    #[error("invalid call count `{raw}`")]
    InvalidCalls {
        raw: String,
        #[source]
        source: ParseIntError,
    },
    #[error("the monotonic clock went backwards")]
    ClockWentBackwards,
}

fn main() -> Result<(), HostcallLoopError> {
    let mut args = std::env::args().skip(1);
    let raw = args.next().ok_or(HostcallLoopError::Usage)?;
    if let Some(extra) = args.next() {
        return Err(HostcallLoopError::UnexpectedArgument(extra));
    }
    let calls: u64 = raw
        .parse()
        .map_err(|source| HostcallLoopError::InvalidCalls { raw, source })?;
    if calls == 0 {
        return Err(HostcallLoopError::Usage);
    }

    let started = monotonic_clock::now();
    let mut last = started;
    // Every reading feeds the next comparison so the loop body cannot be
    // hoisted or elided: each iteration is exactly one host call.
    for _ in 0..calls {
        let now = monotonic_clock::now();
        if now < last {
            return Err(HostcallLoopError::ClockWentBackwards);
        }
        last = now;
    }
    let elapsed = Duration::from_nanos(last - started);

    println!("hostcall-loop:{calls}");
    report_metric("ns_per_call", format!("{:.2}", elapsed.as_nanos() as f64 / calls as f64));
    Ok(())
}
