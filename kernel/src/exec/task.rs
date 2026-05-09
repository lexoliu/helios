use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

/// Returns a future that yields execution back to the executor exactly once.
///
/// This is the cooperative scheduling primitive for CPU-bound async work:
/// the current task asks to be polled again soon, but lets other ready tasks
/// run first.
pub fn yield_now() -> YieldNow {
    YieldNow { yielded: false }
}

#[must_use = "futures do nothing unless awaited"]
pub struct YieldNow {
    yielded: bool,
}

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.yielded {
            return Poll::Ready(());
        }

        self.yielded = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}
