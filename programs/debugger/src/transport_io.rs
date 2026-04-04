use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_io::{AsyncRead, AsyncWrite};
use helios_api::serial::DebugPort;

type BoxFuture<T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send>>;

pub struct DebugPortReader {
    port: Arc<DebugPort>,
    buffer: Vec<u8>,
    offset: usize,
    closed: bool,
    pending: Option<BoxFuture<Vec<u8>>>,
}

pub struct DebugPortWriter {
    port: Arc<DebugPort>,
    write: Option<(usize, BoxFuture<()>)>,
    flush: Option<BoxFuture<()>>,
}

pub fn split(
    read_port: Arc<DebugPort>,
    write_port: Arc<DebugPort>,
) -> (DebugPortReader, DebugPortWriter) {
    (
        DebugPortReader {
            port: read_port,
            buffer: Vec::new(),
            offset: 0,
            closed: false,
            pending: None,
        },
        DebugPortWriter {
            port: write_port,
            write: None,
            flush: None,
        },
    )
}

impl AsyncRead for DebugPortReader {
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
            let port = Arc::clone(&this.port);
            this.pending = Some(Box::pin(async move { port.read(4096).await }));
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

impl AsyncWrite for DebugPortWriter {
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
            let port = Arc::clone(&this.port);
            let bytes = Vec::from(buf);
            let len = bytes.len();
            this.write = Some((len, Box::pin(async move { port.write_all(&bytes).await })));
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
            let port = Arc::clone(&this.port);
            this.flush = Some(Box::pin(async move { port.flush().await }));
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
