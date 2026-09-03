use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, Poll};

use event_listener::{Event, EventListener};

/// A device's "something happened" signal.
///
/// A device interrupt is a broadcast fact — the device made progress —
/// and not a token that one waiter may consume: the packet pump and a
/// per-operation waiter are routinely parked on the same device at
/// once, and a signal that only one of them can claim leaves the other
/// asleep until its own deadline expires, or forever when it has none.
///
/// The signal is therefore a counter rather than a pool of permits.
/// Each waiter records where the counter stood when it began waiting
/// and finishes as soon as the device moves past it, so one interrupt
/// releases every waiter that was already waiting for it, and a wake
/// that lands between creating the future and first polling it is still
/// observed rather than lost.
pub struct Notify {
    generation: AtomicUsize,
    event: Event,
}

pub struct Notified<'a> {
    notify: &'a Notify,
    /// Where the counter stood when this wait began.
    observed: usize,
    listener: Option<EventListener>,
}

impl Notify {
    pub const fn new() -> Self {
        Self {
            generation: AtomicUsize::new(0),
            event: Event::new(),
        }
    }

    /// Waits for the device's next notification.
    ///
    /// The counter is read here, not at the first poll, so the caller
    /// may create the future before re-testing what it is waiting for
    /// and still observe a notification that arrives in between.
    pub fn notified(&self) -> Notified<'_> {
        Notified {
            notify: self,
            observed: self.generation.load(Ordering::Acquire),
            listener: None,
        }
    }

    /// Wakes every task waiting on this notification.
    pub fn notify_all(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.event.notify(usize::MAX);
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
            if self.notify.generation.load(Ordering::Acquire) != self.observed {
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
    use super::Notify;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};

    /// Polls `future` once, reporting whether it finished.
    fn poll_once(future: core::pin::Pin<&mut impl core::future::Future<Output = ()>>) -> bool {
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        matches!(future.poll(&mut context), Poll::Ready(()))
    }

    #[test]
    fn one_notification_releases_every_waiter() {
        // The pump and a per-request waiter park on the same device.
        // A signal only one of them could claim is what leaves the
        // other asleep with no second interrupt coming.
        let notify = Notify::new();
        let mut first = pin!(notify.notified());
        let mut second = pin!(notify.notified());
        assert!(!poll_once(first.as_mut()));
        assert!(!poll_once(second.as_mut()));

        notify.notify_all();

        assert!(poll_once(first.as_mut()));
        assert!(poll_once(second.as_mut()));
    }

    #[test]
    fn a_notification_between_arming_and_polling_is_not_lost() {
        let notify = Notify::new();
        let mut waiter = pin!(notify.notified());
        notify.notify_all();
        assert!(poll_once(waiter.as_mut()));
    }

    #[test]
    fn a_wait_that_follows_a_notification_still_waits() {
        // Notifications are not banked: a waiter that arrives after the
        // device went quiet has to park, or every loop that polls this
        // becomes a spin.
        let notify = Notify::new();
        notify.notify_all();
        let mut waiter = pin!(notify.notified());
        assert!(!poll_once(waiter.as_mut()));
    }
}
