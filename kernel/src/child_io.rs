//! Async byte-stream channels used to connect a spawned child component's
//! WASI stdio to the parent that spawned it.
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

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use concurrent_queue::{ConcurrentQueue, PopError, PushError};

use crate::Notify;

/// A single byte-stream channel, closable from both ends. Producers push
/// `Vec<u8>` chunks; consumers await and receive the same chunks back.
struct ByteChannel {
    queue: ConcurrentQueue<Vec<u8>>,
    /// Notifies consumers when new bytes or a close event are available.
    readable: Notify,
    /// Set to `true` once every `ByteWriter` cloneable handle has been
    /// dropped, signalling EOF to the reader.
    writer_closed: AtomicBool,
    /// Set to `true` when the reader has gone away, so writers can stop
    /// producing bytes.
    reader_closed: AtomicBool,
}

impl ByteChannel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            queue: ConcurrentQueue::unbounded(),
            readable: Notify::new(),
            writer_closed: AtomicBool::new(false),
            reader_closed: AtomicBool::new(false),
        })
    }
}

/// Writer liveness guard — when the last outstanding guard is dropped,
/// the channel is marked closed so the reader observes EOF. Cloneable
/// writers share one `Arc<WriterLiveness>`, so they all count as "the
/// same writer" for close purposes.
struct WriterLiveness {
    channel: Arc<ByteChannel>,
}

impl Drop for WriterLiveness {
    fn drop(&mut self) {
        self.channel.writer_closed.store(true, Ordering::Release);
        self.channel.readable.notify_one();
    }
}

/// Producer half of a byte stream. Writers can be cloned — the channel
/// stays open as long as any cloned writer is alive; it closes once they
/// are all dropped.
#[derive(Clone)]
pub struct ByteWriter {
    channel: Arc<ByteChannel>,
    _liveness: Arc<WriterLiveness>,
}

/// Consumer half of a byte stream. Dropping the reader causes subsequent
/// writer operations to observe a closed reader and drop their payload.
pub struct ByteReader {
    channel: Arc<ByteChannel>,
}

pub fn byte_channel() -> (ByteWriter, ByteReader) {
    let channel = ByteChannel::new();
    (
        ByteWriter {
            channel: channel.clone(),
            _liveness: Arc::new(WriterLiveness {
                channel: channel.clone(),
            }),
        },
        ByteReader { channel },
    )
}

/// Indicates that the peer has gone away. Writers see this when the reader
/// was dropped; readers see EOF as `Option::None` instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClosedPeer;

impl ByteWriter {
    /// Push a chunk of bytes to the consumer. Succeeds as long as the
    /// reader half is alive. Never blocks.
    pub fn write(&self, bytes: Vec<u8>) -> Result<(), ClosedPeer> {
        if self.channel.reader_closed.load(Ordering::Acquire) {
            return Err(ClosedPeer);
        }
        if bytes.is_empty() {
            return Ok(());
        }
        match self.channel.queue.push(bytes) {
            Ok(()) => {
                self.channel.readable.notify_one();
                Ok(())
            }
            Err(PushError::Closed(_)) => Err(ClosedPeer),
            Err(PushError::Full(_)) => {
                unreachable!("byte channel queue is unbounded and cannot report full")
            }
        }
    }

    /// Returns true once the reader has been dropped.
    pub fn is_reader_closed(&self) -> bool {
        self.channel.reader_closed.load(Ordering::Acquire)
    }
}

impl ByteReader {
    /// Await the next chunk. Returns `None` when every writer has been
    /// dropped and the queue is drained (EOF).
    pub async fn read(&self) -> Option<Vec<u8>> {
        loop {
            match self.channel.queue.pop() {
                Ok(bytes) => return Some(bytes),
                Err(PopError::Closed) => return None,
                Err(PopError::Empty) => {
                    if self.channel.writer_closed.load(Ordering::Acquire) {
                        // Drain races: a writer might have enqueued right
                        // before the last liveness guard dropped.
                        return self.channel.queue.pop().ok();
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
            Ok(bytes) => TryRead::Ready(bytes),
            Err(PopError::Closed) => TryRead::Eof,
            Err(PopError::Empty) => {
                if self.channel.writer_closed.load(Ordering::Acquire) {
                    TryRead::Eof
                } else {
                    TryRead::Pending
                }
            }
        }
    }
}

impl Drop for ByteReader {
    fn drop(&mut self) {
        self.channel.reader_closed.store(true, Ordering::Release);
    }
}

pub enum TryRead {
    Ready(Vec<u8>),
    Pending,
    Eof,
}
