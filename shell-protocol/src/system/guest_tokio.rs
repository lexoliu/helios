use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

pub use tokio::io;
pub use tokio::try_join;

pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    JoinHandle {
        aborted: AtomicBool::new(false),
        future: Mutex::new(Some(Box::pin(future))),
    }
}

pub struct JoinHandle<T> {
    aborted: AtomicBool,
    future: Mutex<Option<Pin<Box<dyn Future<Output = T> + Send + 'static>>>>,
}

impl<T> JoinHandle<T> {
    pub fn abort(&self) {
        self.aborted.store(true, Ordering::Release);
        let mut future = self
            .future
            .lock()
            .unwrap_or_else(|_| panic!("guest join handle mutex poisoned"));
        future.take();
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.aborted.load(Ordering::Acquire) {
            return Poll::Ready(Err(JoinError));
        }

        let mut future = self
            .future
            .lock()
            .unwrap_or_else(|_| panic!("guest join handle mutex poisoned"));
        let inner = future
            .as_mut()
            .unwrap_or_else(|| panic!("guest join handle polled after completion"));

        match inner.as_mut().poll(cx) {
            Poll::Ready(output) => {
                future.take();
                Poll::Ready(Ok(output))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct JoinError;

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("guest task was aborted")
    }
}

impl Error for JoinError {}
