//! `sched-tasks`: N cooperative tasks yielding to the kernel scheduler.
//!
//! Each task calls `task::yield_now` `yields` times and records how long
//! every yield took to come back. A yield is one host call
//! (`monotonic-clock.wait-until(now)`) that parks the task and lets the
//! executor run every other ready task first, so the distribution is the
//! cost of a context switch under a load of `tasks` runnable peers. The
//! Linux counterpart (`tools/bench/native/sched-tasks.c`) does the same
//! with `tasks` threads calling `sched_yield`.

use std::env;
use std::num::ParseIntError;
use std::time::Instant;

use helios_api::{channel, task};
use helios_bench_metrics::LatencySamples;
use thiserror::Error;

#[derive(Debug, Error)]
enum SchedTasksError {
    #[error("usage: sched-tasks <tasks> <yields>")]
    Usage,
    #[error("unexpected argument `{0}`")]
    UnexpectedArgument(String),
    #[error("invalid number `{raw}`")]
    InvalidNumber {
        raw: String,
        #[source]
        source: ParseIntError,
    },
    #[error("a task ended without reporting its samples")]
    TaskLost,
}

#[helios_api::main]
async fn main() -> Result<(), SchedTasksError> {
    let mut args = env::args().skip(1);
    let tasks = parse_number(args.next())?;
    let yields = parse_number(args.next())?;
    if let Some(extra) = args.next() {
        return Err(SchedTasksError::UnexpectedArgument(extra));
    }

    let (tx, rx) = channel::bounded(tasks);
    let started = Instant::now();
    for _ in 0..tasks {
        let tx = tx.clone();
        task::spawn(async move {
            let mut samples = LatencySamples::with_capacity(yields);
            for _ in 0..yields {
                let yielded = Instant::now();
                task::yield_now().await;
                samples.record(yielded.elapsed());
            }
            // The receiver only disappears when main already failed.
            let _ = tx.send(samples).await;
        });
    }
    drop(tx);

    let mut all = LatencySamples::with_capacity(tasks * yields);
    for _ in 0..tasks {
        let samples = rx.recv().await.map_err(|_| SchedTasksError::TaskLost)?;
        all.extend(&samples);
    }
    let elapsed = started.elapsed();

    println!("sched-tasks:{}", tasks * yields);
    all.report("switch");
    helios_bench_metrics::report_metric(
        "switches_per_s",
        format!("{:.0}", (tasks * yields) as f64 / elapsed.as_secs_f64()),
    );
    Ok(())
}

fn parse_number(raw: Option<String>) -> Result<usize, SchedTasksError> {
    let raw = raw.ok_or(SchedTasksError::Usage)?;
    let value = raw
        .parse::<usize>()
        .map_err(|source| SchedTasksError::InvalidNumber { raw, source })?;
    if value == 0 {
        return Err(SchedTasksError::Usage);
    }
    Ok(value)
}
