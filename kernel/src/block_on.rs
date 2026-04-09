extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::task::Wake;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Context, Poll, Waker};

pub fn block_on<F: Future>(future: F) -> F::Output {
    let parker = Arc::new(BlockOnParker::new());
    let waker = Waker::from(parker.clone());
    let mut context = Context::from_waker(&waker);
    let mut future = Pin::from(Box::new(future));

    loop {
        parker.clear();
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => parker.wait(),
        }
    }
}

struct BlockOnParker {
    notified: AtomicBool,
}

impl BlockOnParker {
    const fn new() -> Self {
        Self {
            notified: AtomicBool::new(false),
        }
    }

    fn clear(&self) {
        self.notified.store(false, Ordering::Release);
    }

    fn wait(&self) {
        while !self.notified.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
    }
}

impl Wake for BlockOnParker {
    fn wake(self: Arc<Self>) {
        self.notified.store(true, Ordering::Release);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.notified.store(true, Ordering::Release);
    }
}
