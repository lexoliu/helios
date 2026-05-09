extern crate alloc;
#[cfg(test)]
extern crate std;

use alloc::sync::Arc;
use async_lock::{
    Mutex as AsyncMutex, MutexGuard as AsyncMutexGuard, MutexGuardArc as AsyncMutexGuardArc,
    RwLock as AsyncRwLock, RwLockReadGuard as AsyncRwLockReadGuard,
    RwLockReadGuardArc as AsyncRwLockReadGuardArc, RwLockWriteGuard as AsyncRwLockWriteGuard,
    RwLockWriteGuardArc as AsyncRwLockWriteGuardArc,
};
use core::cell::UnsafeCell;
use core::future::Future;
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, Poll};

use event_listener::{Event, EventListener};

/// Interrupt-safe notification primitive for bridging kernel events into async tasks.
///
/// Notifications are remembered as permits, so an interrupt that arrives before
/// a task starts awaiting is still observed later.
pub struct Notify {
    permits: AtomicUsize,
    event: Event,
}

/// Future returned by [`Notify::notified`].
#[must_use = "futures do nothing unless awaited"]
pub struct Notified<'a> {
    notify: &'a Notify,
    listener: Option<EventListener>,
}

/// Reusable state for polling a [`Notify`] without allocating a Future.
pub struct NotifyWaiter {
    listener: Option<EventListener>,
}

/// Raw async mutex state without protected payload storage.
///
/// This is the scheduling primitive used by both kernel-internal mutexes and
/// hosted component resources. Acquisition never spins. Interrupt handlers must not
/// await this lock directly; instead they should record work and wake a task
/// through [`Notify`].
pub struct RawMutex {
    inner: Arc<AsyncMutex<()>>,
}

/// Borrowed raw mutex lease.
pub struct RawMutexLease<'a> {
    _guard: AsyncMutexGuard<'a, ()>,
}

/// Owned raw mutex lease for resource-style integrations.
pub struct OwnedRawMutexLease {
    _guard: AsyncMutexGuardArc<()>,
}

/// Async mutex backed by kernel wakeups instead of spinning.
pub struct Mutex<T> {
    raw: RawMutex,
    value: UnsafeCell<T>,
}

/// Guard returned by [`Mutex::lock`].
pub struct MutexGuard<'a, T> {
    _lease: RawMutexLease<'a>,
    value: *mut T,
}

/// Raw async reader/writer lock state without protected payload storage.
///
/// This uses a fair async lock implementation that blocks new readers once a
/// writer has started waiting, which matches the kernel's requirement to avoid
/// starving mutation-heavy control paths.
pub struct RawRwLock {
    inner: Arc<AsyncRwLock<()>>,
}

/// Borrowed shared raw rwlock lease.
pub struct RawRwLockReadLease<'a> {
    _guard: AsyncRwLockReadGuard<'a, ()>,
}

/// Owned shared raw rwlock lease.
pub struct OwnedRawRwLockReadLease {
    _guard: AsyncRwLockReadGuardArc<()>,
}

/// Borrowed exclusive raw rwlock lease.
pub struct RawRwLockWriteLease<'a> {
    _guard: AsyncRwLockWriteGuard<'a, ()>,
}

/// Owned exclusive raw rwlock lease.
pub struct OwnedRawRwLockWriteLease {
    _guard: AsyncRwLockWriteGuardArc<()>,
}

/// Async reader/writer lock with writer preference.
pub struct RwLock<T> {
    raw: RawRwLock,
    value: UnsafeCell<T>,
}

/// Shared guard returned by [`RwLock::read`].
pub struct RwLockReadGuard<'a, T> {
    _lease: RawRwLockReadLease<'a>,
    value: *const T,
}

/// Exclusive guard returned by [`RwLock::write`].
pub struct RwLockWriteGuard<'a, T> {
    _lease: RawRwLockWriteLease<'a>,
    value: *mut T,
}

impl Notify {
    pub const fn new() -> Self {
        Self {
            permits: AtomicUsize::new(0),
            event: Event::new(),
        }
    }

    /// Returns a future that resolves after consuming one pending notification.
    pub fn notified(&self) -> Notified<'_> {
        Notified {
            notify: self,
            listener: None,
        }
    }

    pub const fn waiter(&self) -> NotifyWaiter {
        NotifyWaiter { listener: None }
    }

    pub fn poll_notified(&self, cx: &mut Context<'_>, waiter: &mut NotifyWaiter) -> Poll<()> {
        loop {
            if self.try_claim_permit() {
                waiter.listener = None;
                return Poll::Ready(());
            }

            if waiter.listener.is_none() {
                waiter.listener = Some(self.event.listen());
                continue;
            }

            let listener = waiter
                .listener
                .as_mut()
                .expect("notification listener disappeared unexpectedly");
            let mut listener = core::pin::pin!(listener);

            match listener.as_mut().poll(cx) {
                Poll::Ready(()) => {
                    waiter.listener = None;
                    continue;
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    /// Signals a single waiter.
    ///
    /// This operation is non-blocking and suitable for interrupt context.
    pub fn notify_one(&self) {
        self.notify_count(1);
    }

    /// Signals one waiter only when no notification is already pending.
    ///
    /// Use this for level-triggered progress signals where one wake is enough
    /// to make the waiter observe the latest state.
    pub fn notify_one_coalesced(&self) {
        if self
            .permits
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.event.notify(1);
        }
    }

    /// Signals up to `count` waiters with one permit update.
    ///
    /// This operation is non-blocking and suitable for interrupt context.
    pub fn notify_count(&self, count: usize) {
        if count == 0 {
            return;
        }
        self.add_permits(count);
        self.event.notify(count);
    }

    /// Signals every current waiter.
    ///
    /// This operation is non-blocking and suitable for interrupt context.
    pub fn notify_all(&self) {
        self.add_permits(usize::MAX / 2);
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
        let mut waiter = NotifyWaiter {
            listener: self.listener.take(),
        };
        let result = self.notify.poll_notified(cx, &mut waiter);
        self.listener = waiter.listener;
        result
    }
}

impl RawMutex {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(AsyncMutex::new(())),
        }
    }

    pub fn try_lock(&self) -> Option<RawMutexLease<'_>> {
        self.inner
            .try_lock()
            .map(|guard| RawMutexLease { _guard: guard })
    }

    pub async fn lock(&self) -> RawMutexLease<'_> {
        RawMutexLease {
            _guard: self.inner.lock().await,
        }
    }

    pub fn try_lock_owned(self: &Arc<Self>) -> Option<OwnedRawMutexLease> {
        self.inner
            .try_lock_arc()
            .map(|guard| OwnedRawMutexLease { _guard: guard })
    }

    pub async fn lock_owned(self: &Arc<Self>) -> OwnedRawMutexLease {
        OwnedRawMutexLease {
            _guard: self.inner.lock_arc().await,
        }
    }
}

impl Default for RawMutex {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Mutex<T> {
    pub fn new(value: T) -> Self {
        Self {
            raw: RawMutex::new(),
            value: UnsafeCell::new(value),
        }
    }

    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        self.raw.try_lock().map(|lease| MutexGuard {
            _lease: lease,
            value: self.value.get(),
        })
    }

    pub async fn lock(&self) -> MutexGuard<'_, T> {
        let lease = self.raw.lock().await;
        MutexGuard {
            _lease: lease,
            value: self.value.get(),
        }
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }

    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

impl<T: Default> Default for Mutex<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: holding the guard means the mutex is exclusively locked.
        unsafe { &*self.value }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: holding the guard means the mutex is exclusively locked.
        unsafe { &mut *self.value }
    }
}

impl RawRwLock {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(AsyncRwLock::new(())),
        }
    }

    pub fn try_read(&self) -> Option<RawRwLockReadLease<'_>> {
        self.inner
            .try_read()
            .map(|guard| RawRwLockReadLease { _guard: guard })
    }

    pub async fn read(&self) -> RawRwLockReadLease<'_> {
        RawRwLockReadLease {
            _guard: self.inner.read().await,
        }
    }

    pub fn try_read_owned(self: &Arc<Self>) -> Option<OwnedRawRwLockReadLease> {
        self.inner
            .try_read_arc()
            .map(|guard| OwnedRawRwLockReadLease { _guard: guard })
    }

    pub async fn read_owned(self: &Arc<Self>) -> OwnedRawRwLockReadLease {
        OwnedRawRwLockReadLease {
            _guard: self.inner.read_arc().await,
        }
    }

    pub fn try_write(&self) -> Option<RawRwLockWriteLease<'_>> {
        self.inner
            .try_write()
            .map(|guard| RawRwLockWriteLease { _guard: guard })
    }

    pub async fn write(&self) -> RawRwLockWriteLease<'_> {
        RawRwLockWriteLease {
            _guard: self.inner.write().await,
        }
    }

    pub fn try_write_owned(self: &Arc<Self>) -> Option<OwnedRawRwLockWriteLease> {
        self.inner
            .try_write_arc()
            .map(|guard| OwnedRawRwLockWriteLease { _guard: guard })
    }

    pub async fn write_owned(self: &Arc<Self>) -> OwnedRawRwLockWriteLease {
        OwnedRawRwLockWriteLease {
            _guard: self.inner.write_arc().await,
        }
    }
}

impl Default for RawRwLock {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> RwLock<T> {
    pub fn new(value: T) -> Self {
        Self {
            raw: RawRwLock::new(),
            value: UnsafeCell::new(value),
        }
    }

    pub fn try_read(&self) -> Option<RwLockReadGuard<'_, T>> {
        self.raw.try_read().map(|lease| RwLockReadGuard {
            _lease: lease,
            value: self.value.get().cast_const(),
        })
    }

    pub async fn read(&self) -> RwLockReadGuard<'_, T> {
        let lease = self.raw.read().await;
        RwLockReadGuard {
            _lease: lease,
            value: self.value.get().cast_const(),
        }
    }

    pub fn try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
        self.raw.try_write().map(|lease| RwLockWriteGuard {
            _lease: lease,
            value: self.value.get(),
        })
    }

    pub async fn write(&self) -> RwLockWriteGuard<'_, T> {
        let lease = self.raw.write().await;
        RwLockWriteGuard {
            _lease: lease,
            value: self.value.get(),
        }
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }

    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

impl<T: Default> Default for RwLock<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> Deref for RwLockReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: holding the guard means the rwlock is held for shared reading.
        unsafe { &*self.value }
    }
}

impl<T> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: holding the guard means the rwlock is held exclusively.
        unsafe { &*self.value }
    }
}

impl<T> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: holding the guard means the rwlock is held exclusively.
        unsafe { &mut *self.value }
    }
}

unsafe impl<T: Send> Send for Mutex<T> {}
unsafe impl<T: Send> Sync for Mutex<T> {}

unsafe impl<T: Send> Send for RwLock<T> {}
unsafe impl<T: Send + Sync> Sync for RwLock<T> {}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicBool, Ordering};

    use futures_lite::future::{block_on, yield_now, zip};

    use super::{Mutex, Notify, RawMutex, RwLock};

    #[test]
    fn notify_preserves_signal_sent_before_await() {
        let notify = Notify::new();
        notify.notify_one();
        block_on(notify.notified());
    }

    #[test]
    fn notify_wakes_async_waiter() {
        let notify = Arc::new(Notify::new());
        let ready = Arc::new(AtomicBool::new(false));
        let notify_thread = notify.clone();
        let ready_thread = ready.clone();

        let thread = std::thread::spawn(move || {
            while !ready_thread.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            notify_thread.notify_one();
        });

        block_on(async {
            ready.store(true, Ordering::Release);
            notify.notified().await;
        });

        thread
            .join()
            .unwrap_or_else(|_| panic!("notify helper thread panicked unexpectedly"));
    }

    #[test]
    fn notify_count_preserves_multiple_permits() {
        let notify = Notify::new();
        notify.notify_count(2);
        block_on(async {
            notify.notified().await;
            notify.notified().await;
        });
    }

    #[test]
    fn notify_one_coalesced_keeps_one_pending_permit() {
        let notify = Notify::new();
        notify.notify_one_coalesced();
        notify.notify_one_coalesced();
        assert_eq!(notify.permits.load(Ordering::Acquire), 1);

        block_on(notify.notified());
        assert_eq!(notify.permits.load(Ordering::Acquire), 0);
    }

    #[test]
    fn raw_mutex_supports_owned_leases() {
        block_on(async {
            let mutex = Arc::new(RawMutex::new());
            let first = mutex.lock_owned().await;
            let release = async move {
                yield_now().await;
                drop(first);
            };
            let acquire = {
                let mutex = mutex.clone();
                async move {
                    let second = mutex.lock_owned().await;
                    drop(second);
                }
            };

            zip(release, acquire).await;
        });
    }

    #[test]
    fn mutex_serializes_async_tasks() {
        block_on(async {
            let value = Arc::new(Mutex::new(Vec::new()));
            let left = {
                let value = value.clone();
                async move {
                    let mut guard = value.lock().await;
                    guard.push(1);
                    yield_now().await;
                    guard.push(3);
                }
            };
            let right = {
                let value = value.clone();
                async move {
                    yield_now().await;
                    let mut guard = value.lock().await;
                    guard.push(2);
                }
            };

            zip(left, right).await;

            let guard = value.lock().await;
            assert_eq!(&*guard, &[1, 3, 2]);
        });
    }

    #[test]
    fn rwlock_blocks_new_readers_while_writer_waits() {
        block_on(async {
            let lock = Arc::new(RwLock::new(5usize));
            let order = Arc::new(std::sync::Mutex::new(Vec::new()));

            let first_reader = {
                let lock = lock.clone();
                let order = order.clone();
                async move {
                    let reader = lock.read().await;
                    order.lock().unwrap().push("reader-1-acquired");
                    yield_now().await;
                    yield_now().await;
                    drop(reader);
                    order.lock().unwrap().push("reader-1-dropped");
                }
            };

            let writer = {
                let lock = lock.clone();
                let order = order.clone();
                async move {
                    yield_now().await;
                    let mut writer = lock.write().await;
                    order.lock().unwrap().push("writer-acquired");
                    *writer = 9;
                    yield_now().await;
                    drop(writer);
                    order.lock().unwrap().push("writer-dropped");
                }
            };

            let second_reader = {
                let lock = lock.clone();
                let order = order.clone();
                async move {
                    yield_now().await;
                    yield_now().await;
                    let reader = lock.read().await;
                    order.lock().unwrap().push("reader-2-acquired");
                    assert_eq!(*reader, 9);
                }
            };

            zip(first_reader, zip(writer, second_reader)).await;

            let order = order.lock().unwrap().clone();
            assert_eq!(
                order,
                vec![
                    "reader-1-acquired",
                    "reader-1-dropped",
                    "writer-acquired",
                    "writer-dropped",
                    "reader-2-acquired",
                ]
            );
        });
    }
}
