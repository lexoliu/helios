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

extern crate alloc;

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use bytes::Bytes;
use heapless::Deque;
use spin::Mutex;
use triomphe::Arc;

use crate::{Notify, NotifyWaiter};

const BYTE_CHANNEL_CHUNK_CAPACITY: usize = 256;

/// A single byte-stream channel, closable from both ends. Producers push
/// reference-counted byte chunks; consumers await and receive the same chunks
/// back without forcing an extra copy at adapter boundaries.
struct ByteChannel {
    queue: ByteQueue,
    /// Notifies consumers when new bytes or a close event are available.
    readable: Notify,
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
            readable: Notify::new(),
            writer_closed: AtomicBool::new(false),
            reader_closed: AtomicBool::new(false),
            writer_handles: AtomicUsize::new(1),
            reader_handles: AtomicUsize::new(1),
        })
    }

    fn push(&self, bytes: Bytes) -> Result<(), ClosedPeer> {
        self.queue
            .push_if_open(bytes, &self.reader_closed, &self.writer_closed)
    }

    fn close_reader(&self) {
        self.queue.close_reader(&self.reader_closed);
        self.readable.notify_one();
    }
}

struct ByteQueue {
    chunks: Mutex<Deque<Bytes, BYTE_CHANNEL_CHUNK_CAPACITY>>,
}

impl ByteQueue {
    const fn new() -> Self {
        Self {
            chunks: Mutex::new(Deque::new()),
        }
    }

    fn push_if_open(
        &self,
        bytes: Bytes,
        reader_closed: &AtomicBool,
        writer_closed: &AtomicBool,
    ) -> Result<(), ClosedPeer> {
        let mut chunks = self.chunks.lock();
        if reader_closed.load(Ordering::Acquire) || writer_closed.load(Ordering::Acquire) {
            return Err(ClosedPeer);
        }
        chunks.push_back(bytes).unwrap_or_else(|_| {
            panic!("byte channel capacity {BYTE_CHANNEL_CHUNK_CAPACITY} exhausted")
        });
        Ok(())
    }

    fn pop(&self) -> Option<Bytes> {
        self.chunks.lock().pop_front()
    }

    fn is_empty(&self) -> bool {
        self.chunks.lock().is_empty()
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
    readable: NotifyWaiter,
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

impl Clone for ByteWriter {
    fn clone(&self) -> Self {
        let previous = self.channel.writer_handles.fetch_add(1, Ordering::AcqRel);
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
        let previous = self.channel.writer_handles.fetch_sub(1, Ordering::AcqRel);
        assert!(previous != 0, "byte writer handle count underflowed");
        if previous == 1 {
            self.channel.writer_closed.store(true, Ordering::Release);
            self.channel.readable.notify_one();
        }
    }
}

impl Clone for ByteReader {
    fn clone(&self) -> Self {
        let previous = self.channel.reader_handles.fetch_add(1, Ordering::AcqRel);
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
        let previous = self.channel.reader_handles.fetch_sub(1, Ordering::AcqRel);
        assert!(previous != 0, "byte reader handle count underflowed");
        if previous == 1 {
            self.channel.close_reader();
        }
    }
}

impl ByteWriter {
    /// Push a chunk of bytes to the consumer. Succeeds as long as the
    /// reader half is alive. Never blocks.
    pub fn write(&self, bytes: impl Into<Bytes>) -> Result<(), ClosedPeer> {
        if self.channel.reader_closed.load(Ordering::Acquire)
            || self.channel.writer_closed.load(Ordering::Acquire)
        {
            return Err(ClosedPeer);
        }
        let bytes = bytes.into();
        if bytes.is_empty() {
            return Ok(());
        }
        match self.channel.push(bytes) {
            Ok(()) => {
                self.channel.readable.notify_one();
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Returns true once the reader has been dropped.
    pub fn is_reader_closed(&self) -> bool {
        self.channel.reader_closed.load(Ordering::Acquire)
    }

    pub fn close(&self) {
        self.channel.writer_closed.store(true, Ordering::Release);
        self.channel.readable.notify_one();
    }
}

impl ByteReader {
    pub fn wait_state(&self) -> ByteReadWait {
        ByteReadWait {
            readable: self.channel.readable.waiter(),
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
            match self.channel.readable.poll_notified(cx, &mut wait.readable) {
                core::task::Poll::Ready(()) => continue,
                core::task::Poll::Pending => return core::task::Poll::Pending,
            }
        }
    }

    /// Await the next chunk. Returns `None` when every writer has been
    /// dropped and the queue is drained (EOF).
    pub async fn read(&self) -> Option<Bytes> {
        loop {
            match self.channel.queue.pop() {
                Some(bytes) => return Some(bytes),
                None => {
                    if self.channel.writer_closed.load(Ordering::Acquire)
                        || self.channel.reader_closed.load(Ordering::Acquire)
                    {
                        // Drain races: a writer might have enqueued right
                        // before the last liveness guard dropped.
                        return self.channel.queue.pop();
                    }
                    self.channel.readable.notified().await;
                }
            }
        }
    }

    /// Non-blocking peek: returns an immediately available chunk or
    /// signals EOF / not-ready.
    pub fn try_read(&self) -> TryRead {
        match self.channel.queue.pop() {
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
            match self.channel.queue.pop() {
                Some(bytes) => return core::task::Poll::Ready(Some(bytes)),
                None => {
                    if self.channel.writer_closed.load(Ordering::Acquire)
                        || self.channel.reader_closed.load(Ordering::Acquire)
                    {
                        return core::task::Poll::Ready(self.channel.queue.pop());
                    }
                    match self.channel.readable.poll_notified(cx, &mut wait.readable) {
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
    use bytes::Bytes;

    use super::{BYTE_CHANNEL_CHUNK_CAPACITY, byte_channel};

    #[test]
    fn byte_channel_preserves_bytes_chunks_without_copying() {
        let (writer, reader) = byte_channel();
        let bytes = Bytes::from_static(b"helios");
        let ptr = bytes.as_ptr();

        writer.write(bytes).expect("reader is still open");
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

        writer
            .write(Bytes::from_static(b"poll"))
            .expect("reader is still open");
        drop(writer);

        let received = futures_lite::future::block_on(futures_lite::future::poll_fn(|cx| {
            reader.poll_read(cx, &mut wait)
        }))
        .expect("queued bytes should be delivered before EOF");

        assert_eq!(received.as_ref(), b"poll");
    }

    #[test]
    fn byte_channel_reports_readability_for_data_and_eof() {
        let (writer, reader) = byte_channel();

        assert!(!reader.is_readable());
        writer
            .write(Bytes::from_static(b"ready"))
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

        writer
            .write(Bytes::from_static(b"before-close"))
            .expect("reader is still open");
        writer.close();
        assert_eq!(
            writer.write(Bytes::from_static(b"after-close")),
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
            writer.write(Bytes::from_static(b"closed")),
            Err(super::ClosedPeer)
        );
        assert!(reader.is_readable());
        assert!(futures_lite::future::block_on(reader.read()).is_none());
    }
}
