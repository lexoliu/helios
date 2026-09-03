//! Async byte-stream channels used to connect a spawned child component's
//! stdio streams to the parent that spawned it.
//!
//! These channels are single-producer-ish multiple-consumer-ish only on
//! the live-object level: writers can be cloned freely, and EOF is
//! delivered to the reader once *every* writer has been dropped. On the
//! reader side there is exactly one consumer — dropping it cancels any
//! further deliveries.
//!
//! Every operation is async — nothing here ever spins or blocks the
//! cooperative executor.
//!
//! # Flow control
//!
//! The queue holds at most [`BYTE_CHANNEL_CHUNK_CAPACITY`] chunks. That
//! bound is *flow control*, not an assertion: a guest that prints faster
//! than its reader drains must be made to wait, never to kill the kernel.
//! [`ByteWriter::try_write`] reports [`TryWrite::Full`] and hands the
//! chunk back to the caller; [`ByteWriter::write`] parks the calling task
//! on the `writable` signal until room appears. No byte is ever dropped
//! on a full channel and a transiently full channel is never reported as
//! an error.
//!
//! A parked writer is resumed as soon as a pop frees a slot — the resume
//! threshold is "the queue is no longer full", identical to the predicate
//! the writer parked on. A lower watermark (say, half capacity) would
//! batch wakeups better, but it deadlocks peers that alternate: a reader
//! that consumes part of a burst and then waits for the writer's next
//! chunk would never let the writer past a half-full queue. POSIX pipes
//! wake writers on any read progress for the same reason, and wakeups are
//! already coalesced here — `notify_all` drains the waker set, and a pop
//! only signals when the queue was full, so one park costs exactly one
//! wakeup.
//!
//! # Concurrency contract (SMP)
//!
//! - `ByteChunks` is the only mutable queue state and lives behind a
//!   `spin::Mutex`. It is held for the duration of a push/pop/clear and
//!   never across a wake or an `.await`.
//! - The close flags (`reader_closed`, `writer_closed`) and the handle
//!   counters are plain atomics. Close is published with `Release` and
//!   observed with `Acquire`, so a writer that sees "open" and then
//!   pushes under the queue lock cannot race a `close_reader` that
//!   already drained the queue: `close_reader` sets the flag *while
//!   holding* the queue lock.
//! - Each direction has its own [`ByteSignal`]. A signal is a coalescing
//!   permit plus a waker set guarded by its own `spin::Mutex`; the two
//!   signals are independent, so a drain on one CPU and a write on
//!   another never contend on the same lock.
//! - Wakeup safety (no lost wakeups) rests on a Dekker-style handshake
//!   between [`ByteSignal::poll_notified`] and the notifiers. A waiter
//!   claims the permit, registers its waker, then claims the permit
//!   *again* before reporting `Pending`. A notifier publishes the permit
//!   first and only then looks at the waiter count. Both the permit
//!   store/load and the waiter-count store/load use `SeqCst` so that the
//!   store-then-load pairs on the two sides cannot be reordered: if the
//!   notifier's count load misses a registration, its permit store is
//!   ordered before the waiter's second permit load, and the waiter
//!   returns `Ready` instead of parking.

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use core::task::{Context, Poll, Waker};

use bytes::Bytes;
use heapless::{Deque, Vec as HeapVec};
use spin::Mutex;
use triomphe::Arc;

const BYTE_CHANNEL_CHUNK_CAPACITY: usize = 256;
// Keep process/stdio/socketpair setup cheap while preserving the common
// 64-chunk burst shape: 16 chunks stay in the channel allocation and the first
// overflow reserve covers the remaining 48 chunks. Local divan keeps the
// 64-chunk case at 2.304 KiB while create/drop and 16-chunk transfers move to
// a 768 B channel allocation instead of the old inline-64 2.304 KiB slab.
const BYTE_CHANNEL_INLINE_CHUNKS: usize = 16;
const BYTE_CHANNEL_OVERFLOW_INITIAL_CHUNKS: usize = 48;
/// Waiters parked without touching the allocator. One reader and one
/// writer task per channel is the norm; guest threads sharing a pipe can
/// exceed it, and those spill into [`WakerSet::overflow`] rather than
/// panicking a kernel that a guest can drive.
const BYTE_CHANNEL_WAKER_CAPACITY: usize = 8;

/// A single byte-stream channel, closable from both ends. Producers push
/// reference-counted byte chunks; consumers await and receive the same chunks
/// back without forcing an extra copy at adapter boundaries.
struct ByteChannel {
    queue: ByteQueue,
    /// Notifies consumers when new bytes or a close event are available.
    readable: ByteSignal,
    /// Notifies producers parked on a full queue that a slot was freed or
    /// that the channel closed under them.
    writable: ByteSignal,
    /// Set to `true` once every `ByteWriter` cloneable handle has been
    /// dropped, signalling EOF to the reader.
    writer_closed: AtomicBool,
    /// Set to `true` when the reader has gone away, so writers can stop
    /// producing bytes.
    reader_closed: AtomicBool,
    writer_handles: AtomicUsize,
    reader_handles: AtomicUsize,
}

impl ByteChannel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            queue: ByteQueue::new(),
            readable: ByteSignal::new(),
            writable: ByteSignal::new(),
            writer_closed: AtomicBool::new(false),
            reader_closed: AtomicBool::new(false),
            writer_handles: AtomicUsize::new(1),
            reader_handles: AtomicUsize::new(1),
        })
    }

    fn push(&self, bytes: Bytes) -> Result<QueuePush, ClosedPeer> {
        self.queue
            .push_if_open(bytes, &self.reader_closed, &self.writer_closed)
    }

    /// Pop one chunk and release a writer parked on a full queue.
    ///
    /// The wakeup is only issued when the queue *was* full, which is the
    /// only state a writer can park in, so a steady drain of a channel
    /// nobody is blocked on costs one comparison.
    fn pop(&self) -> Option<Bytes> {
        let (bytes, was_full) = self.queue.pop();
        if was_full && bytes.is_some() {
            self.writable.notify_all();
        }
        bytes
    }

    fn close_reader(&self) {
        self.queue.close_reader(&self.reader_closed);
        self.readable.notify_one();
        self.writable.notify_all();
    }

    fn is_closed(&self) -> bool {
        self.reader_closed.load(Ordering::Acquire) || self.writer_closed.load(Ordering::Acquire)
    }
}

struct ByteQueue {
    chunks: Mutex<ByteChunks>,
}

struct ByteChunks {
    inline: Deque<Bytes, BYTE_CHANNEL_INLINE_CHUNKS>,
    overflow: Option<VecDeque<Bytes>>,
    len: usize,
}

/// Outcome of a push against the bounded queue.
enum QueuePush {
    Pushed,
    /// The queue is at capacity; the chunk is handed back untouched.
    Full(Bytes),
}

impl ByteQueue {
    const fn new() -> Self {
        Self {
            chunks: Mutex::new(ByteChunks::new()),
        }
    }

    fn push_if_open(
        &self,
        bytes: Bytes,
        reader_closed: &AtomicBool,
        writer_closed: &AtomicBool,
    ) -> Result<QueuePush, ClosedPeer> {
        let mut chunks = self.chunks.lock();
        if reader_closed.load(Ordering::Acquire) || writer_closed.load(Ordering::Acquire) {
            return Err(ClosedPeer);
        }
        Ok(chunks.push(bytes))
    }

    /// Pops one chunk, reporting whether the queue was full beforehand.
    fn pop(&self) -> (Option<Bytes>, bool) {
        let mut chunks = self.chunks.lock();
        let was_full = chunks.is_full();
        (chunks.pop(), was_full)
    }

    fn is_empty(&self) -> bool {
        self.chunks.lock().is_empty()
    }

    fn is_full(&self) -> bool {
        self.chunks.lock().is_full()
    }

    fn close_reader(&self, reader_closed: &AtomicBool) {
        let mut chunks = self.chunks.lock();
        reader_closed.store(true, Ordering::Release);
        chunks.clear();
    }

    #[cfg(test)]
    const fn capacity(&self) -> usize {
        BYTE_CHANNEL_CHUNK_CAPACITY
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.chunks.lock().len
    }
}

impl ByteChunks {
    const fn new() -> Self {
        Self {
            inline: Deque::new(),
            overflow: None,
            len: 0,
        }
    }

    fn push(&mut self, bytes: Bytes) -> QueuePush {
        if self.is_full() {
            return QueuePush::Full(bytes);
        }
        if let Some(overflow) = self.overflow.as_mut() {
            overflow.push_back(bytes);
            self.len += 1;
            debug_assert!(self.len <= BYTE_CHANNEL_CHUNK_CAPACITY);
            return QueuePush::Pushed;
        }

        match self.inline.push_back(bytes) {
            Ok(()) => {
                self.len += 1;
            }
            Err(bytes) => {
                let mut overflow = VecDeque::with_capacity(BYTE_CHANNEL_OVERFLOW_INITIAL_CHUNKS);
                overflow.push_back(bytes);
                self.overflow = Some(overflow);
                self.len += 1;
            }
        }
        debug_assert!(self.len <= BYTE_CHANNEL_CHUNK_CAPACITY);
        QueuePush::Pushed
    }

    fn pop(&mut self) -> Option<Bytes> {
        if let Some(bytes) = self.inline.pop_front() {
            self.len -= 1;
            return Some(bytes);
        }
        let overflow = self.overflow.as_mut()?;
        let bytes = overflow.pop_front();
        if bytes.is_some() {
            self.len -= 1;
        }
        if overflow.is_empty() {
            self.overflow = None;
        }
        bytes
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn is_full(&self) -> bool {
        self.len >= BYTE_CHANNEL_CHUNK_CAPACITY
    }

    fn clear(&mut self) {
        self.inline.clear();
        self.overflow = None;
        self.len = 0;
    }
}

/// Parked wakers for one direction of one channel.
///
/// The inline capacity covers the ordinary one-task-per-direction case
/// without allocating. It spills instead of panicking because the number
/// of parked tasks is guest-driven: a guest's threads can all block writing
/// to the same pipe.
struct WakerSet {
    inline: HeapVec<Waker, BYTE_CHANNEL_WAKER_CAPACITY>,
    overflow: Option<Vec<Waker>>,
    len: usize,
}

impl WakerSet {
    const fn new() -> Self {
        Self {
            inline: HeapVec::new(),
            overflow: None,
            len: 0,
        }
    }

    fn push(&mut self, waker: Waker) {
        if let Some(overflow) = self.overflow.as_mut() {
            overflow.push(waker);
            self.len += 1;
            return;
        }
        match self.inline.push(waker) {
            Ok(()) => self.len += 1,
            Err(waker) => {
                self.overflow = Some(alloc::vec![waker]);
                self.len += 1;
            }
        }
    }

    fn pop(&mut self) -> Option<Waker> {
        if let Some(overflow) = self.overflow.as_mut() {
            let waker = overflow.pop();
            let drained = overflow.is_empty();
            if drained {
                self.overflow = None;
            }
            if waker.is_some() {
                self.len -= 1;
                return waker;
            }
        }
        let waker = self.inline.pop();
        if waker.is_some() {
            self.len -= 1;
        }
        waker
    }

    /// Replace a previously registered waker in place, keeping the set
    /// free of duplicates for a repeatedly polled future.
    fn replace(&mut self, registered: &Waker, waker: Waker) -> bool {
        if let Some(overflow) = self.overflow.as_mut()
            && let Some(stored) = overflow
                .iter_mut()
                .find(|stored| stored.will_wake(registered))
        {
            *stored = waker;
            return true;
        }
        if let Some(stored) = self
            .inline
            .iter_mut()
            .find(|stored| stored.will_wake(registered))
        {
            *stored = waker;
            return true;
        }
        false
    }

    fn remove(&mut self, waker: &Waker) {
        if let Some(overflow) = self.overflow.as_mut()
            && let Some(index) = overflow.iter().position(|stored| stored.will_wake(waker))
        {
            overflow.swap_remove(index);
            let drained = overflow.is_empty();
            if drained {
                self.overflow = None;
            }
            self.len -= 1;
            return;
        }
        if let Some(index) = self
            .inline
            .iter()
            .position(|stored| stored.will_wake(waker))
        {
            self.inline.swap_remove(index);
            self.len -= 1;
        }
    }
}

struct ByteSignal {
    // Coalesce notifications: queue state is authoritative, so a burst of
    // writes must not leave hundreds of stale permits for the next empty wait.
    permit: AtomicBool,
    /// Number of wakers currently parked in `wakers`. Mutated only under
    /// the `wakers` lock, read without it on the notify fast path.
    waiters: AtomicUsize,
    wakers: Mutex<WakerSet>,
}

impl ByteSignal {
    const fn new() -> Self {
        Self {
            permit: AtomicBool::new(false),
            waiters: AtomicUsize::new(0),
            wakers: Mutex::new(WakerSet::new()),
        }
    }

    fn notify_one(&self) {
        if !self.publish_permit() {
            return;
        }
        let waker = self.take_waker();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Wake every parked waiter. Used for writability, where several
    /// tasks may be parked on the same full queue and each of them has to
    /// re-evaluate the queue for itself.
    fn notify_all(&self) {
        if !self.publish_permit() {
            return;
        }
        while let Some(waker) = self.take_waker() {
            waker.wake();
        }
    }

    /// Publish the permit and report whether anyone is parked.
    ///
    /// The `SeqCst` pair is load-bearing: see the module's wakeup-safety
    /// note. A `Release` store followed by an `Acquire` load may be
    /// reordered, and that reordering is exactly a lost wakeup.
    fn publish_permit(&self) -> bool {
        self.permit.store(true, Ordering::SeqCst);
        self.waiters.load(Ordering::SeqCst) != 0
    }

    fn take_waker(&self) -> Option<Waker> {
        let mut wakers = self.wakers.lock();
        let waker = wakers.pop();
        self.waiters.store(wakers.len, Ordering::SeqCst);
        waker
    }

    fn poll_notified(&self, cx: &mut Context<'_>, wait: &mut SignalWait) -> Poll<()> {
        if self.try_claim_permit() {
            self.remove_registered_waker(wait);
            return Poll::Ready(());
        }
        self.register_waker(wait, cx.waker());
        if self.try_claim_permit() {
            self.remove_registered_waker(wait);
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    fn register_waker(&self, wait: &mut SignalWait, waker: &Waker) {
        let new_waker = waker.clone();
        let mut wakers = self.wakers.lock();
        if let Some(registered) = wait.registered.as_ref()
            && wakers.replace(registered, new_waker.clone())
        {
            wait.registered = Some(new_waker);
            return;
        }
        wait.registered = None;
        wakers.push(new_waker.clone());
        self.waiters.store(wakers.len, Ordering::SeqCst);
        wait.registered = Some(new_waker);
    }

    fn remove_registered_waker(&self, wait: &mut SignalWait) {
        if let Some(waker) = wait.registered.take() {
            self.remove_waker(&waker);
        }
    }

    fn remove_waker(&self, waker: &Waker) {
        let mut wakers = self.wakers.lock();
        wakers.remove(waker);
        self.waiters.store(wakers.len, Ordering::SeqCst);
    }

    fn try_claim_permit(&self) -> bool {
        self.permit.swap(false, Ordering::SeqCst)
    }
}

/// Reusable waker registration for one signal.
struct SignalWait {
    registered: Option<Waker>,
}

impl SignalWait {
    const fn new() -> Self {
        Self { registered: None }
    }
}

/// Producer half of a byte stream. Writers can be cloned — the channel
/// stays open as long as any cloned writer is alive; it closes once they
/// are all dropped.
pub struct ByteWriter {
    channel: Arc<ByteChannel>,
}

/// Consumer half of a byte stream. Clonable so multiple async tasks can
/// share the same byte feed — the underlying queue is already
/// MPMC-safe. Dropping the last clone closes the reader side.
pub struct ByteReader {
    channel: Arc<ByteChannel>,
}

pub struct ByteReadWait {
    channel: Arc<ByteChannel>,
    wait: SignalWait,
}

impl Drop for ByteReadWait {
    fn drop(&mut self) {
        if let Some(waker) = self.wait.registered.take() {
            self.channel.readable.remove_waker(&waker);
        }
    }
}

/// Reusable parking state for [`ByteWriter::poll_writable`] and
/// [`ByteWriter::poll_write`], mirroring [`ByteReadWait`].
pub struct ByteWriteWait {
    channel: Arc<ByteChannel>,
    wait: SignalWait,
}

impl Drop for ByteWriteWait {
    fn drop(&mut self) {
        if let Some(waker) = self.wait.registered.take() {
            self.channel.writable.remove_waker(&waker);
        }
    }
}

pub fn byte_channel() -> (ByteWriter, ByteReader) {
    let channel = ByteChannel::new();
    (
        ByteWriter {
            channel: channel.clone(),
        },
        ByteReader { channel },
    )
}

/// Indicates that the peer has gone away. Writers see this when the reader
/// was dropped; readers see EOF as `Option::None` instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClosedPeer;

/// Outcome of a non-blocking [`ByteWriter::try_write`].
#[derive(Debug, PartialEq, Eq)]
pub enum TryWrite {
    /// The chunk was queued.
    Written,
    /// The queue is at capacity. The chunk is returned untouched so the
    /// caller can park on writability and retry with the same bytes —
    /// nothing is dropped and nothing is reported as an error.
    Full(Bytes),
    /// The peer has gone away; the chunk will never be delivered.
    Closed,
}

impl Clone for ByteWriter {
    fn clone(&self) -> Self {
        // The separate closed flags publish EOF/peer-close visibility; these
        // counters only elect the last handle.
        let previous = self.channel.writer_handles.fetch_add(1, Ordering::Relaxed);
        assert!(
            previous != 0,
            "byte writer cloned after liveness reached zero"
        );
        Self {
            channel: self.channel.clone(),
        }
    }
}

impl Drop for ByteWriter {
    fn drop(&mut self) {
        let previous = self.channel.writer_handles.fetch_sub(1, Ordering::Relaxed);
        assert!(previous != 0, "byte writer handle count underflowed");
        if previous == 1 {
            self.channel.writer_closed.store(true, Ordering::Release);
            self.channel.readable.notify_one();
        }
    }
}

impl Clone for ByteReader {
    fn clone(&self) -> Self {
        // The separate closed flags publish EOF/peer-close visibility; these
        // counters only elect the last handle.
        let previous = self.channel.reader_handles.fetch_add(1, Ordering::Relaxed);
        assert!(
            previous != 0,
            "byte reader cloned after liveness reached zero"
        );
        Self {
            channel: self.channel.clone(),
        }
    }
}

impl Drop for ByteReader {
    fn drop(&mut self) {
        let previous = self.channel.reader_handles.fetch_sub(1, Ordering::Relaxed);
        assert!(previous != 0, "byte reader handle count underflowed");
        if previous == 1 {
            self.channel.close_reader();
        }
    }
}

impl ByteWriter {
    /// Reusable parking state for the poll-based write paths.
    pub fn wait_state(&self) -> ByteWriteWait {
        ByteWriteWait {
            channel: self.channel.clone(),
            wait: SignalWait::new(),
        }
    }

    /// Push a chunk of bytes to the consumer, parking the calling task
    /// while the queue is at capacity. Resolves once the chunk is queued
    /// or the peer is gone.
    pub async fn write(&self, bytes: impl Into<Bytes>) -> Result<(), ClosedPeer> {
        let mut pending = match self.try_write(bytes) {
            TryWrite::Written => return Ok(()),
            TryWrite::Closed => return Err(ClosedPeer),
            TryWrite::Full(bytes) => Some(bytes),
        };
        let mut wait = self.wait_state();
        core::future::poll_fn(|cx| self.poll_write(cx, &mut wait, &mut pending)).await
    }

    /// Non-blocking push. Reports [`TryWrite::Full`] with the chunk
    /// handed back when the queue is at capacity.
    pub fn try_write(&self, bytes: impl Into<Bytes>) -> TryWrite {
        if self.channel.is_closed() {
            return TryWrite::Closed;
        }
        let bytes = bytes.into();
        if bytes.is_empty() {
            return TryWrite::Written;
        }
        match self.channel.push(bytes) {
            Ok(QueuePush::Pushed) => {
                self.channel.readable.notify_one();
                TryWrite::Written
            }
            Ok(QueuePush::Full(bytes)) => TryWrite::Full(bytes),
            Err(ClosedPeer) => TryWrite::Closed,
        }
    }

    /// Poll-driven push for synchronous poll contexts (a component's
    /// stream consumers). On `Poll::Pending` the chunk stays in `pending` and the
    /// task is registered for the next writability wakeup.
    pub fn poll_write(
        &self,
        cx: &mut Context<'_>,
        wait: &mut ByteWriteWait,
        pending: &mut Option<Bytes>,
    ) -> Poll<Result<(), ClosedPeer>> {
        loop {
            let Some(bytes) = pending.take() else {
                return Poll::Ready(Ok(()));
            };
            match self.try_write(bytes) {
                TryWrite::Written => return Poll::Ready(Ok(())),
                TryWrite::Closed => return Poll::Ready(Err(ClosedPeer)),
                TryWrite::Full(bytes) => {
                    *pending = Some(bytes);
                    match self.channel.writable.poll_notified(cx, &mut wait.wait) {
                        Poll::Ready(()) => continue,
                        Poll::Pending => return Poll::Pending,
                    }
                }
            }
        }
    }

    /// True when a `try_write` would not report [`TryWrite::Full`]: the
    /// queue has room, or the channel is closed and the write would fail
    /// outright rather than block.
    pub fn is_writable(&self) -> bool {
        self.channel.is_closed() || !self.channel.queue.is_full()
    }

    pub fn poll_writable(&self, cx: &mut Context<'_>, wait: &mut ByteWriteWait) -> Poll<()> {
        loop {
            if self.is_writable() {
                return Poll::Ready(());
            }
            match self.channel.writable.poll_notified(cx, &mut wait.wait) {
                Poll::Ready(()) => continue,
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    /// Await room in the queue without pushing anything, for adapters
    /// whose readiness pollable is separate from the write itself.
    pub async fn writable(&self) {
        if self.is_writable() {
            return;
        }
        let mut wait = self.wait_state();
        core::future::poll_fn(|cx| self.poll_writable(cx, &mut wait)).await;
    }

    /// Returns true once the reader has been dropped.
    pub fn is_reader_closed(&self) -> bool {
        self.channel.reader_closed.load(Ordering::Acquire)
    }

    pub fn close(&self) {
        self.channel.writer_closed.store(true, Ordering::Release);
        self.channel.readable.notify_one();
        self.channel.writable.notify_all();
    }
}

impl ByteReader {
    pub fn wait_state(&self) -> ByteReadWait {
        ByteReadWait {
            channel: self.channel.clone(),
            wait: SignalWait::new(),
        }
    }

    pub fn is_readable(&self) -> bool {
        !self.channel.queue.is_empty()
            || self.channel.writer_closed.load(Ordering::Acquire)
            || self.channel.reader_closed.load(Ordering::Acquire)
    }

    pub fn poll_readable(
        &self,
        cx: &mut core::task::Context<'_>,
        wait: &mut ByteReadWait,
    ) -> core::task::Poll<()> {
        loop {
            if self.is_readable() {
                return core::task::Poll::Ready(());
            }
            match self.channel.readable.poll_notified(cx, &mut wait.wait) {
                core::task::Poll::Ready(()) => continue,
                core::task::Poll::Pending => return core::task::Poll::Pending,
            }
        }
    }

    /// Await the next chunk. Returns `None` when every writer has been
    /// dropped and the queue is drained (EOF).
    pub async fn read(&self) -> Option<Bytes> {
        let mut wait = None;
        loop {
            match self.channel.pop() {
                Some(bytes) => return Some(bytes),
                None => {
                    if self.channel.writer_closed.load(Ordering::Acquire)
                        || self.channel.reader_closed.load(Ordering::Acquire)
                    {
                        // Drain races: a writer might have enqueued right
                        // before the last liveness guard dropped.
                        return self.channel.pop();
                    }
                    let wait = wait.get_or_insert_with(|| self.wait_state());
                    core::future::poll_fn(|cx| {
                        self.channel.readable.poll_notified(cx, &mut wait.wait)
                    })
                    .await;
                }
            }
        }
    }

    /// Non-blocking peek: returns an immediately available chunk or
    /// signals EOF / not-ready.
    pub fn try_read(&self) -> TryRead {
        match self.channel.pop() {
            Some(bytes) => TryRead::Ready(bytes),
            None => {
                if self.channel.writer_closed.load(Ordering::Acquire)
                    || self.channel.reader_closed.load(Ordering::Acquire)
                {
                    TryRead::Eof
                } else {
                    TryRead::Pending
                }
            }
        }
    }

    pub fn poll_read(
        &self,
        cx: &mut core::task::Context<'_>,
        wait: &mut ByteReadWait,
    ) -> core::task::Poll<Option<Bytes>> {
        loop {
            match self.channel.pop() {
                Some(bytes) => return core::task::Poll::Ready(Some(bytes)),
                None => {
                    if self.channel.writer_closed.load(Ordering::Acquire)
                        || self.channel.reader_closed.load(Ordering::Acquire)
                    {
                        return core::task::Poll::Ready(self.channel.pop());
                    }
                    match self.channel.readable.poll_notified(cx, &mut wait.wait) {
                        core::task::Poll::Ready(()) => continue,
                        core::task::Poll::Pending => return core::task::Poll::Pending,
                    }
                }
            }
        }
    }

    pub fn close(&self) {
        self.channel.close_reader();
    }
}

pub enum TryRead {
    Ready(Bytes),
    Pending,
    Eof,
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc as StdArc;
    use alloc::vec::Vec;
    use core::future::Future;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use core::task::{Context, Poll};

    use bytes::Bytes;
    use futures::task::{ArcWake, waker};

    use super::{BYTE_CHANNEL_CHUNK_CAPACITY, TryWrite, byte_channel};

    struct CountingWaker {
        wakes: AtomicUsize,
    }

    impl CountingWaker {
        fn new() -> StdArc<Self> {
            StdArc::new(Self {
                wakes: AtomicUsize::new(0),
            })
        }

        fn count(&self) -> usize {
            self.wakes.load(Ordering::SeqCst)
        }
    }

    impl ArcWake for CountingWaker {
        fn wake_by_ref(arc_self: &StdArc<Self>) {
            arc_self.wakes.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn fill_to_capacity(writer: &super::ByteWriter) {
        for index in 0..BYTE_CHANNEL_CHUNK_CAPACITY {
            assert_eq!(
                writer.try_write(Bytes::copy_from_slice(&(index as u16).to_le_bytes())),
                TryWrite::Written,
                "chunk {index} should fit below capacity"
            );
        }
    }

    #[test]
    fn byte_channel_preserves_bytes_chunks_without_copying() {
        let (writer, reader) = byte_channel();
        let bytes = Bytes::from_static(b"helios");
        let ptr = bytes.as_ptr();

        futures_lite::future::block_on(writer.write(bytes)).expect("reader is still open");
        drop(writer);

        let received = futures_lite::future::block_on(reader.read())
            .expect("queued bytes should be delivered before EOF");
        assert_eq!(received.as_ref(), b"helios");
        assert_eq!(received.as_ptr(), ptr);
    }

    #[test]
    fn byte_channel_queue_is_bounded_to_kernel_capacity() {
        let (writer, _) = byte_channel();

        assert_eq!(writer.channel.queue.capacity(), BYTE_CHANNEL_CHUNK_CAPACITY);
    }

    #[test]
    fn byte_channel_poll_read_uses_reusable_wait_state() {
        let (writer, reader) = byte_channel();
        let mut wait = reader.wait_state();

        futures_lite::future::block_on(writer.write(Bytes::from_static(b"poll")))
            .expect("reader is still open");
        drop(writer);

        let received = futures_lite::future::block_on(futures_lite::future::poll_fn(|cx| {
            reader.poll_read(cx, &mut wait)
        }))
        .expect("queued bytes should be delivered before EOF");

        assert_eq!(received.as_ref(), b"poll");
    }

    #[test]
    fn byte_channel_spills_after_inline_chunks_without_reordering() {
        let (writer, reader) = byte_channel();
        for value in 0..80u8 {
            futures_lite::future::block_on(writer.write(Bytes::copy_from_slice(&[value])))
                .expect("reader is still open");
        }
        drop(writer);

        for value in 0..80u8 {
            let received = futures_lite::future::block_on(reader.read())
                .expect("queued byte should be delivered before EOF");
            assert_eq!(received.as_ref(), &[value]);
        }
        assert!(futures_lite::future::block_on(reader.read()).is_none());
    }

    #[test]
    fn byte_channel_preserves_capacity_after_inline_segment_drains() {
        let (writer, reader) = byte_channel();
        for value in 0..80u16 {
            futures_lite::future::block_on(
                writer.write(Bytes::copy_from_slice(&value.to_le_bytes())),
            )
            .expect("reader is still open");
        }

        for value in 0..64u16 {
            let received = futures_lite::future::block_on(reader.read())
                .expect("queued byte should be delivered before EOF");
            assert_eq!(received.as_ref(), &value.to_le_bytes());
        }

        for value in 80..320u16 {
            futures_lite::future::block_on(
                writer.write(Bytes::copy_from_slice(&value.to_le_bytes())),
            )
            .expect("reader is still open");
        }
        drop(writer);

        for value in 64..320u16 {
            let received = futures_lite::future::block_on(reader.read())
                .expect("queued byte should be delivered before EOF");
            assert_eq!(received.as_ref(), &value.to_le_bytes());
        }
        assert!(futures_lite::future::block_on(reader.read()).is_none());
    }

    #[test]
    fn byte_channel_reports_readability_for_data_and_eof() {
        let (writer, reader) = byte_channel();

        assert!(!reader.is_readable());
        futures_lite::future::block_on(writer.write(Bytes::from_static(b"ready")))
            .expect("reader is still open");
        assert!(reader.is_readable());

        let received = futures_lite::future::block_on(reader.read())
            .expect("queued bytes should be delivered before EOF");
        assert_eq!(received.as_ref(), b"ready");
        assert!(!reader.is_readable());

        drop(writer);
        assert!(reader.is_readable());
        assert!(futures_lite::future::block_on(reader.read()).is_none());
    }

    #[test]
    fn byte_writer_close_reports_eof_after_queued_bytes() {
        let (writer, reader) = byte_channel();

        futures_lite::future::block_on(writer.write(Bytes::from_static(b"before-close")))
            .expect("reader is still open");
        writer.close();
        assert_eq!(
            futures_lite::future::block_on(writer.write(Bytes::from_static(b"after-close"))),
            Err(super::ClosedPeer)
        );

        let received = futures_lite::future::block_on(reader.read())
            .expect("queued bytes should be delivered before EOF");
        assert_eq!(received.as_ref(), b"before-close");
        assert!(futures_lite::future::block_on(reader.read()).is_none());
    }

    #[test]
    fn byte_reader_close_rejects_future_writes() {
        let (writer, reader) = byte_channel();

        reader.close();

        assert!(writer.is_reader_closed());
        assert_eq!(
            futures_lite::future::block_on(writer.write(Bytes::from_static(b"closed"))),
            Err(super::ClosedPeer)
        );
        assert!(reader.is_readable());
        assert!(futures_lite::future::block_on(reader.read()).is_none());
    }

    #[test]
    fn byte_channel_try_write_reports_full_at_capacity_without_losing_bytes() {
        let (writer, reader) = byte_channel();
        fill_to_capacity(&writer);

        assert!(!writer.is_writable());
        let rejected = match writer.try_write(Bytes::from_static(b"overflow")) {
            TryWrite::Full(bytes) => bytes,
            TryWrite::Written => panic!("a full channel must not accept another chunk"),
            TryWrite::Closed => panic!("the reader is still open"),
        };
        assert_eq!(rejected.as_ref(), b"overflow");
        assert_eq!(writer.channel.queue.len(), BYTE_CHANNEL_CHUNK_CAPACITY);

        // One pop makes room, and the same bytes go in untouched.
        futures_lite::future::block_on(reader.read()).expect("queued bytes are available");
        assert!(writer.is_writable());
        assert_eq!(writer.try_write(rejected), TryWrite::Written);
    }

    #[test]
    fn byte_channel_writer_pends_at_capacity_and_resumes_after_a_drain() {
        let (writer, reader) = byte_channel();
        fill_to_capacity(&writer);

        let counter = CountingWaker::new();
        let waker = waker(counter.clone());
        let mut cx = Context::from_waker(&waker);
        let mut wait = writer.wait_state();
        let mut pending = Some(Bytes::from_static(b"parked"));

        assert!(
            writer
                .poll_write(&mut cx, &mut wait, &mut pending)
                .is_pending(),
            "a full channel must park the writer instead of panicking"
        );
        assert_eq!(counter.count(), 0);
        assert!(pending.is_some(), "the parked chunk must not be dropped");

        futures_lite::future::block_on(reader.read()).expect("queued bytes are available");
        assert_eq!(counter.count(), 1, "the drain must wake the parked writer");

        assert_eq!(
            writer.poll_write(&mut cx, &mut wait, &mut pending),
            Poll::Ready(Ok(()))
        );
        assert!(pending.is_none());

        // The parked chunk is delivered in order, after everything queued
        // before it.
        let mut received = Vec::new();
        drop(writer);
        while let Some(bytes) = futures_lite::future::block_on(reader.read()) {
            received.push(bytes);
        }
        assert_eq!(received.len(), BYTE_CHANNEL_CHUNK_CAPACITY);
        assert_eq!(
            received
                .last()
                .expect("the parked chunk is the final delivery")
                .as_ref(),
            b"parked"
        );
    }

    #[test]
    fn byte_channel_write_future_completes_after_the_reader_drains() {
        let (writer, reader) = byte_channel();
        fill_to_capacity(&writer);

        let counter = CountingWaker::new();
        let waker = waker(counter.clone());
        let mut cx = Context::from_waker(&waker);
        let mut write = core::pin::pin!(writer.write(Bytes::from_static(b"awaited")));

        assert!(write.as_mut().poll(&mut cx).is_pending());
        futures_lite::future::block_on(reader.read()).expect("queued bytes are available");
        assert!(counter.count() >= 1);
        assert_eq!(write.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
    }

    #[test]
    fn byte_channel_does_not_lose_a_wakeup_when_a_drain_races_registration() {
        let (writer, reader) = byte_channel();
        fill_to_capacity(&writer);
        let signal = &writer.channel.writable;

        let counter = CountingWaker::new();
        let waker = waker(counter.clone());
        let mut cx = Context::from_waker(&waker);

        // Interleaving A: the drain publishes its permit after the writer
        // decided the queue was full but before the waker is registered.
        // Nobody is parked, so no wake is sent — the permit alone has to
        // carry the writer through, or it parks forever.
        let mut wait = super::SignalWait::new();
        assert!(!signal.try_claim_permit());
        signal.notify_all();
        assert_eq!(
            counter.count(),
            0,
            "no waker is registered yet, so nothing can be woken"
        );
        assert_eq!(signal.poll_notified(&mut cx, &mut wait), Poll::Ready(()));

        // Interleaving B: the writer registers first, so the drain reaches
        // its waker and the following poll observes the permit.
        assert!(signal.poll_notified(&mut cx, &mut wait).is_pending());
        signal.notify_all();
        assert_eq!(counter.count(), 1);
        assert_eq!(signal.poll_notified(&mut cx, &mut wait), Poll::Ready(()));

        // The same handshake end to end, driven by a real drain.
        let mut wait = writer.wait_state();
        let mut pending = Some(Bytes::from_static(b"raced"));
        assert!(
            writer
                .poll_write(&mut cx, &mut wait, &mut pending)
                .is_pending()
        );
        futures_lite::future::block_on(reader.read()).expect("queued bytes are available");
        assert_eq!(counter.count(), 2);
        assert_eq!(
            writer.poll_write(&mut cx, &mut wait, &mut pending),
            Poll::Ready(Ok(())),
            "the permit published by the drain must be observed"
        );
        assert!(pending.is_none());
    }

    #[test]
    fn byte_channel_wakes_every_parked_writer() {
        let (writer, reader) = byte_channel();
        let second = writer.clone();
        fill_to_capacity(&writer);

        let first_counter = CountingWaker::new();
        let first_waker = waker(first_counter.clone());
        let mut first_cx = Context::from_waker(&first_waker);
        let mut first_wait = writer.wait_state();
        let mut first_pending = Some(Bytes::from_static(b"first"));

        let second_counter = CountingWaker::new();
        let second_waker = waker(second_counter.clone());
        let mut second_cx = Context::from_waker(&second_waker);
        let mut second_wait = second.wait_state();
        let mut second_pending = Some(Bytes::from_static(b"second"));

        assert!(
            writer
                .poll_write(&mut first_cx, &mut first_wait, &mut first_pending)
                .is_pending()
        );
        assert!(
            second
                .poll_write(&mut second_cx, &mut second_wait, &mut second_pending)
                .is_pending()
        );

        futures_lite::future::block_on(reader.read()).expect("queued bytes are available");

        assert_eq!(first_counter.count(), 1);
        assert_eq!(second_counter.count(), 1);
    }

    #[test]
    fn byte_channel_parked_writer_observes_a_closing_reader() {
        let (writer, reader) = byte_channel();
        fill_to_capacity(&writer);

        let counter = CountingWaker::new();
        let waker = waker(counter.clone());
        let mut cx = Context::from_waker(&waker);
        let mut wait = writer.wait_state();
        let mut pending = Some(Bytes::from_static(b"orphaned"));

        assert!(
            writer
                .poll_write(&mut cx, &mut wait, &mut pending)
                .is_pending()
        );

        reader.close();

        assert_eq!(counter.count(), 1, "closing must wake parked writers");
        assert_eq!(
            writer.poll_write(&mut cx, &mut wait, &mut pending),
            Poll::Ready(Err(super::ClosedPeer))
        );
    }

    #[test]
    fn byte_channel_parked_writer_observes_a_dropped_reader() {
        let (writer, reader) = byte_channel();
        fill_to_capacity(&writer);

        let counter = CountingWaker::new();
        let waker = waker(counter.clone());
        let mut cx = Context::from_waker(&waker);
        let mut wait = writer.wait_state();
        let mut pending = Some(Bytes::from_static(b"orphaned"));

        assert!(
            writer
                .poll_write(&mut cx, &mut wait, &mut pending)
                .is_pending()
        );

        drop(reader);

        assert_eq!(counter.count(), 1);
        assert_eq!(
            writer.poll_write(&mut cx, &mut wait, &mut pending),
            Poll::Ready(Err(super::ClosedPeer))
        );
        assert!(
            writer.is_writable(),
            "a closed channel never parks a writer"
        );
    }

    #[test]
    fn byte_channel_poll_writable_tracks_queue_occupancy() {
        let (writer, reader) = byte_channel();
        let mut wait = writer.wait_state();
        let counter = CountingWaker::new();
        let waker = waker(counter.clone());
        let mut cx = Context::from_waker(&waker);

        assert_eq!(writer.poll_writable(&mut cx, &mut wait), Poll::Ready(()));
        fill_to_capacity(&writer);
        assert!(writer.poll_writable(&mut cx, &mut wait).is_pending());

        futures_lite::future::block_on(reader.read()).expect("queued bytes are available");
        assert_eq!(writer.poll_writable(&mut cx, &mut wait), Poll::Ready(()));
    }

    #[test]
    fn byte_channel_write_survives_more_parked_waiters_than_inline_capacity() {
        let (writer, reader) = byte_channel();
        fill_to_capacity(&writer);

        let waiters = super::BYTE_CHANNEL_WAKER_CAPACITY + 4;
        let mut counters = Vec::new();
        let mut wakers = Vec::new();
        let mut waits = Vec::new();
        let mut pendings = Vec::new();
        for _ in 0..waiters {
            let counter = CountingWaker::new();
            wakers.push(waker(counter.clone()));
            counters.push(counter);
            waits.push(writer.wait_state());
            pendings.push(Some(Bytes::from_static(b"crowd")));
        }

        for index in 0..waiters {
            let mut cx = Context::from_waker(&wakers[index]);
            assert!(
                writer
                    .poll_write(&mut cx, &mut waits[index], &mut pendings[index])
                    .is_pending(),
                "waiter {index} should park on the full channel"
            );
        }

        futures_lite::future::block_on(reader.read()).expect("queued bytes are available");

        for (index, counter) in counters.iter().enumerate() {
            assert_eq!(counter.count(), 1, "waiter {index} should have been woken");
        }
    }
}
