mod exports;
#[cfg(feature = "guest")]
mod forwarder;
mod guest_tokio;
mod imports;
pub mod serial;
pub mod stats;
pub mod sync;
pub mod tracing;

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::Result;
use futures_core::Stream;
use futures_lite::future;

#[cfg(feature = "guest")]
pub use forwarder::serve;

pub(crate) async fn resolve_future<'a, T, F, D>(future: F, driver: Option<D>) -> Result<T>
where
    F: Future<Output = T> + Send + 'a,
    D: Future<Output = Result<()>> + Send + 'a,
{
    match driver {
        Some(driver) => {
            let (value, driver) = future::zip(future, driver).await;
            driver?;
            Ok(value)
        }
        None => Ok(future.await),
    }
}

pub struct Subscription<'a, T> {
    stream: Pin<Box<dyn Stream<Item = Vec<T>> + Send + 'a>>,
    driver: Option<Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>>,
    buffered: VecDeque<T>,
}

impl<'a, T> Subscription<'a, T> {
    pub(crate) fn new<S, D>(stream: S, driver: Option<D>) -> Self
    where
        S: Stream<Item = Vec<T>> + Send + 'a,
        D: Future<Output = Result<()>> + Send + 'a,
    {
        Self {
            stream: Box::pin(stream),
            driver: driver.map(|driver| Box::pin(driver) as _),
            buffered: VecDeque::new(),
        }
    }

    pub async fn next(&mut self) -> Result<Option<T>> {
        if let Some(item) = self.buffered.pop_front() {
            return Ok(Some(item));
        }

        future::poll_fn(|cx| self.poll_next(cx)).await
    }

    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<T>>> {
        if let Some(item) = self.buffered.pop_front() {
            return Poll::Ready(Ok(Some(item)));
        }

        if let Some(driver) = self.driver.as_mut() {
            match driver.as_mut().poll(cx) {
                Poll::Ready(Ok(())) => {
                    self.driver = None;
                }
                Poll::Ready(Err(error)) => {
                    self.driver = None;
                    return Poll::Ready(Err(error));
                }
                Poll::Pending => {}
            }
        }

        match self.stream.as_mut().poll_next(cx) {
            Poll::Ready(Some(items)) => {
                self.buffered.extend(items);
                if let Some(item) = self.buffered.pop_front() {
                    Poll::Ready(Ok(Some(item)))
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
            Poll::Ready(None) if self.driver.is_none() => Poll::Ready(Ok(None)),
            Poll::Ready(None) => Poll::Pending,
            Poll::Pending => Poll::Pending,
        }
    }
}
