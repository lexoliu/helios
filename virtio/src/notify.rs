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

    pub fn notify_one(&self) {
        self.add_permits(1);
        self.event.notify(1);
    }

    pub fn notify_readiness(&self) {
        if self
            .permits
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.event.notify(1);
        }
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

#[cfg(test)]
mod tests {
    use futures_lite::future::block_on;

    use super::Notify;

    #[test]
    fn readiness_notification_coalesces_pending_permits() {
        let notify = Notify::new();

        notify.notify_readiness();
        notify.notify_readiness();

        block_on(notify.notified());
        assert!(
            !notify.try_claim_permit(),
            "readiness notification should not count duplicate interrupt edges"
        );
    }

    #[test]
    fn readiness_notification_rearms_after_waiter_consumes_permit() {
        let notify = Notify::new();

        notify.notify_readiness();
        block_on(notify.notified());
        notify.notify_readiness();

        block_on(notify.notified());
    }
}
