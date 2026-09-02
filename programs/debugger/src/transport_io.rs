//! `futures-io` adapters over the byte transports the debugger serves
//! RPC on.
//!
//! Both transports — the debug serial port and a vsock connection —
//! present the same three async operations, so the poll state machine
//! that turns them into `AsyncRead`/`AsyncWrite` is written once over
//! [`ByteEndpoint`] rather than once per transport.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_io::{AsyncRead, AsyncWrite};
use helios_api::serial::DebugPort;
use helios_api::vsock::VsockStream;

type BoxFuture<T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send>>;

/// A byte transport the RPC framing can run over.
///
/// The receiver is an `Arc` so a future can own its endpoint for as long
/// as it is in flight, which is what lets the reader and the writer hold
/// the same transport at once.
pub trait ByteEndpoint: Send + Sync + 'static {
    /// Reads whatever is available, up to `max_bytes`. An empty vector
    /// means the transport is closed.
    fn read(self: &Arc<Self>, max_bytes: usize) -> BoxFuture<Vec<u8>>;

    fn write_all(self: &Arc<Self>, bytes: Vec<u8>) -> BoxFuture<()>;

    fn flush(self: &Arc<Self>) -> BoxFuture<()>;
}

/// Bytes the reader asks its transport for in one call.
const READ_CHUNK_BYTES: usize = 4096;

impl ByteEndpoint for DebugPort {
    fn read(self: &Arc<Self>, max_bytes: usize) -> BoxFuture<Vec<u8>> {
        let port = Arc::clone(self);
        // The receiver is narrowed to `&Self` so these resolve to the
        // transport's own inherent methods rather than recursing back
        // into this trait, whose receiver is the `Arc`.
        Box::pin(async move { port.as_ref().read(max_bytes).await })
    }

    fn write_all(self: &Arc<Self>, bytes: Vec<u8>) -> BoxFuture<()> {
        let port = Arc::clone(self);
        Box::pin(async move { port.as_ref().write_all(&bytes).await })
    }

    fn flush(self: &Arc<Self>) -> BoxFuture<()> {
        let port = Arc::clone(self);
        Box::pin(async move { port.as_ref().flush().await })
    }
}

impl ByteEndpoint for VsockStream {
    fn read(self: &Arc<Self>, max_bytes: usize) -> BoxFuture<Vec<u8>> {
        let stream = Arc::clone(self);
        // An orderly end of file is an empty read, which is what the
        // poll adapter treats as "closed".
        Box::pin(async move { Ok(stream.as_ref().read(max_bytes).await?.unwrap_or_default()) })
    }

    fn write_all(self: &Arc<Self>, bytes: Vec<u8>) -> BoxFuture<()> {
        let stream = Arc::clone(self);
        Box::pin(async move { stream.as_ref().write_all(&bytes).await })
    }

    fn flush(self: &Arc<Self>) -> BoxFuture<()> {
        // A vsock write has reached the device by the time it returns;
        // there is no buffer between here and the peer to push.
        let _ = self;
        Box::pin(async move { Ok(()) })
    }
}

pub struct EndpointReader<Endpoint: ByteEndpoint> {
    endpoint: Arc<Endpoint>,
    buffer: Vec<u8>,
    offset: usize,
    closed: bool,
    pending: Option<BoxFuture<Vec<u8>>>,
}

pub struct EndpointWriter<Endpoint: ByteEndpoint> {
    endpoint: Arc<Endpoint>,
    write: Option<(usize, BoxFuture<()>)>,
    flush: Option<BoxFuture<()>>,
}

/// Splits one transport into the reader and writer halves the RPC
/// framing takes.
pub fn split<Endpoint: ByteEndpoint>(
    read_endpoint: Arc<Endpoint>,
    write_endpoint: Arc<Endpoint>,
) -> (EndpointReader<Endpoint>, EndpointWriter<Endpoint>) {
    (
        EndpointReader {
            endpoint: read_endpoint,
            buffer: Vec::new(),
            offset: 0,
            closed: false,
            pending: None,
        },
        EndpointWriter {
            endpoint: write_endpoint,
            write: None,
            flush: None,
        },
    )
}

impl<Endpoint: ByteEndpoint> AsyncRead for EndpointReader<Endpoint> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        if this.offset < this.buffer.len() {
            let available = this.buffer.len() - this.offset;
            let count = available.min(buf.len());
            buf[..count].copy_from_slice(&this.buffer[this.offset..this.offset + count]);
            this.offset += count;
            if this.offset == this.buffer.len() {
                this.buffer.clear();
                this.offset = 0;
            }
            return Poll::Ready(Ok(count));
        }

        if this.closed {
            return Poll::Ready(Ok(0));
        }

        if this.pending.is_none() {
            this.pending = Some(this.endpoint.read(READ_CHUNK_BYTES));
        }

        match this
            .pending
            .as_mut()
            .expect("reader future must exist")
            .as_mut()
            .poll(cx)
        {
            Poll::Ready(Ok(bytes)) => {
                this.pending = None;
                if bytes.is_empty() {
                    this.closed = true;
                    Poll::Ready(Ok(0))
                } else {
                    this.buffer = bytes;
                    this.offset = 0;
                    Pin::new(this).poll_read(cx, buf)
                }
            }
            Poll::Ready(Err(error)) => {
                this.pending = None;
                this.closed = true;
                Poll::Ready(Err(error))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<Endpoint: ByteEndpoint> AsyncWrite for EndpointWriter<Endpoint> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        if this.write.is_none() {
            let bytes = Vec::from(buf);
            let len = bytes.len();
            this.write = Some((len, this.endpoint.write_all(bytes)));
        }

        let (len, future) = this.write.as_mut().expect("writer future must exist");
        match future.as_mut().poll(cx) {
            Poll::Ready(Ok(())) => {
                let len = *len;
                this.write = None;
                Poll::Ready(Ok(len))
            }
            Poll::Ready(Err(error)) => {
                this.write = None;
                Poll::Ready(Err(error))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        if let Some((_, future)) = this.write.as_mut() {
            match future.as_mut().poll(cx) {
                Poll::Ready(Ok(())) => this.write = None,
                Poll::Ready(Err(error)) => {
                    this.write = None;
                    return Poll::Ready(Err(error));
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        if this.flush.is_none() {
            this.flush = Some(this.endpoint.flush());
        }

        match this
            .flush
            .as_mut()
            .expect("flush future must exist")
            .as_mut()
            .poll(cx)
        {
            Poll::Ready(Ok(())) => {
                this.flush = None;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => {
                this.flush = None;
                Poll::Ready(Err(error))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}
