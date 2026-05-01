use async_signal::{Signal, Signals};
use std::future::Future;
use std::time::Duration;

use async_io::Timer;
use futures_lite::{StreamExt as _, future};

pub(crate) enum CommandRun<T> {
    Completed(T),
    Interrupted,
}

pub(crate) fn block_on<T>(future: impl Future<Output = T>) -> T {
    async_io::block_on(future)
}

pub(crate) async fn interruptible<T>(
    command: impl Future<Output = T>,
) -> std::io::Result<CommandRun<T>> {
    let mut signals = Signals::new([Signal::Int])?;
    future::or(
        async move { Ok(CommandRun::Completed(command.await)) },
        async move {
            match signals.next().await {
                Some(Ok(Signal::Int)) => Ok(CommandRun::Interrupted),
                Some(Ok(signal)) => panic!(
                    "unexpected signal while waiting for SIGINT command cancellation: {signal:?}"
                ),
                Some(Err(error)) => Err(error),
                None => panic!("signal stream ended while waiting for SIGINT"),
            }
        },
    )
    .await
}

pub(crate) async fn timeout<T>(duration: Duration, future: impl Future<Output = T>) -> Option<T> {
    future::or(async move { Some(future.await) }, async move {
        Timer::after(duration).await;
        None
    })
    .await
}
