use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, Poll};

use event_listener::{Event, EventListener};

pub struct Notify {
    permits: AtomicUsize,
    event: Event,
}

pub struct Notified<'a> {
    notify: &'a Notify,
    listener: Option<EventListener>,
}

impl Notify {
    pub const fn new() -> Self {
        Self {
            permits: AtomicUsize::new(0),
            event: Event::new(),
        }
    }

    pub fn notified(&self) -> Notified<'_> {
        Notified {
            notify: self,
            listener: None,
        }
    }

    /// Wakes every task waiting on this notification.
    ///
    /// A device interrupt is a broadcast fact — the device made
    /// progress — and not a token that one waiter may consume: the
    /// packet pump and a per-operation waiter are routinely parked on
    /// the same device at once, and handing the wake to only one of
    /// them leaves the other asleep until its own deadline expires.
    /// The permit still covers a waiter that registers just after the
    /// interrupt rather than just before it.
    pub fn notify_all(&self) {
        self.add_permits(1);
        self.event.notify(usize::MAX);
    }

    fn add_permits(&self, permits: usize) {
        let mut current = self.permits.load(Ordering::Acquire);
        loop {
            let next = current.saturating_add(permits);
            match self.permits.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    fn try_claim_permit(&self) -> bool {
        let mut permits = self.permits.load(Ordering::Acquire);
        loop {
            if permits == 0 {
                return false;
            }

            match self.permits.compare_exchange_weak(
                permits,
                permits - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(next) => permits = next,
            }
        }
    }
}

impl Default for Notify {
    fn default() -> Self {
        Self::new()
    }
}

impl Future for Notified<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            if self.notify.try_claim_permit() {
                self.listener = None;
                return Poll::Ready(());
            }

            if self.listener.is_none() {
                self.listener = Some(self.notify.event.listen());
                continue;
            }

            let listener = self
                .listener
                .as_mut()
                .expect("notification listener disappeared unexpectedly");
            let mut listener = core::pin::pin!(listener);

            match listener.as_mut().poll(cx) {
                Poll::Ready(()) => {
                    self.listener = None;
                    continue;
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
