use std::future::Future;
use std::time::Duration;

use async_io::Timer;
use futures_lite::future;

pub(crate) fn block_on<T>(future: impl Future<Output = T>) -> T {
    async_io::block_on(future)
}

pub(crate) async fn timeout<T>(duration: Duration, future: impl Future<Output = T>) -> Option<T> {
    future::or(async move { Some(future.await) }, async move {
        Timer::after(duration).await;
        None
    })
    .await
}
